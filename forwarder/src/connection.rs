use std::collections::HashMap;
use std::fmt::Display;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use displaydoc::Display;
use futures_util::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::sync::mpsc::error::SendError;
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};
use tokio::{
    net::TcpStream,
    select,
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, instrument, warn};
use tungstenite::{Error as TungError, Message as TungMessage};

use serde::{Deserialize, Serialize};

use crate::proto_message::{
    headermap_to_hashmap, Http, HttpResponse, ProtoError, ProtoMessage, WebSocket, WebSocketMessage,
};

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

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
}

#[derive(Debug)]
struct ConnectionHandle {
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

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct Id {
    port: u16,
    host: Arc<String>,
}

impl Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "({},{})", self.port, self.host)
    }
}

impl Id {
    pub fn new(port: u16, host: String) -> Self {
        Self {
            port,
            host: Arc::new(host),
        }
    }
}

#[derive(Debug)]
pub struct Transmitted {
    id: Id,
    msg: ConnMsg,
}

impl Transmitted {
    pub fn into_inner(self) -> (Id, ConnMsg) {
        (self.id, self.msg)
    }

    // fn data(id: Id, data: Vec<u8>) -> Self {
    //     Self {
    //         id,
    //         msg: ConnMsg::Data(data),
    //     }
    // }

    // fn eot(id: Id) -> Self {
    //     Self {
    //         id,
    //         msg: ConnMsg::Eot,
    //     }
    // }

    // fn close(id: Id) -> Self {
    //     Self {
    //         id,
    //         msg: ConnMsg::Close,
    //     }
    // }
}

