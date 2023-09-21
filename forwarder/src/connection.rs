use std::fmt::Display;
use std::ops::{Deref, DerefMut};

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
    /// Error when sending message over websockwet connection, `{0}`.
    WebSocketSend(#[from] TungError),
    /// Error when receiving message over websockwet connection, `{0}`.
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
    pub async fn send_channel(&mut self, ws_msg: WebSocketMessage) -> Result<(), ConnectionError> {
        self.tx_con.send(ws_msg).map_err(|err| err.into())
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

    fn can_write(&self) -> bool {
        matches!(self, ConnState::ReadWrite | ConnState::Write)
    }
}

enum Either {
    Read(TungMessage),
    Write(Option<WebSocketMessage>),
}

#[derive(Debug)]
pub struct Connection {
    id: Vec<u8>,
    stream: WsStream,
    state: ConnState,
    tx_ws: UnboundedSender<ProtoMessage>,
    rx_con: UnboundedReceiver<WebSocketMessage>,
}

impl Connection {
    pub fn new(
        id: &[u8],
        stream: WsStream,
        tx_ws: UnboundedSender<ProtoMessage>,
        rx_con: UnboundedReceiver<WebSocketMessage>,
    ) -> Self {
        Self {
            id: id.to_vec(),
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
                Some(Err(err)) => error!(?err), // TODO: check if closing the connection or only reporting the error
                None => {
                    info!("connection state Closed, exiting...");
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

    /// Read from websocket.
    #[instrument(skip(self), fields(state = %self.state))]
    fn handle_ws_read(&mut self, data: TungMessage) -> Result<(), ConnectionError> {
        // TODO: trovare un modo per gestire lo shutdown della Read.
        // forse non c'e bisogno perche ttyd inviera close, che viene forwardato.

        if data.is_close() {
            info!("received close frame from TTYD, changing read state");
            self.state.shutdown_read();
        }

        let msg = ProtoMessage::try_from(data)?;

        self.tx_ws.send(msg).map_err(|err| err.into())
    }

    /// Write to websocket.
    #[instrument(skip_all, fields(state = %self.state))]
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

        // TODO: this condition should never occur
        if !self.state.can_write() {
            warn!("not allowed to write");
            return Ok(());
        }

        let tung_msg = data.into();
        self.stream.send(tung_msg).await.map_err(|err| err.into())
    }
}
