use std::collections::HashMap;

use backoff::ExponentialBackoff;
use displaydoc::Display;
use futures_util::{SinkExt, StreamExt};
use thiserror::Error as ThisError;
use tokio::time::{timeout, Duration};
use tokio::{
    net::TcpStream,
    select,
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Error as TungError;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, instrument, warn};
use tungstenite::Message as TungMessage;
use url::Url;

use crate::connection::{Connection, ConnectionError, ConnectionHandle};
use crate::proto_message::{
    Http as ProtoHttp, HttpResponse, ProtoError, ProtoMessage, Protocol as ProtoProtocol,
};

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Display, ThisError, Debug)]
pub enum Error {
    /// Error performing exponential backoff when trying to connect with Edgehog, `{0}`.
    WebSocketConnect(#[from] TungError),

    /// Trying to perform an operation over a non-existing connection
    NonExistingConnection,
    /// Connection errors, `{0}`.
    Connection(#[from] ConnectionError),
    /// Protobuf error, `{0}`.
    Protobuf(#[from] ProtoError),
}

#[derive(Debug)]
pub struct ConnectionsHandler {
    connections: Connections,
    ws_stream: WsStream,
    rx_ws: UnboundedReceiver<ProtoMessage>,
    url: Url,
}

impl ConnectionsHandler {
    /// Method called to establish a new connection between the device and edgehog.
    pub async fn connect(url: Url) -> Result<Self, Error> {
        // TODO: (TO FIX) when a wrong URL is passed it will endlessly try to connect
        // try openning a websocket connection with edgehog using exponential backoff
        let (ws_stream, http_res) =
            backoff::future::retry(ExponentialBackoff::default(), || async {
                println!("creating websocket connection with {}", url);
                Ok(connect_async(&url).await?)
            })
            .await
            .map_err(Error::WebSocketConnect)?;

        debug!(?http_res);

        // this channel is used by tasks associated to each connection to communicate new
        // information available on a given websocket between the device and TTYD.
        // it is also used to forward the incoming data from TTYD to the device.
        let (tx_ws, rx_ws) = mpsc::unbounded_channel();

        let connections = Connections::new(tx_ws);

        Ok(Self {
            connections,
            ws_stream,
            rx_ws,
            url,
        })
    }

    pub async fn handle_connections(&mut self) -> Result<(), Error> {
        loop {
            // TODO: decide how to set the timeout interval to send Ping frame
            let op = self.select_ws_op(Duration::from_secs(5)).await;

            let res = match op {
                // receive data from edgehog
                WebSocketOperation::Receive(msg) => match msg {
                    Some(Ok(msg)) => self.handle_msg(msg).await,
                    Some(Err(err)) => Err(err.into()),
                    None => {
                        error!("stream closed, terminating device...");
                        break;
                    }
                },
                // receive data from TTYD
                WebSocketOperation::Send(tung_msg) => {
                    let msg = tung_msg
                        .expect("rx_ws channel returned None, channel closed")
                        .encode()?;

                    let msg = TungMessage::Binary(msg);

                    self.send(msg).await
                }
                // in case no data is received in X seconds over the websocket, send a ping.
                // if no pong is received within Y seconds, close the connection gracefully
                WebSocketOperation::Ping => {
                    let msg = TungMessage::Ping(Vec::new());

                    self.send(msg).await
                }
            };

            // if the connection has been suddenly interrupted, try re-establishing it
            if let Err(err) = res {
                match err {
                    ConnectionError::WebSocketSend(err)
                    | ConnectionError::WebSocketNext(err)
                    | ConnectionError::Reconnect(err) => {
                        error!(?err);
                        debug!("trying to reconnect");

                        self.reconnect().await?;
                    }
                    err => error!(?err),
                }
            }
        }

        Ok(())
    }

    async fn select_ws_op(&mut self, timeout_ping: Duration) -> WebSocketOperation {
        select! {
            res = timeout(timeout_ping, self.ws_stream.next()) => {
                match res {
                    Ok(msg) => WebSocketOperation::Receive(msg),
                    Err(_) => WebSocketOperation::Ping
                }
            }
            tung_msg = self.rx_ws.recv() => WebSocketOperation::Send(tung_msg)
        }
    }

    async fn send(&mut self, tung_msg: TungMessage) -> Result<(), ConnectionError> {
        self.ws_stream
            .send(tung_msg)
            .await
            .map_err(|err| err.into())
    }

    // TODO: IT SHOULD BE THE CONNECTION TO SEND THE RESPONSE TO THE CONNECTIONHANDLER
    fn send_http_res(
        &mut self,
        request_id: &[u8],
        status_code: u16,
        headers: HashMap<String, String>,
        payload: Vec<u8>,
    ) -> Result<(), ConnectionError> {
        let proto_res = HttpResponse::new(request_id, status_code, headers, payload);

        debug!(?proto_res);

        let msg = ProtoMessage::new(ProtoProtocol::Http(ProtoHttp::Response(proto_res)));

        self.tx_ws.send(msg).map_err(|err| err.into())
    }

    #[instrument(skip_all)]
    async fn handle_msg(&mut self, msg: TungMessage) -> Result<(), ConnectionError> {
        match msg {
            TungMessage::Ping(data) => {
                let msg = TungMessage::Pong(data);

                self.send(msg).await?;
            }
            TungMessage::Pong(_) => debug!("received Pong frame from edgehog"),
            TungMessage::Close(_) => {
                // close every websocket connection between TTYD and the device
                self.connections
                    .values_mut()
                    .for_each(|con| con.abort_handle().abort());

                self.connections.clear();

                info!("closed every websocket connection between TTYD and the device");
            }
            TungMessage::Text(data) => warn!("received Text websocket frame, {data}"),
            TungMessage::Binary(bytes) => {
                let protocol = ProtoMessage::decode(&bytes)?.protocol();

                match protocol {
                    ProtoProtocol::Http(ProtoHttp::Response(http_res)) => {
                        error!(
                            "shouldn't receive HttpResponses from edgehog, {:?}",
                            http_res
                        )
                    }
                    ProtoProtocol::Http(ProtoHttp::Request(http_req)) => {
                        if http_req.is_connection_upgrade() {
                            let http_upgrade = http_req.upgrade().await?;
                            let (request_id, ws_stream_ttyd, res) = http_upgrade.into_inner();

                            // store the new WebSocketStream inside Connections struct
                            // use as ID of the connection the ID of the HTTP request/response
                            self.add_connection(ws_stream_ttyd, request_id.clone())
                                .await;

                            let status_code = res.status().into();
                            let headers = headermap_to_hashmap(res.headers().iter());
                            let payload = res.into_body().map_or(Vec::new(), |p| p);

                            self.send_http_res(&request_id, status_code, headers, payload)?;
                        } else {
                            let (request_id, mut res) = http_req.send().await?;

                            let status_code = res.status().into();
                            let headers = headermap_to_hashmap(res.headers().iter());

                            // for every received chunk of bytes
                            // TODO: spawn a task to avoid blocking
                            while let Some(payload) = res.chunk().await? {
                                self.send_http_res(
                                    &request_id,
                                    status_code,
                                    headers.clone(),
                                    payload.into(),
                                )?;
                            }
                        }
                    }
                    ProtoProtocol::WebSocket(ws) => {
                        let (id, ws_msg) = ws.into_inner();

                        // check if the message received from edgehog is a Close frame. Close the corresponding connection.
                        if ws_msg.is_close() {
                            match self.remove_connection(&id) {
                                Some(connection) => connection.close(ws_msg).await?,
                                None => {
                                    error!("connection {id:?} not found, discarding data");
                                    return Ok(());
                                }
                            }
                            return Ok(());
                        }

                        match self.get_connection(&id) {
                            Some(connection) => connection.send_channel(ws_msg).await?,
                            None => {
                                error!("connection {id:?} not found, discarding data");
                                return Ok(());
                            }
                        }
                    }
                }
            }
            // wrong Message type
            _ => error!("unhandled message type: {msg:?}"),
        }

        Ok(())
    }

    pub async fn reconnect(&mut self) -> Result<(), ConnectionError> {
        // try openning a websocket connection with edgehog using exponential backoff
        let (new_ws_stream, http_res) =
            backoff::future::retry(ExponentialBackoff::default(), || async {
                println!("trying to reconnect with {}", self.url);
                Ok(tokio_tungstenite::connect_async(&self.url).await?)
            })
            .await
            .map_err(|err| ConnectionError::Reconnect(err))?;

        debug!(?http_res);

        self.ws_stream = new_ws_stream;

        Ok(())
    }
}

pub enum WebSocketOperation {
    Receive(Option<Result<TungMessage, TungError>>),
    Send(Option<ProtoMessage>),
    Ping,
}

#[derive(Debug)]
pub struct Connections {
    connections: HashMap<Vec<u8>, ConnectionHandle>,
    tx_ws: UnboundedSender<ProtoMessage>,
}

impl Connections {
    pub fn new(tx_ws: UnboundedSender<ProtoMessage>) -> Self {
        Self {
            connections: HashMap::new(),
            tx_ws,
        }
    }

    // insertion of a new connection given a tcp_stream
    pub async fn add_connection(&mut self, stream: WsStream, id: Vec<u8>) {
        let (tx_con, rx_con) = mpsc::unbounded_channel();

        let tx_ws = self.tx_ws.clone();
        let connection = Connection::new(id.clone(), stream, tx_ws, rx_con).spawn(tx_con);

        // because the id_count is internally managed, this function should always return Some(id)
        // otherwise it would mean that a new connection with the same ID of an existing one is opened
        if self.connections.insert(id, connection).is_some() {
            error!("connection replaced");
        }
    }

    #[instrument(skip_all)]
    fn get_connection(&mut self, id: &[u8]) -> Option<&mut ConnectionHandle> {
        self.connections.get_mut(id)
    }

    #[instrument(skip_all)]
    fn remove_connection(&mut self, id: &[u8]) -> Option<ConnectionHandle> {
        self.connections.remove(id)
    }
}