#[derive(Debug)]
pub enum ConnMsg {
    Data(Vec<u8>),
    Eot,
    Close,
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
struct Connection {
    id: Vec<u8>,
    stream: WsStream,
    state: ConnState,
    tx_ws: UnboundedSender<ProtoMessage>,
    rx_con: UnboundedReceiver<WebSocketMessage>,
}

impl Connection {
    fn new(
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

    fn spawn(self, tx_con: UnboundedSender<WebSocketMessage>) -> ConnectionHandle {
        // spawn a task responsible for notifying when new data is available on the TCP reader
        let handle = tokio::spawn(self.task());
        ConnectionHandle { handle, tx_con }
    }

    async fn task(mut self) -> Result<(), ConnectionError> {
        self.task_loop().await.map_err(|err| {
            error!("error while reading/writing, {err}");

            let msg = ProtoMessage::new(crate::proto_message::Protocol::WebSocket(
                WebSocket::close(self.id, None),
            ));

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
        // let msg = bytes.map(|bytes| {
        //     info!("received {bytes} bytes");
        //     let id = self.id.clone();
        //     match bytes {
        //         0 => {
        //             self.state.shutdown_read();
        //             info!("changed read state");
        //             Transmitted::eot(id)
        //         }
        //         n => Transmitted::data(id, buf[0..n].to_vec()),
        //     }
        // })?;

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

        // match data {
        //     ConnMsg::Data(data) if data.is_empty() => {
        //         error!("received 0 bytes from ws, shutting down write");
        //         self.shutdown_write().await?;
        //         info!("changed write state after receiving 0 bytes");
        //     }
        //     ConnMsg::Eot => {
        //         info!("eot received, shutting down write");
        //         self.shutdown_write().await?;
        //         info!("changed write state after receiving eot");
        //     }
        //     ConnMsg::Data(data) => {
        //         info!("received {} bytes from ws", data.len());

        //         if !self.state.can_write() {
        //             warn!("not allowed to write");
        //             return Ok(());
        //         }

        //         self.stream.write_all(&data).await?;
        //     }
        //     ConnMsg::Close => {
        //         // connection has already been closed
        //         info!("connection closed");
        //         self.stream.shutdown().await?;
        //         self.state = ConnState::Closed;
        //     }
        // }

        // Ok(())
    }

    // async fn shutdown_write(&mut self) -> io::Result<()>
    // where
    //     T: AsyncWrite + Unpin,
    // {
    //     self.state.shutdown_write();
    //     self.stream.shutdown().await
    // }
}

// #[derive(Debug)]
// pub struct WsTransmitted {
//     id: Id,
//     msg: WsMsg,
// }

// impl WsTransmitted {
//     pub fn new(id: Id, msg: WsMsg) -> Self {
//         Self { id, msg }
//     }

//     pub fn encode(self) -> Result<Vec<u8>, ConnectionError> {
//         let mut id = proto::Id::default();
//         id.port = self.id.port.into();
//         id.host = self.id.host.to_string();

//         let mut msg = proto::WsMsg::default();
//         msg.ws_msg_type = Some(self.msg.into());

//         let mut transmitted_msg = proto::WsTransmitted::default();
//         transmitted_msg.id = Some(id);
//         transmitted_msg.msg = Some(msg);

//         let mut buf = Vec::new();
//         let buf_size = transmitted_msg.encoded_len();
//         buf.reserve(buf_size);

//         transmitted_msg
//             .encode(&mut buf)
//             .expect("failed to serialize WsTransmitted");

//         Ok(buf)
//     }

//     pub fn decode(bytes: &[u8]) -> Result<Self, ()> {
//         let msg_transmitted = proto::WsTransmitted::decode(bytes).map_err(|_| ())?;
//         WsTransmitted::try_from(msg_transmitted)
//     }
// }

// #[derive(Serialize, Deserialize, Debug)]
// pub enum WsMsg {
//     NewConnection,
//     Eot,
//     CloseConnection,
//     Data(Vec<u8>),
// }

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
    pub fn new(
        session_token: String,
        ws_stream: WsStream,
    ) -> (Self, UnboundedReceiver<ProtoMessage>) {
        // this channel is used by tasks associated to each connection to communicate new
        // information available on a given tcp reader handle (associated to an u16 connection ID).
        // it is also used to forward the incoming data to the device over the websocket connection
        let (tx_ws, rx_ws) = mpsc::unbounded_channel();

        let connections = Self {
            session_token,
            ws_stream,
            connections: HashMap::new(),
            tx_ws,
        };

        (connections, rx_ws)
    }

    // insertion of a new connection given a tcp_stream
    pub async fn add_connection(&mut self, stream: WsStream, id: &[u8]) {
        let (tx_con, rx_con) = mpsc::unbounded_channel();

        let tx_ws = self.tx_ws.clone();
        let connection = Connection::new(id, stream, tx_ws, rx_con).spawn(tx_con);

        // because the id_count is internally managed, this function should always return Some(id)
        // otherwise it would mean that a new connection with the same ID of an existing one is openned
        if self.connections.insert(id.to_vec(), connection).is_some() {
            error!("connection replaced");
        }
    }

    #[instrument(skip_all)]
    fn get_connection(&mut self, id: &[u8]) -> Option<&mut ConnectionHandle> {
        self.connections.get_mut(id)
    }

    pub async fn select_ws_op(
        &mut self,
        rx_ws: &mut UnboundedReceiver<ProtoMessage>,
        timeout_ping: Duration,
    ) -> WebSocketOperation {
        select! {
            res = timeout(timeout_ping, self.ws_stream.next()) => {
                match res {
                    Ok(msg) => WebSocketOperation::Receive(msg),
                    Err(_) => WebSocketOperation::Ping
                }
            }
            tung_msg = rx_ws.recv() => WebSocketOperation::Send(tung_msg)
        }
    }

    pub async fn send(&mut self, tung_msg: TungMessage) -> Result<(), ConnectionError> {
        self.ws_stream
            .send(tung_msg)
            .await
            .map_err(|err| err.into())
    }

    #[instrument(skip_all)]
    pub async fn handle_msg(&mut self, msg: TungMessage) -> Result<(), ConnectionError> {
        match msg {
            // TODO: handle other types of messages
            TungMessage::Ping(data) => {
                let msg = TungMessage::Pong(data);

                self.send(msg).await?;
            }
            TungMessage::Pong(_) => debug!("received Pong frame from edgeho"),
            TungMessage::Close(_) => todo!("handle Close from edgehog"),
            TungMessage::Binary(bytes) => {
                let protocol = ProtoMessage::decode(&bytes)?.protocol();

                match protocol {
                    crate::proto_message::Protocol::Http(Http::Response(http_res)) => {
                        error!(
                            "shouldn't receive HttpResponses from edgehog, {:?}",
                            http_res
                        )
                    }
                    crate::proto_message::Protocol::Http(Http::Request(http_req)) => {
                        if http_req.is_connection_upgrade() {
                            let (request_id, ws_stream_ttyd, res) = http_req.upgrade().await?;

                            // store the new WebSocketStream inside Connections struct
                            // use as ID of the connection the ID of the HTTP request/response
                            self.add_connection(ws_stream_ttyd, &request_id).await;

                            let status_code = res.status().into();
                            let headers = headermap_to_hashmap(res.headers().iter());
                            let payload = res.into_body();

                            let proto_res =
                                HttpResponse::new(&request_id, status_code, headers, payload);
                            let msg = ProtoMessage::new(crate::proto_message::Protocol::Http(
                                Http::Response(proto_res),
                            ));

                            self.tx_ws.send(msg)?;
                        } else {
                            let (request_id, mut res) = http_req.send().await?;

                            let status_code = res.status().into();
                            let headers = headermap_to_hashmap(res.headers().iter());

                            // for every received chunk of bytes
                            while let Some(payload) = res.chunk().await? {
                                let proto_res = HttpResponse::new(
                                    &request_id,
                                    status_code,
                                    headers.clone(),
                                    Some(payload.into()),
                                );

                                let msg = ProtoMessage::new(crate::proto_message::Protocol::Http(
                                    Http::Response(proto_res),
                                ));

                                self.tx_ws.send(msg)?;
                            }
                        }
                    }
                    crate::proto_message::Protocol::WebSocket(ws) => {
                        // TODO: construct the equivalent tokio_tungstenite Message and send it to ttyd
                        // using the socket_id to identify which ws channel must be used
                        let (id, ws_msg) = ws.into_inner();
                        match self.get_connection(&id) {
                            Some(connection) => connection.tx_con.send(ws_msg)?,
                            None => {
                                error!("connection {id:?} not found, discarding data");
                                return Ok(());
                            }
                        }
                    }
                }

                // let msg_transmitted = WsTransmitted::decode(&bytes)
                //     .expect("failed to deserialize from bytes to WsTransmitted");
                // let WsTransmitted { id, msg } = msg_transmitted;

                // match msg {
                //     // this will be called only by a device when a NewConnection msg is received
                //     WsMsg::NewConnection => {
                //         debug!("new connection received, {id}");

                //         let ttyd_addr = "127.0.0.1:7681"; // TTYD
                //         let ttdy_stream = match TcpStream::connect(ttyd_addr).await {
                //             Ok(stream) => stream,
                //             Err(err) => {
                //                 error!(?err);
                //                 return Ok(());
                //             }
                //         };

                //         // // TODO: it was also possible to create a websocket connection to TTYD server instance instead of using TCP
                //         // let ws_ttyd = match connect_async("ws://127.0.0.1:7681").await {
                //         //     Ok(stream) => stream,
                //         //     Err(err) => {
                //         //         error!(?err);
                //         //         return Ok(());
                //         //     }
                //         // };

                //         debug!("connection accepted: {id}");

                //         self.add_connection(ttdy_stream, id).await;
                //     }

                //     // handle the reception of new data by forwarding them through the TCP connection
                //     WsMsg::Data(data) => match self.get_connection(&id) {
                //         Some(connection) => connection.tx_con.send(ConnMsg::Data(data))?,
                //         None => {
                //             error!("connection {id} not found, discarding data");
                //             return Ok(());
                //         }
                //     },
                //     WsMsg::Eot => match self.get_connection(&id) {
                //         Some(connection) => connection.tx_con.send(ConnMsg::Eot)?,
                //         None => {
                //             error!("connection {id} not found, discarding data");
                //             return Ok(());
                //         }
                //     },
                //     // handle the closure of a connection
                //     WsMsg::CloseConnection => {
                //         match self.connections.remove(&id) {
                //             Some(con) => {
                //                 if let Err(err) = con.tx_con.send(ConnMsg::Close) {
                //                     debug!("connection {id} already closed, {err}");
                //                 }
                //                 info!("connection closed: {id}");
                //             }
                //             // this occurs in case there no exist a connection with the provided id
                //             None => warn!("connection {id} already removed"),
                //         }
                //     }
                // }
            }
            // wrong Message type
            _ => error!("unhandled message type: {msg:?}"),
        }

        Ok(())
    }
}

// // TODO: rename in "send_ws"
// #[instrument(skip_all)]
// pub async fn recv_tcp<T>(data: Option<TungMessage>, ws_stream: &mut WebSocketStream<T>)
// where
//     T: AsyncRead + AsyncWrite + Unpin,
// {
//     match data {
//         Some(transmitted) => {

//             // close the connection if no data is sent
//             let bridge_data = match msg {
//                 ConnMsg::Data(data) => {
//                     info!("{}: {} bytes received from TCP connection", id, data.len());
//                     WsTransmitted::new(id, WsMsg::Data(data))
//                 }
//                 ConnMsg::Eot => {
//                     info!("eot received, closing connection {id}");
//                     WsTransmitted::new(id, WsMsg::Eot)
//                 }
//                 ConnMsg::Close => {
//                     info!("tcp closed, closing connection {id}");
//                     WsTransmitted::new(id, WsMsg::CloseConnection)
//                 }
//             };

//             let bytes = bridge_data
//                 .encode()
//                 .expect("Failed to serialize WsTransmitted into items::WsTransmitted");
//             let msg = TungMessage::Binary(bytes);
//             ws_stream
//                 .send(msg)
//                 .await
//                 .expect("failed to send data on websocket toward device");
//         }
//         // rx hand side of the channel read None, therefore the connection has been closed
//         None => {
//             warn!("rx hand side of the channel read None, all connections have been closed");
//         }
//     }
// }
