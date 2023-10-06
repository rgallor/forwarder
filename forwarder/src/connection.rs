use std::fmt::Display;
use std::ops::{Deref, DerefMut};

use base64::Engine as _;
use displaydoc::Display;
use futures_util::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::sync::mpsc::error::SendError;
use tokio::task::JoinHandle;
use tokio::{
    select,
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
};
use tracing::{debug, error, info, instrument, warn};
use tungstenite::{Error as TungError, Message as TungMessage};

use crate::connections::WsStream;
use crate::proto_message::Protocol as ProtoProtocol;
use crate::proto_message::{
    ProtoError, ProtoMessage, WebSocket as ProtoWebSocket, WebSocketMessage,
};

#[non_exhaustive]
#[derive(Display, Error, Debug)]
pub enum ConnectionError {
    /// Error when sending message over websocket connection, `{0}`.
    WebSocketSend(#[from] TungError),
    /// Error when receiving message over websocket connection, `{0}`.
    WebSocketNext(#[source] TungError),
    /// Protobuf error.
    Protobuf(#[from] ProtoError),
    /// Channel error
    ChannelToWs(#[from] SendError<ProtoMessage>),
    /// Channel error
    ChannelToTtyd(#[from] SendError<WebSocketMessage>),
    /// Reqwest error, `{0}`.
    Reqwest(#[from] reqwest::Error),
    /// Failed to re-establish websocket connection, `{0}`
    Reconnect(#[source] TungError),
}

#[derive(Debug)]
pub struct ConnectionHandle {
    handle: JoinHandle<Result<(), ConnectionError>>,
    tx_con: UnboundedSender<WebSocketMessage>,
}

impl Deref for ConnectionHandle {
    type Target = JoinHandle<Result<(), ConnectionError>>;

    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

impl DerefMut for ConnectionHandle {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.handle
    }
}

impl ConnectionHandle {
    /// Forward a [`protobuf message`](crate::messages::ProtoMessage) to the connection responsible
    /// for its management.
    pub async fn send_channel(&mut self, ws_msg: WebSocketMessage) -> Result<(), ConnectionError> {
        self.tx_con.send(ws_msg).map_err(|err| err.into())
    }

    /// Gracefully close the task responsible for the connection handling.
    pub async fn close(mut self, ws_msg: WebSocketMessage) -> Result<(), ConnectionError> {
        // send the close frame to the connection
        self.send_channel(ws_msg).await?;

        let Self { handle, tx_con } = self;

        // by dropping the Sending end of the channel, when the connection will attempt to read (from the Receiving end)
        // it will receive None, stating that the
        drop(tx_con);

        handle.await.expect("failed to await tokio task")
    }
}

#[derive(Debug, Default)]
enum ConnState {
    #[default]
    ReadWrite,
    Read,
    Write,
    Closed,
}

impl Display for ConnState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnState::ReadWrite => write!(f, "ReadWrite"),
            ConnState::Read => write!(f, "Read"),
            ConnState::Write => write!(f, "Write"),
            ConnState::Closed => write!(f, "Closed"),
        }
    }
}

impl ConnState {
    fn shutdown_read(&mut self) {
        match self {
            ConnState::ReadWrite => {
                let _ = std::mem::replace(self, ConnState::Write);
            }
            ConnState::Read => {
                let _ = std::mem::replace(self, ConnState::Closed);
            }
            ConnState::Write | ConnState::Closed => {}
        }
    }

    fn shutdown_write(&mut self) {
        match self {
            ConnState::ReadWrite => {
                let _ = std::mem::replace(self, ConnState::Read);
            }
            ConnState::Write => {
                let _ = std::mem::replace(self, ConnState::Closed);
            }
            ConnState::Read | ConnState::Closed => {}
        }
    }
}

enum Either {
    Read(TungMessage),
    Write(Option<WebSocketMessage>),
}

#[derive(Debug)]
pub struct Connection {
    id: Vec<u8>,
    // TODO: Connection becomes generic on T, where T implements a trait (that is a stream of ProtoMessage)
    stream: WsStream,
    state: ConnState,
    tx_ws: UnboundedSender<ProtoMessage>,
    rx_con: UnboundedReceiver<WebSocketMessage>,
}

impl Connection {
    pub fn new(
        id: Vec<u8>,
        stream: WsStream,
        tx_ws: UnboundedSender<ProtoMessage>,
        rx_con: UnboundedReceiver<WebSocketMessage>,
    ) -> Self {
        Self {
            id,
            stream,
            state: ConnState::default(),
            tx_ws,
            rx_con,
        }
    }

    pub fn spawn(self, tx_con: UnboundedSender<WebSocketMessage>) -> ConnectionHandle {
        // spawn a task responsible for notifying when new data is available on the TCP reader
        let handle = tokio::spawn(self.task());
        ConnectionHandle { handle, tx_con }
    }

    async fn task(mut self) -> Result<(), ConnectionError> {
        self.task_loop().await.map_err(|err| {
            error!("error while reading/writing, {err}");

            // send Close frame to edgehog
            let msg = ProtoMessage::new(ProtoProtocol::WebSocket(ProtoWebSocket::close(
                self.id, None,
            )));

            self.tx_ws
                .send(msg)
                .expect("failed to send over tx_ws channel");

            err
        })
    }

    #[instrument(skip_all)]
    async fn task_loop(&mut self) -> Result<(), ConnectionError> {
        // select return None if the connection state is Closed
        loop {
            let res = self.select().await;

            match res {
                Some(Ok(r_w)) => match r_w {
                    Either::Read(data) => self.handle_ws_read(data)?,
                    Either::Write(opt) => self.handle_ws_write(opt).await?,
                },
                // TODO: check if closing the connection or only reporting the error
                // if there is an error such TungError::ConnectionClosed or AlreadyClosed we should call shutdown_read()
                Some(Err(err)) => error!(?err),
                None => {
                    info!("closing connection...");
                    return Ok(());
                }
            }
        }
    }

    async fn select(&mut self) -> Option<Result<Either, ConnectionError>> {
        match self.state {
            ConnState::ReadWrite => select! {
                res = Connection::stream_next(&mut self.stream) => res,
                opt = Connection::channel_recv(&mut self.rx_con) => opt,
            },
            ConnState::Read => Connection::stream_next(&mut self.stream).await,
            ConnState::Write => Connection::channel_recv(&mut self.rx_con).await,
            ConnState::Closed => None,
        }
    }

    async fn stream_next(stream: &mut WsStream) -> Option<Result<Either, ConnectionError>> {
        match stream.next().await {
            Some(Ok(tung_msg)) => Some(Ok(Either::Read(tung_msg))),
            Some(Err(err)) => Some(Err(ConnectionError::WebSocketNext(err))),
            None => {
                warn!("next returned None, websocket stream closed");
                // return None so that the main task_loop ends.
                None
            }
        }
    }

    async fn channel_recv(
        rx_con: &mut UnboundedReceiver<WebSocketMessage>,
    ) -> Option<Result<Either, ConnectionError>> {
        Some(Ok(Either::Write(rx_con.recv().await)))
    }

    /// Read from TTYD-device websocket.
    ///
    /// This method is called only when the `ConnState` is `ConnState::ReadWrite` and `ConnState::Read`
    #[instrument(skip(self), fields(id = base64::engine::general_purpose::STANDARD.encode(& self.id), state = % self.state))]
    fn handle_ws_read(&mut self, data: TungMessage) -> Result<(), ConnectionError> {
        if data.is_close() {
            info!("received close frame from TTYD, changing read state");
            self.state.shutdown_read();
        }

        let msg = ProtoMessage::try_from(data)?;

        self.tx_ws.send(msg).map_err(|err| err.into())
    }

    /// Write to TTYD-device websocket.
    ///
    /// This method is called only when the `ConnState` is `ConnState::ReadWrite` and `ConnState::Write`
    #[instrument(skip_all, fields(id = base64::engine::general_purpose::STANDARD.encode(& self.id), state = % self.state))]
    async fn handle_ws_write(
        &mut self,
        data: Option<WebSocketMessage>,
    ) -> Result<(), ConnectionError> {
        let data = match data {
            None => {
                error!("rx_con channel dropped, changing write state to avoid receiving other data from the channel");

                self.state.shutdown_write();
                debug!("changed write state after channel closure");

                // return after changing the state. in this way it will not be possible to write data anymore
                return Ok(());
            }
            Some(msg) => msg,
        };

        let tung_msg = data.into();
        self.stream.send(tung_msg).await.map_err(|err| err.into())
    }
}

#[cfg(test)]
mod tests {
    use core::panic;

    use super::*;

    // create a test connection
    //
    // remember to start ttyd on port 7681
    async fn create_conn(
        tx_ws: UnboundedSender<ProtoMessage>,
        rx_con: UnboundedReceiver<WebSocketMessage>,
    ) -> Connection {
        let id = String::from("id1").as_bytes().to_vec();
        let (stream, _) = tokio_tungstenite::connect_async("ws://localhost:7681")
            .await
            .expect("failed to connct to ttyd through ws");

        Connection::new(id, stream, tx_ws, rx_con)
    }

    #[tokio::test]
    async fn test_handle_ws_write() -> Result<(), ConnectionError> {
        let (tx_ws, _rx_ws) = tokio::sync::mpsc::unbounded_channel::<ProtoMessage>();
        let (_tx_con, rx_con) = tokio::sync::mpsc::unbounded_channel::<WebSocketMessage>();

        let mut con = create_conn(tx_ws, rx_con).await;

        con.handle_ws_write(None).await?;
        assert!(matches!(con.state, ConnState::Read));

        con.stream.close(None).await?;
        if let Ok(_) = con
            .handle_ws_write(Some(WebSocketMessage::Binary(b"test".to_vec())))
            .await
        {
            panic!("should have returned error because the websocket stream has been closed");
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_handle_ws_read() -> Result<(), ConnectionError> {
        let (tx_ws, mut rx_ws) = tokio::sync::mpsc::unbounded_channel::<ProtoMessage>();
        let (_tx_con, rx_con) = tokio::sync::mpsc::unbounded_channel::<WebSocketMessage>();

        let mut con = create_conn(tx_ws, rx_con).await;

        let tung_msg = TungMessage::Binary(b"test".to_vec());
        match con.handle_ws_read(tung_msg) {
            Ok(()) => {
                let received = rx_ws.recv().await.expect("channel dropped");

                match received.protocol() {
                    ProtoProtocol::WebSocket(ws) => {
                        if let (_, WebSocketMessage::Binary(msg)) = ws.into_inner() {
                            assert_eq!(msg, b"test".to_vec());
                        } else {
                            panic!("wrong websocket message");
                        }
                    }
                    proto => panic!("wrong protocol message sent, {proto:?}"),
                }
            }
            Err(err) => return Err(err),
        }

        let tung_msg = TungMessage::Close(None);
        match con.handle_ws_read(tung_msg) {
            Ok(()) => {
                let received = rx_ws.recv().await.expect("channel dropped");

                match received.protocol() {
                    ProtoProtocol::WebSocket(ws) => {
                        if let (_, WebSocketMessage::Close { code, reason }) = ws.into_inner() {
                            assert_eq!(code, 1000);
                            assert_eq!(reason, None);
                        } else {
                            panic!("wrong websocket message");
                        }
                    }
                    proto => panic!("wrong protocol message sent, {proto:?}"),
                }
            }
            Err(err) => return Err(err),
        }

        assert!(matches!(con.state, ConnState::Write));

        // check for channel error handling
        rx_ws.close();
        let tung_msg = TungMessage::Close(None);
        match con.handle_ws_read(tung_msg) {
            Ok(_) => panic!("having closed rx_ws, the handle_ws_read should return error"),
            Err(ConnectionError::ChannelToWs(_err)) => {}
            Err(err) => panic!("expected another kind of error, not {err:?}"),
        }

        Ok(())
    }
}
