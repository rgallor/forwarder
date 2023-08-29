use std::{collections::HashMap, env, io::Error, sync::Arc};

use futures_util::{stream::StreamExt, SinkExt};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{tcp::OwnedWriteHalf, TcpStream},
    select,
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
};
use tokio::{net::TcpListener, sync::Mutex};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::connect_async;
use tungstenite::Message;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
enum BridgeMsg {
    NewConnection(usize),
    CloseConnection(usize),
    Data(usize, Vec<u8>),
}

#[derive(Serialize, Deserialize, Debug)]
enum DeviceMsg {
    Reply(usize, Vec<u8>),
    CloseConnection(usize),
}

struct Connection {
    id: usize,
    tcp_writer: OwnedWriteHalf,
    reader_handle: JoinHandle<()>,
}

impl Connection {
    fn new(id: usize, tcp_stream: TcpStream, tx: UnboundedSender<(usize, Vec<u8>)>) -> Self {
        let (mut tcp_reader, tcp_writer) = tcp_stream.into_split();

        // spawn a task responsible for notifying when new data is available on the TCP reader
        let reader_handle = tokio::spawn(async move {
            let mut buf = [0; 1024];

            while let Ok(()) = tcp_reader.readable().await {
                let n = tcp_reader.read(&mut buf).await.unwrap();
                tx.send((id, buf[0..n].to_vec()))
                    .expect("error while sending on channel");
            }

            println!("TCP reader not readable anymore");
        });

        Self {
            id,
            tcp_writer,
            reader_handle,
        }
    }

    async fn close(&mut self) -> Result<(), Error> {
        // sending CloseConnection msg to bridge
        let close_msg = BridgeMsg::CloseConnection(self.id);
        let bytes = bson::to_vec(&close_msg).expect("failed to serialize COnnectionClose message");

        self.tcp_writer
            .write_all(&bytes)
            .await
            .expect("error while writing data on tcp writer");

        // close the writer half of the TCP stream and flush any remaining data in the writer before closing
        self.tcp_writer.shutdown().await?;
        self.tcp_writer.flush().await?;

        // abort the task responsible for sending TCP data to the
        self.reader_handle.abort();

        Ok(())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // call the close method to gracefully close the TCP connection
        if let Err(err) = tokio::runtime::Handle::current().block_on(self.close()) {
            eprintln!("Error while closing connection: {:?}", err);
        }

        println!("closing connection {}", self.id);
    }
}

#[derive(Clone)]
struct Connections {
    id_count: usize,
    connections: Arc<Mutex<HashMap<usize, Connection>>>,
    tx: UnboundedSender<(usize, Vec<u8>)>,
    rx: Arc<Mutex<UnboundedReceiver<(usize, Vec<u8>)>>>,
}

impl Connections {
    fn new() -> Self {
        // this channel is used by tasks associated to each connection to communicate new
        // information available on a given tcp reader handle (associated to an usize connection ID).
        // it is also used to forward the incoming data to the device over the websocket connection
        let (tx, rx) = mpsc::unbounded_channel::<(usize, Vec<u8>)>();
        let rx = Arc::new(Mutex::new(rx));

        Self {
            id_count: 1,
            connections: Arc::new(Mutex::new(HashMap::new())),
            tx,
            rx,
        }
    }

    // insertion of a new connection given a tcp_stream
    async fn new_connection(&mut self, tcp_stream: TcpStream) -> Option<usize> {
        let id = self.id_count;
        let tx = self.tx.clone();
        let connection = Connection::new(id, tcp_stream, tx);

        // because the id_count is internally managed, this function should always return Some(id)
        // otherwise it would mean that a new connection with the same ID of an existing one is openned
        match self.connections.lock().await.insert(id, connection) {
            None => {
                // increment the id_count for next insertions
                self.id_count
                    .checked_add(1)
                    .expect("overflow occurred when incrementing connection ID");
                Some(id)
            }
            _ => None,
        }
    }

