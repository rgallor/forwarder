use std::fmt::Display;
use std::io;
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::{collections::HashMap, env};

use futures_util::{stream::StreamExt, SinkExt};
use prost::Message as ProstMsg;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::mpsc::error::SendError;
use tokio::task::JoinHandle;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    select,
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::{accept_async, WebSocketStream};
use tracing::{debug, error, info, instrument, warn};
use tracing_subscriber::{prelude::*, EnvFilter};
use tungstenite::Message;

use serde::{Deserialize, Serialize};

// Include the `items` module, which is generated from items.proto.
pub mod items {
    include!(concat!(env!("OUT_DIR"), "/forwarder.items.rs"));
}

impl TryFrom<items::WsTransmitted> for WsTransmitted {
    type Error = ();

    fn try_from(value: items::WsTransmitted) -> Result<Self, Self::Error> {
        let items::WsTransmitted { id, msg } = value;

        let id = id.map(|id| id.try_into()).unwrap()?;
        let msg = msg.map(|ws_msg| ws_msg.into()).unwrap();

        let res = WsTransmitted { id, msg };
        Ok(res)
    }
}

impl TryFrom<items::Id> for Id {
    type Error = (); // TODO: return a new error type

    fn try_from(value: items::Id) -> Result<Self, Self::Error> {
        let items::Id { port, host } = value;

        let port = u16::try_from(port).map_err(|_| ())?;
        let host = Arc::new(host);

        let id = Id { port, host };
        Ok(id)
    }
}

impl From<items::ws_msg::WsMsgType> for WsMsg {
    fn from(value: items::ws_msg::WsMsgType) -> Self {
        match value {
            items::ws_msg::WsMsgType::NewConnection(_) => WsMsg::NewConnection,
            items::ws_msg::WsMsgType::Eot(_) => WsMsg::Eot,
            items::ws_msg::WsMsgType::CloseConnection(_) => WsMsg::CloseConnection,
            items::ws_msg::WsMsgType::Data(items::Data { data }) => WsMsg::Data(data),
        }
    }
}

impl From<WsMsg> for items::ws_msg::WsMsgType {
    fn from(value: WsMsg) -> Self {
        match value {
            WsMsg::NewConnection => Self::NewConnection(items::NewConnection {}),
            WsMsg::Eot => Self::Eot(items::Eot {}),
            WsMsg::CloseConnection => Self::CloseConnection(items::CloseConnection {}),
            WsMsg::Data(data) => Self::Data(items::Data { data }),
        }
    }
}

impl From<items::WsMsg> for WsMsg {
    fn from(value: items::WsMsg) -> Self {
        value
            .ws_msg_type
            .map(|msg_type| msg_type.into())
            .expect("WsMsgType empty")
    }
}

// -----------------------------------------------------------------------------------------------------------------------------------

struct ConnectionHandle {
    handle: JoinHandle<()>,
    tx_con: UnboundedSender<ConnMsg>,
}

impl Deref for ConnectionHandle {
    type Target = JoinHandle<()>;

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
struct Id {
    port: u16,
    host: Arc<String>,
}

impl Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "({},{})", self.port, self.host)
    }
}

#[derive(Debug)]
struct Transmitted {
    id: Id,
    msg: ConnMsg,
}

impl Transmitted {
    fn data(id: Id, data: Vec<u8>) -> Self {
        Self {
            id,
            msg: ConnMsg::Data(data),
        }
    }

    fn eot(id: Id) -> Self {
        Self {
            id,
            msg: ConnMsg::Eot,
        }
    }

    fn close(id: Id) -> Self {
        Self {
            id,
            msg: ConnMsg::Close,
        }
    }
}

#[derive(Debug)]
enum ConnMsg {
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
    Read(std::io::Result<usize>),
    Write(Option<ConnMsg>),
}

#[derive(Debug)]
struct Connection<T> {
    id: Id,
    stream: T,
    state: ConnState,
    tx_ws: UnboundedSender<Transmitted>,
    rx_con: UnboundedReceiver<ConnMsg>,
}

impl<T> Connection<T> {
    fn new(
        id: Id,
        stream: T,
        tx_ws: UnboundedSender<Transmitted>,
        rx_con: UnboundedReceiver<ConnMsg>,
    ) -> Self {
        Self {
            id,
            stream,
            state: ConnState::default(),
            tx_ws,
            rx_con,
        }
    }

    fn spawn(self, tx_con: UnboundedSender<ConnMsg>) -> ConnectionHandle
    where
        T: Send + AsyncRead + AsyncWrite + Unpin + 'static,
    {
        // spawn a task responsible for notifying when new data is available on the TCP reader
        let handle = tokio::spawn(self.task());
        ConnectionHandle { handle, tx_con }
    }

