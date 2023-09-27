use std::collections::HashMap;
use std::ops::{Deref, DerefMut};

use backoff::ExponentialBackoff;
use futures_util::{SinkExt, StreamExt};
use tokio::time::{timeout, Duration};
use tokio::{
    net::TcpStream,
    select,
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, instrument, warn};
use tungstenite::{Error as TungError, Message as TungMessage};
use url::Url;

use crate::connection::{Connection, ConnectionError, ConnectionHandle};
use crate::proto_message::Protocol as ProtoProtocol;
use crate::proto_message::{headermap_to_hashmap, Http as ProtoHttp, HttpResponse, ProtoMessage};

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug)]
pub struct ConnectionsHandler {
    connections: Connections,
    rx_ws: UnboundedReceiver<ProtoMessage>,
    url: Url,
}

impl ConnectionsHandler {
    pub fn new(connections: Connections, rx_ws: UnboundedReceiver<ProtoMessage>, url: Url) -> Self {
        Self {
            connections,
            rx_ws,
            url,
        }
    }

    pub fn get_url(&self) -> Url {
        self.url.clone()
    }

    pub async fn select_ws_op(&mut self, timeout_ping: Duration) -> WebSocketOperation {
        let Self {
            connections, rx_ws, ..
        } = self;

        select! {
            res = timeout(timeout_ping, connections.ws_stream.next()) => {
                match res {
                    Ok(msg) => WebSocketOperation::Receive(msg),
                    Err(_) => WebSocketOperation::Ping
                }
            }
            tung_msg = rx_ws.recv() => WebSocketOperation::Send(tung_msg)
        }
    }
}

impl Deref for ConnectionsHandler {
    type Target = Connections;

    fn deref(&self) -> &Self::Target {
        &self.connections
    }
}

impl DerefMut for ConnectionsHandler {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.connections
    }
}

pub enum WebSocketOperation {
    Receive(Option<Result<TungMessage, TungError>>),
    Send(Option<ProtoMessage>),
    Ping,
}

#[derive(Debug)]
pub struct Connections {
    // TODO: USARE IL SESSION TOKEN PER EFFETTUARE UNA RECONNECT
    session_token: String,
    ws_stream: WsStream,
    connections: HashMap<Vec<u8>, ConnectionHandle>,
    tx_ws: UnboundedSender<ProtoMessage>,
}

impl Connections {
    pub fn new(session_token: String, ws_stream: WsStream, url: Url) -> ConnectionsHandler {
        // this channel is used by tasks associated to each connection to communicate new
        // information available on a given websocket between the device and TTYD.
        // it is also used to forward the incoming data from TTYD to the device.
        let (tx_ws, rx_ws) = mpsc::unbounded_channel();

        let connections = Self {
            session_token,
            ws_stream,
            connections: HashMap::new(),
            tx_ws,
        };

        ConnectionsHandler::new(connections, rx_ws, url)
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

    pub async fn reconnect(&mut self, url: Url) -> Result<(), ConnectionError> {
        // try openning a websocket connection with edgehog using exponential backoff
        let (new_ws_stream, http_res) =
            backoff::future::retry(ExponentialBackoff::default(), || async {
                println!("trying to reconnect with {}", url);
                Ok(tokio_tungstenite::connect_async(&url).await?)
            })
            .await
            .map_err(|err| ConnectionError::Reconnect(err))?;

        debug!(?http_res);

        self.ws_stream = new_ws_stream;

        Ok(())
    }

    pub async fn send(&mut self, tung_msg: TungMessage) -> Result<(), ConnectionError> {
        self.ws_stream
            .send(tung_msg)
            .await
            .map_err(|err| err.into())
    }

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
    pub async fn handle_msg(&mut self, msg: TungMessage) -> Result<(), ConnectionError> {
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
}