    async fn forward_to_browser(&mut self, msg: Message) -> Result<(), ()> {
        match msg {
            Message::Binary(bytes) => {
                let device_msg: DeviceMsg =
                    bson::from_slice(&bytes).expect("failed to deserialize");

                match device_msg {
                    // handle the reception of new data by forwarding them through the TCP connection
                    DeviceMsg::Reply(id, data) => {
                        let mut connections = self.connections.lock().await;
                        let connection = match connections.get_mut(&id) {
                            Some(conn) => conn,
                            None => return Err(()),
                        };

                        connection
                            .tcp_writer
                            .write_all(&data)
                            .await
                            .expect("error while writing data on tcp writer");
                    }
                    // handle the closure of a connection
                    DeviceMsg::CloseConnection(id) => {
                        // removing the connection from the hashmap will automatically call the drop() method
                        // on the connection, which has been implemented to gracefully shut down the TCP connection
                        if self.connections.lock().await.remove(&id).is_none() {
                            return Err(()); // this occurs in case there no exist a connection with the provided id
                        }
                    }
                }
            }
            // wrong Message type
            _ => return Err(()),
        }

        Ok(())
    }

    async fn recv_from_tcp(&mut self) -> Option<(usize, Vec<u8>)> {
        self.rx.lock().await.recv().await
    }
}

async fn main_bridge() {
    // TODO: use tracing to log
    println!("\nBRIDGE\n");

    // use IP 0.0.0.0 so that it is possible to receive any traffic directed to port 8080
    let listener = TcpListener::bind("0.0.0.0:8080").await.unwrap();
    println!("WebSocket server listening on 0.0.0.0:8080");
    let (stream, _) = listener.accept().await.unwrap();
    let ws_stream = accept_async(stream).await.unwrap();
    let (ws_sink, mut ws_stream) = ws_stream.split();
    let ws_sink = Arc::new(Mutex::new(ws_sink));

    println!("-------------------------------");
    println!("waiting for browser connections");

    // open the browser on the following address
    let ttyd_addr = "127.0.0.1:9090";
    let ttyd_listener = TcpListener::bind(ttyd_addr).await.unwrap();

    println!("-------------------------------");
    println!("handle browser connections");

    // map used to keep track of all the connections
    let mut connections = Connections::new();

    let ws_sink_clone = Arc::clone(&ws_sink);
    let mut connections_clone = connections.clone();

    // spawn a task responsible for the handling of new connections
    let new_connections_handle = tokio::spawn(async move {
        while let Ok((browser_stream, _)) = ttyd_listener.accept().await {
            // if case the inseriton of a new connections fails
            let id = match connections_clone.new_connection(browser_stream).await {
                Some(id) => id,
                None => panic!("failed to add connection to hashmap"),
            };

            println!("connection accepted: {id}");

            // communicate to the device that a new connection has been established
            let msg = BridgeMsg::NewConnection(id);
            let bytes = bson::to_vec(&msg).expect("failed to serialize BridgeMsg");
            ws_sink_clone
                .lock()
                .await
                .send(Message::Binary(bytes))
                .await
                .unwrap();
        }
    });

    // wait for one of the possible events:
    // - the device sent data on the websocket channel, which must be forwarded to the correct connection
    // - the browser connection has data available, which must be forwarded to the device
    loop {
        select! {
            msg = ws_stream.next() => {
                println!("msg received from WS");
                let cloned_msg = msg.unwrap().unwrap().clone();
                connections.forward_to_browser(cloned_msg).await.expect("error while forwarding the message to browser");
            }
            res = connections.recv_from_tcp() => {
                match res {
                    Some((id, data)) => {
                        println!("{}: {} bytes received from TCP connection", id, data.len());

                        let bridge_data = BridgeMsg::Data(id, data);
                        let bytes = bson::to_vec(&bridge_data).expect("failed to serialize bridge data");
                        let msg = Message::Binary(bytes);
                        ws_sink.lock().await.send(msg).await.expect("failed to send data on websocket toward device");
                    }
                    None => break
                }
            }
        }
    }

    // before exiting
    new_connections_handle.abort();
}