    async fn task(mut self)
    where
        T: AsyncRead + AsyncWrite + Unpin,
    {
        if let Err(err) = self.task_loop().await {
            error!("error while reading/writing, {err}");
        }

        self.tx_ws
            .send(Transmitted::close(self.id))
            .expect("failed to send over tx_ws channel");
    }

    #[instrument(skip_all)]
    async fn task_loop(&mut self) -> io::Result<()>
    where
        T: AsyncRead + AsyncWrite + Unpin,
    {
        let mut buf = vec![0u8; 1024];

        // TODO: add timeout
        // select return None if the connection state is Closed
        while let Some(r_w) = self.select(&mut buf).await {
            match r_w {
                Either::Read(data) => self.handle_tcp_read(data, &buf)?,
                Either::Write(opt) => self.handle_tcp_write(opt).await?,
            }
        }

        info!("connection state Closed, exiting...");

        Ok(())
    }

    async fn select(&mut self, buf: &mut [u8]) -> Option<Either>
    where
        T: AsyncRead + Unpin,
    {
        match self.state {
            ConnState::ReadWrite | ConnState::Read => Some(select! {
                res = self.stream.read(buf) => Either::Read(res),
                opt = self.rx_con.recv() => Either::Write(opt),
            }),
            ConnState::Write => Some(Either::Write(self.rx_con.recv().await)),
            ConnState::Closed => None,
        }
    }

    /// Read from tcp.
    #[instrument(skip(self, buf), fields(id = %self.id, state = %self.state))]
    fn handle_tcp_read(&mut self, bytes: io::Result<usize>, buf: &[u8]) -> io::Result<()> {
        let msg = bytes.map(|bytes| {
            info!("received {bytes} bytes");
            let id = self.id.clone();
            match bytes {
                0 => {
                    self.state.shutdown_read();
                    info!("changed read state");
                    Transmitted::eot(id)
                }
                n => Transmitted::data(id, buf[0..n].to_vec()),
            }
        })?;

        self.tx_ws
            .send(msg)
            .expect("error while sending on channel");
        Ok(())
    }

    /// Write to tcp.
    #[instrument(skip_all, fields(id = %self.id, state = %self.state))]
    async fn handle_tcp_write(&mut self, data: Option<ConnMsg>) -> io::Result<()>
    where
        T: AsyncWrite + Unpin,
    {
        let data = data.unwrap_or_else(|| {
            error!("tcp channel dropped");
            ConnMsg::Close
        });

        match data {
            ConnMsg::Data(data) if data.is_empty() => {
                error!("received 0 bytes from ws, shutting down write");
                self.shutdown_write().await?;
                info!("changed write state after receiving 0 bytes");
            }
            ConnMsg::Eot => {
                info!("eot received, shutting down write");
                self.shutdown_write().await?;
                info!("changed write state after receiving eot");
            }
            ConnMsg::Data(data) => {
                info!("received {} bytes from ws", data.len());

                if !self.state.can_write() {
                    warn!("not allowed to write");
                    return Ok(());
                }

                self.stream.write_all(&data).await?;
            }
            ConnMsg::Close => {
                // connection has already been closed
                info!("connection closed");
                self.stream.shutdown().await?;
                self.state = ConnState::Closed;
            }
        }

        Ok(())
    }

    async fn shutdown_write(&mut self) -> io::Result<()>
    where
        T: AsyncWrite + Unpin,
    {
        self.state.shutdown_write();
        self.stream.shutdown().await
    }
}

#[derive(Debug)]
struct WsTransmitted {
    id: Id,
    msg: WsMsg,
}

impl WsTransmitted {
    fn encode(self) -> Result<Vec<u8>, ()> {
        let mut id = items::Id::default();
        id.port = self.id.port.into();
        id.host = self.id.host.to_string(); //.expect("failed to remove String from Arc");

        let mut msg = items::WsMsg::default();
        msg.ws_msg_type = Some(self.msg.into());

        let mut transmitted_msg = items::WsTransmitted::default();
        transmitted_msg.id = Some(id);
        transmitted_msg.msg = Some(msg);

        let mut buf = Vec::new();
        let buf_size = transmitted_msg.encoded_len();
        buf.reserve(buf_size);

        transmitted_msg
            .encode(&mut buf)
            .expect("failed to serialize WsTransmitted");

        Ok(buf)
    }

    fn decode(bytes: &[u8]) -> Result<Self, ()> {
        let msg_transmitted = items::WsTransmitted::decode(bytes).map_err(|_| ())?;
        WsTransmitted::try_from(msg_transmitted)
    }
}

