use std::collections::HashMap;
use std::fmt::Display;
use std::io;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use futures_util::SinkExt;
use prost::Message as ProtoMessage;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::error::SendError;
use tokio::task::JoinHandle;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    select,
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
};
use tokio_tungstenite::WebSocketStream;
use tracing::{debug, error, info, instrument, warn};
use tungstenite::Message;

use serde::{Deserialize, Serialize};

use crate::proto::proto;

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
pub struct WsTransmitted {
    id: Id,
    msg: WsMsg,
}

impl WsTransmitted {
    pub fn new(id: Id, msg: WsMsg) -> Self {
        Self { id, msg }
    }

    pub fn encode(self) -> Result<Vec<u8>, ()> {
        let mut id = proto::Id::default();
        id.port = self.id.port.into();
        id.host = self.id.host.to_string(); //.expect("failed to remove String from Arc");

        let mut msg = proto::WsMsg::default();
        msg.ws_msg_type = Some(self.msg.into());

        let mut transmitted_msg = proto::WsTransmitted::default();
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

    pub fn decode(bytes: &[u8]) -> Result<Self, ()> {
        let msg_transmitted = proto::WsTransmitted::decode(bytes).map_err(|_| ())?;
        WsTransmitted::try_from(msg_transmitted)
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum WsMsg {
    NewConnection,
    Eot,
    CloseConnection,
    Data(Vec<u8>),
}

pub struct Connections {
    connections: HashMap<Id, ConnectionHandle>,
    tx_ws: UnboundedSender<Transmitted>,
}

impl Connections {
    pub fn new() -> (Self, UnboundedReceiver<Transmitted>) {
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
    pub async fn add_connection(&mut self, tcp_stream: TcpStream, id: Id) {
        let (tx_con, rx_con) = mpsc::unbounded_channel();

        let tx_ws = self.tx_ws.clone();
        let connection = Connection::new(id.clone(), tcp_stream, tx_ws, rx_con).spawn(tx_con);

        // because the id_count is internally managed, this function should always return Some(id)
        // otherwise it would mean that a new connection with the same ID of an existing one is openned

        if self.connections.insert(id, connection).is_some() {
            error!("connection replaced");
        }
    }

    #[instrument(skip_all)]
    fn get_connection(&mut self, id: &Id) -> Option<&mut ConnectionHandle> {
        self.connections.get_mut(&id)
    }

    #[instrument(skip_all)]
    pub async fn handle_msg(&mut self, msg: Message) -> Result<(), SendError<ConnMsg>> {
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
                        let ttdy_stream = match TcpStream::connect(ttyd_addr).await {
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
                    WsMsg::Data(data) => match self.get_connection(&id) {
                        Some(connection) => connection.tx_con.send(ConnMsg::Data(data))?,
                        None => {
                            error!("connection {id} not found, discarding data");
                            return Ok(());
                        }
                    },
                    WsMsg::Eot => match self.get_connection(&id) {
                        Some(connection) => connection.tx_con.send(ConnMsg::Eot)?,
                        None => {
                            error!("connection {id} not found, discarding data");
                            return Ok(());
                        }
                    },
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

#[instrument(skip_all)]
pub async fn recv_tcp<T>(data: Option<Transmitted>, ws_stream: &mut WebSocketStream<T>)
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    match data {
        Some(transmitted) => {
            let (id, msg) = transmitted.into_inner();

            // close the connection if no data is sent
            let bridge_data = match msg {
                ConnMsg::Data(data) => {
                    info!("{}: {} bytes received from TCP connection", id, data.len());
                    WsTransmitted::new(id, WsMsg::Data(data))
                }
                ConnMsg::Eot => {
                    info!("eot received, closing connection {id}");
                    WsTransmitted::new(id, WsMsg::Eot)
                }
                ConnMsg::Close => {
                    info!("tcp closed, closing connection {id}");
                    WsTransmitted::new(id, WsMsg::CloseConnection)
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
pub async fn recv_ws(msg: Message, connections: &mut Connections) {
    if let Err(err) = connections.handle_msg(msg).await {
        error!(?err);
    }
}