async fn main_device() {
    println!("\nDEVICE\n");

    // open a websocket connection with the bridge
    let url = "ws://192.168.122.36:8080"; // ws://IP_ADDR_VM:PORT_WS_CONN
    let (ws_stream, _) = connect_async(url).await.expect("Failed to connect.");
    let (mut ws_sink, mut ws_stream) = ws_stream.split();

    println!("------------------");
    println!("connecting to ttyd");

    let ttyd_addr = "127.0.0.1:7681"; // TTYD
    let ttdy_stream = tokio::net::TcpStream::connect(ttyd_addr).await.unwrap();
    let (mut ttyd_reader, mut ttyd_writer) = tokio::io::split(ttdy_stream);

    println!("------------------");
    println!("handling connections");

    // define a collection of connections, each one associated with an ID and a status (open = true, closed = false)
    let mut connections: HashMap<usize, bool> = HashMap::new();

    // wait 1st connection (so to synchronize with the host before starting the interaction with TTYD)

    // listens for Bridge messages and handles the connections
    while let Some(res) = ws_stream.next().await {
        let msg = res.expect("Tung error while receiving on ws stream");

        let bridge_msg: BridgeMsg = match msg {
            Message::Binary(bytes) => {
                bson::from_slice(bytes.as_slice()).expect("failed to exec BSON deserialize")
            }
            _ => panic!("wrong tungstenite message type received"),
        };

        match bridge_msg {
            BridgeMsg::NewConnection(id) => {
                // check if the connection has already been opened, eventually printing an error.
                // if not, store a new connection in the hashmap and set its status as open (true)
                if let Some(_val) = connections.insert(id, true) {
                    eprintln!("connection already existent");
                }

                println!("new connection: {id}");
            }
            BridgeMsg::CloseConnection(id) => {
                // check if the connection exists. if not, it means it has never been opened -> print error.
                // if it exists, check if the status is true (still open) or false (closed).
                // if open, close it gracefully sending a CloseConnection DeviceMsg to the bridge, otherwise print error
                match connections.get_mut(&id) {
                    None => eprintln!("connection {id} does not exists"),
                    Some(status) if !*status => {
                        eprintln!("connection {id} has already been closed")
                    }
                    Some(status) => {
                        *status = false;

                        // send a DeviceMsg to the bridge telling that the conneciton has been properly closed
                        let msg = DeviceMsg::CloseConnection(id);
                        let bytes = bson::to_vec(&msg).expect("failed to serialize to bson");
                        ws_sink
                            .send(Message::Binary(bytes))
                            .await
                            .expect("error while sending message on the device-bridge ws");

                        println!("connection {id} closed");
                    }
                }
            }
            // forward the traffic to TTYD, wait response data from TTYD, and forward it to the bridge via ws
            BridgeMsg::Data(id, data) => {
                // check if the connection exists, eventually printing an error and returning
                match connections.get_mut(&id) {
                    None => eprintln!("connection {id} does not exists"),
                    Some(status) if !*status => {
                        eprintln!("connection {id} has already been closed")
                    }
                    Some(status) => {
                        // forward the traffic to TTYD
                        ttyd_writer.write_all(data.as_slice()).await.unwrap();
                        ttyd_writer.flush().await.unwrap();

                        println!("{}: {} bytes forwarded onto TCP connection", id, data.len());

                        // TODO: maybe TTYD doesn't send back any data. In that case, 0 bytes are read and the
                        // connection is closed, even though it shouldn't.
                        // (e.g., before being able to receive data from TTYD, it may be necessary to receive more
                        //  than one Bridge::Data message).

                        // wait data from TTYD
                        let mut buf = [0; 1024];
                        while let Ok(n) = ttyd_reader.read(&mut buf).await {
                            // the socket has been closed (connection closed normally).
                            // it should occur in case the connection between the device and TTYD is terminated
                            // or when no data is temporarily available.
                            // TODO: check that, in case no data is temporarily available but the connection is still open there is no deadlock condition.
                            if n == 0 {
                                // update the connection status to closed (false) and communicate it to the bridge.
                                *status = false;

                                // send a DeviceMsg to the bridge telling that the conneciton has been properly closed
                                let msg = DeviceMsg::CloseConnection(id);
                                let bytes =
                                    bson::to_vec(&msg).expect("failed to serialize to bson");
                                ws_sink
                                    .send(Message::Binary(bytes))
                                    .await
                                    .expect("error while sending message on the device-bridge ws");

                                println!("{id}: 0 bytes read, closing connection");

                                break;
                            }

                            // forward the data to the bridge via ws
                            let msg = DeviceMsg::Reply(id, buf[0..n].into());
                            let bytes =
                                bson::to_vec(&msg).expect("failed to serialize DeviceMsg to BSON");
                            ws_sink.send(Message::Binary(bytes)).await.unwrap();

                            println!("{}: {} bytes read and sent onto WS", id, data.len());
                        }
                    }
                }
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let args = env::args().nth(1).expect("argument not found");

    match args.as_str() {
        "bridge" => main_bridge().await,
        "device" => main_device().await,
        _ => panic!(),
    }
}