#[derive(Serialize, Deserialize, Debug)]
enum WsMsg {
    NewConnection, // TODO: use Transport (TCP or UDP)
    Eot,
    CloseConnection,
    Data(Vec<u8>),
}

struct Connections {
    connections: HashMap<Id, ConnectionHandle>,
    tx_ws: UnboundedSender<Transmitted>,
}

impl Connections {
    fn new() -> (Self, UnboundedReceiver<Transmitted>) {
        // this channel is used by tasks associated to each connection to communicate new
        // information available on a given tcp reader handle (associated to an u16 connection ID).
        // it is also used to forward the incoming data to the device over the websocket connection
        let (tx_ws, rx_ws) = mpsc::unbounded_channel();

        let connections = Self {
            connections: HashMap::new(),
            tx_ws,
        };

        (connections, rx_ws)
    }

    // insertion of a new connection given a tcp_stream
    async fn add_connection(&mut self, tcp_stream: TcpStream, id: Id) {
        let (tx_con, rx_con) = mpsc::unbounded_channel();

        let tx_ws = self.tx_ws.clone();
        let connection = Connection::new(id.clone(), tcp_stream, tx_ws, rx_con).spawn(tx_con);

        // because the id_count is internally managed, this function should always return Some(id)
        // otherwise it would mean that a new connection with the same ID of an existing one is openned

        if self.connections.insert(id, connection).is_some() {
            error!("connection replaced");
        }
    }

    // TODO: define method get_connection and return error
    #[instrument(skip_all)]
    async fn handle_msg(&mut self, msg: Message) -> Result<(), SendError<ConnMsg>> {
        match msg {
            Message::Binary(bytes) => {
                let msg_transmitted = WsTransmitted::decode(&bytes)
                    .expect("failed to deserialize from bytes to WsTransmitted");
                let WsTransmitted { id, msg } = msg_transmitted;

                match msg {
                    // this will be called only by a device when a NewConnection msg is received
                    WsMsg::NewConnection => {
                        debug!("new connection received, {id}");

                        let ttyd_addr = "127.0.0.1:7681"; // TTYD
                        let ttdy_stream = match tokio::net::TcpStream::connect(ttyd_addr).await {
                            Ok(stream) => stream,
                            Err(err) => {
                                error!(?err);
                                return Ok(());
                            }
                        };

                        debug!("connection accepted: {id}");

                        self.add_connection(ttdy_stream, id).await;
                    }

                    // handle the reception of new data by forwarding them through the TCP connection
                    WsMsg::Data(data) => {
                        let Some(connection) = self.connections.get_mut(&id) else {
                            error!("connection {id} not found, discarding data");
                            // TODO: discard received data if they belong to a different generation
                            return Ok(());
                        };

                        connection.tx_con.send(ConnMsg::Data(data))?;
                    }
                    WsMsg::Eot => {
                        let Some(connection) = self.connections.get_mut(&id) else {
                            error!("connection {id} not found, discarding data");
                            return Ok(());
                        };

                        connection.tx_con.send(ConnMsg::Eot)?;
                    }
                    // handle the closure of a connection
                    WsMsg::CloseConnection => {
                        match self.connections.remove(&id) {
                            Some(con) => {
                                if let Err(err) = con.tx_con.send(ConnMsg::Close) {
                                    debug!("connection {id} already closed, {err}");
                                }
                                info!("connection closed: {id}");
                            }
                            // this occurs in case there no exist a connection with the provided id
                            None => warn!("connection {id} already removed"),
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

async fn main_bridge() -> color_eyre::Result<()> {
    // TODO: use tracing to log
    println!("\nBRIDGE\n");

    // use IP 0.0.0.0 so that it is possible to receive any traffic directed to port 8080
    let listener = TcpListener::bind("0.0.0.0:8080").await?;
    println!("WebSocket server listening on 0.0.0.0:8080");
    let (stream, _) = listener.accept().await?;
    let mut ws_stream = accept_async(stream).await?;

    println!("-------------------------------");
    println!("waiting for browser connections");

    // open the browser on the following address
    let browser_addr = "127.0.0.1:9090";
    let browser_listener = TcpListener::bind(browser_addr).await?;

    println!("-------------------------------");
    println!("handle browser connections");

    // map used to keep track of all the connections
    let (mut connections, mut rx) = Connections::new();

    // wait for one of the possible events:
    // - a new connection has been accepted
    // - the device sent data on the websocket channel, which must be forwarded to the correct connection
    // - the browser connection has data available, which must be forwarded to the device
    loop {
        match select_bridge(&browser_listener, &mut rx, &mut ws_stream).await {
            Receive::Connection(connection) => {
                let (browser_stream, addr) = connection?;
                handle_new_connection(
                    browser_stream,
                    &addr,
                    String::from("HOST"), // TODO: use non-static host
                    &mut connections,
                    &mut ws_stream,
                )
                .await;
            }
            Receive::Tcp(transmitted) => {
                recv_tcp(transmitted, &mut ws_stream).await;
            }
            Receive::Ws(msg) => {
                recv_ws(msg.unwrap().unwrap(), &mut connections).await;
            }
        }
    }
}

enum Receive {
    Connection(std::io::Result<(TcpStream, SocketAddr)>),
    Tcp(Option<Transmitted>),
    Ws(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
}

async fn select_bridge(
    browser_listener: &TcpListener,
    rx: &mut UnboundedReceiver<Transmitted>,
    ws_stream: &mut WebSocketStream<TcpStream>,
) -> Receive {
    select! {
        connection = browser_listener.accept() => Receive::Connection(connection),
        data = rx.recv() => Receive::Tcp(data),
        msg = ws_stream.next() => Receive::Ws(msg),
    }
}

#[instrument(skip_all)]
async fn handle_new_connection(
    browser_stream: TcpStream,
    addr: &SocketAddr,
    host: String,
    connections: &mut Connections,
    ws_stream: &mut WebSocketStream<TcpStream>,
) {
    // if case the inseriton of a new connections fails

    let id = Id {
        port: addr.port(),
        host: Arc::new(host),
    };
    connections.add_connection(browser_stream, id.clone()).await;

    info!("connection accepted: {}", id);

    // communicate to the device that a new connection has been establishedother
    let msg = WsTransmitted {
        id,
        msg: WsMsg::NewConnection,
    };

    let bytes = msg
        .encode()
        .expect("Failed to serialize WsTransmitted into items::WsTransmitted");

    ws_stream.send(Message::Binary(bytes)).await.unwrap();
}

#[instrument(skip_all)]
async fn recv_tcp<T>(data: Option<Transmitted>, ws_stream: &mut WebSocketStream<T>)
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    match data {
        Some(Transmitted { id, msg }) => {
            // close the connection if no data is sent
            let bridge_data = match msg {
                ConnMsg::Data(data) => {
                    info!("{}: {} bytes received from TCP connection", id, data.len());
                    WsTransmitted {
                        id,
                        msg: WsMsg::Data(data),
                    }
                }
                ConnMsg::Eot => {
                    info!("eot received, closing connection {id}");

                    WsTransmitted {
                        id,
                        msg: WsMsg::Eot,
                    }
                }
                ConnMsg::Close => {
                    info!("tcp closed, closing connection {id}");

                    WsTransmitted {
                        id,
                        msg: WsMsg::CloseConnection,
                    }
                }
            };

            let bytes = bridge_data
                .encode()
                .expect("Failed to serialize WsTransmitted into items::WsTransmitted");
            let msg = Message::Binary(bytes);
            ws_stream
                .send(msg)
                .await
                .expect("failed to send data on websocket toward device");
        }
        // rx hand side of the channel read None, therefore the connection has been closed
        None => {
            warn!("rx hand side of the channel read None, all connections have been closed");
        }
    }
}

#[instrument(skip_all)]
async fn recv_ws(msg: Message, connections: &mut Connections) {
    if let Err(err) = connections.handle_msg(msg).await {
        error!(?err);
    }
}

async fn main_device() {
    // TODO: use tracing to log
    println!("\nDEVICE\n");

    // use IP 0.0.0.0 so that it is possible to receive any traffic directed to port 8080
    let (mut ws_stream, _) = connect_async("ws://192.168.122.36:8080").await.unwrap();

    println!("-------------------------------");
    println!("waiting for browser connections");

    // map used to keep track of all the connections
    let (mut connections, mut rx) = Connections::new();

    loop {
        select! {
            msg = ws_stream.next() => {
                match msg {
                    Some(Ok(msg)) => recv_ws(msg, &mut connections).await,
                    Some(Err(err)) => error!(?err),
                    None => {
                        error!("stream closed");
                        break;
                    },
                }
            }
            id_data = rx.recv() => {
                recv_tcp(id_data, &mut ws_stream).await;
            }
        }
    }
}

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install().unwrap();
    tracing_subscriber::registry()
        .with(console_subscriber::spawn())
        .with(tracing_subscriber::fmt::layer())
        .with(EnvFilter::from_default_env())
        .try_init()?;

    match env::args().nth(1).as_deref() {
        Some("bridge") => main_bridge().await?,
        Some("device") => main_device().await,
        _ => panic!(),
    }

    Ok(())
}
