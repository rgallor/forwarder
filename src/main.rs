use std::{collections::HashMap, env, io::Error, sync::Arc};

use futures_util::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{tcp::WriteHalf, TcpStream},
    select,
    sync::mpsc::{self, UnboundedSender},
    task::JoinHandle,
};
use tokio::{net::TcpListener, sync::Mutex};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::connect_async;
use tungstenite::Message;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
enum BridgeMsg {
    NewConnection(u32),
    CloseConnection(u32),
    Data(u32, Vec<u8>),
}

#[derive(Serialize, Deserialize, Debug)]
enum DeviceMsg {
    Reply(u32, Vec<u8>),
    CloseConnection(u32),
}

struct Connection<'a> {
    id: u32,
    tcp_writer: WriteHalf<'a>,
    reader_handle: JoinHandle<()>,
}

impl Connection<'_> {
    fn new(id: u32, mut tcp_stream: TcpStream, tx: UnboundedSender<(u32, Vec<u8>)>) -> Self {
        let (tcp_reader, tcp_writer) = tcp_stream.split();

        // spawn a task responsible for notifying when new data are available on the TCP reader
        let reader_handle = tokio::spawn(async move {
            let mut buf = [0; 1024];

            while let Ok(()) = tcp_reader.readable().await {
                let n = tcp_reader.read(&mut buf).await.unwrap();
                tx.send((id, buf[0..n].to_vec()))
                    .expect("error while sending on channel");
            }
        });

        Self {
            id,
            tcp_writer,
            reader_handle,
        }
    }

    async fn close(&mut self) -> Result<(), Error> {
        // close the writer half of the TCP stream
        self.tcp_writer.shutdown().await?;

        // flush any remaining data in the writer before closing
        self.tcp_writer.flush().await?;

        // abort the task responsible for
        self.reader_handle.abort();

        Ok(())
    }
}

impl Drop for Connection<'_> {
    fn drop(&mut self) {
        // call the close method to gracefully close the TCP connection
        if let Err(err) = tokio::runtime::Handle::current().block_on(self.close()) {
            eprintln!("Error while closing connection: {:?}", err);
        }
    }
}

struct Connections<'a> {
    id_count: u32,
    connections: HashMap<u32, Connection<'a>>,
}

impl Connections<'_> {
    fn new() -> Self {
        Self {
            id_count: 1,
            connections: HashMap::new(),
        }
    }

    // insertion of a new connection given a tcp_stream
    fn new_connection(
        &mut self,
        tcp_stream: TcpStream,
        tx: UnboundedSender<(u32, Vec<u8>)>,
    ) -> Option<u32> {
        let id = self.id_count;
        let connection = Connection::new(id, tcp_stream, tx);

        // because the id_count is internally managed, this function should always return Some(id)
        // otherwise it would mean that a new connection with the same ID of an existing one is openned
        match self.connections.insert(id, connection) {
            None => {
                // increment the id_count for next insertions
                self.id_count += 1;
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
                        let connection = match self.connections.get_mut(&id) {
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
                        // TODO: check that at the device side the connection has been closed (set to false on the hashmap)
                        if let None = self.connections.remove(&id) {
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
}

async fn main_bridge() {
    // TODO: use tracing to log
    println!("\nBRIDGE\n");

    // use IP 0.0.0.0 so that it is possible to receive any traffic directed to port 8080
    let listener = TcpListener::bind("0.0.0.0:8080").await.unwrap();
    println!("WebSocket server listening on 0.0.0.0:8080");
    let (stream, _) = listener.accept().await.unwrap();
    let ws_stream = accept_async(stream).await.unwrap();
    let (ws_sink, ws_stream) = ws_stream.split();

    let ws_sink = Arc::new(Mutex::new(ws_sink));
    let ws_stream = Arc::new(Mutex::new(ws_stream));

    println!("-------------------------------");
    println!("waiting for browser connections");

    // open the browser on the following address
    let ttyd_addr = "127.0.0.1:9090";
    let ttyd_listener = TcpListener::bind(ttyd_addr).await.unwrap();

    // map used to keep track of all the connections
    let connections = Arc::new(Mutex::new(Connections::new()));

    // this channel is used by tasks associated to each connection to communicate the availability
    // of new information on a given tcp reader handle (associated to an u32 connection ID).
    // it is also used by the current task to forward the incoming data to the device over the websocket connection
    let (tx, mut rx) = mpsc::unbounded_channel::<(u32, Vec<u8>)>();

    // spawn a task responsible for the handling of new connections
    let ws_sink_clone = Arc::clone(&ws_sink);
    let connections_clone = Arc::clone(&connections);

    let new_connections_handle = tokio::spawn(async move {
        while let Ok((browser_stream, _)) = ttyd_listener.accept().await {
            println!("\nCONNECTION ACCEPTED\n");

            // if case the inseriton of a new connections fails
            let id = match connections_clone
                .lock()
                .await
                .new_connection(browser_stream, tx.clone())
            {
                Some(id) => id,
                None => panic!("failed to add connection to hashmap"),
            };

            // communicate to the device that a new connection has been established
            let msg = BridgeMsg::NewConnection(id);
            let msg_ser = bson::to_vec(&msg).expect("failed to serialize BridgeMsg");
            ws_sink_clone
                .lock()
                .await
                .send(Message::Binary(msg_ser))
                .await
                .unwrap();
        }
    });

    // wait for one of the possible events:
    // - the device sent data on the websocket channel, which must be forwarded to the correct connection
    // - the browser connection has data available, which must be forwarded to the device

    loop {
        select! {
            msg = ws_stream.lock().await.next() => {
                let cloned_msg = msg.unwrap().unwrap().clone();
                connections.lock().await.forward_to_browser(cloned_msg).await.expect("error while forwarding the message to browser");
            }
            res = rx.next() => {
                match res {
                    Some((id, data)) => {
                        let bridge_data = BridgeMsg::Data(id, data);
                        let bytes = bson::to_vec(&bridge_data).expect("failed to serialize bridge data");
                        let msg = Message::Binary(bytes);
                        ws_sink.lock().await.send(msg);
                    }
                    None => break
                }
            }
        }
    }

    // before exiting
    new_connections_handle.abort();

    // let (ttyd_reader, ttyd_writer) = tokio::io::split(browser_stream);

    // // handle connections from http://127.0.0.1:9090
    // let (ttyd_stream, _) = ttyd_listener.accept().await.unwrap();
    // println!("\nCONNECTION ACCEPTED\n");

    // let ws_sink_clone = Arc::clone(&ws_sink);
    // let ws_stream_clone = Arc::clone(&ws_stream);

    // let handle = tokio::spawn(handle_bridge_device_connection(
    //     ttyd_reader,
    //     ttyd_writer,
    //     ws_sink_clone,
    //     ws_stream_clone,
    // ));

    // handle.await.unwrap();

    // // upgrade communication to ws
    // let (ttyd_stream, _) = ttyd_listener.accept().await.unwrap();
    // let ws_ttyd_stream = accept_async(ttyd_stream).await.unwrap();
    // println!("\nCONNECTION ACCEPTED\n");
    // let (ws_ttyd_writer, ws_ttyd_reader) = ws_ttyd_stream.split();

    // let ws_ttyd_writer = Arc::new(Mutex::new(ws_ttyd_writer));
    // let ws_ttyd_reader = Arc::new(Mutex::new(ws_ttyd_reader));

    // let handle = tokio::spawn(async move {
    //     loop {
    //         select! {
    //             // read from (TTYD) webpage and send through websocket
    //             ctrlf = ws_forward(ws_ttyd_reader.clone(), ws_sink.clone()) => {
    //                 match ctrlf {
    //                     ControlFlow::Continue(()) => {}
    //                     ControlFlow::Break(()) => break,
    //                 }
    //             }
    //             // read from websocket and send to TTDY webpage
    //             ctrlf = ws_forward(ws_stream.clone(), ws_ttyd_writer.clone()) => {
    //                 match ctrlf {
    //                     ControlFlow::Continue(()) => {}
    //                     ControlFlow::Break(()) => break,
    //                 }
    //             }
    //         }
    //     }

    //     println!("CONNECTION TERMINATED");
    // });

    // handle
    //     .await
    //     .expect("failed to handle ws between browser and bridge");
}

// async fn ws_forward<S>(
//     ws_stream: Arc<Mutex<SplitStream<WebSocketStream<S>>>>,
//     ws_sink: Arc<Mutex<SplitSink<WebSocketStream<S>, Message>>>,
// ) -> ControlFlow<()>
// where
//     S: AsyncRead + AsyncWrite + Unpin,
// {
//     let mut ws_stream = ws_stream.lock().await;
//     let res = ws_stream.next().await.unwrap();

//     match res {
//         Ok(msg) => {
//             ws_sink.lock().await.send(msg).await.unwrap();
//             ControlFlow::Continue(())
//         }
//         Err(err) => {
//             eprintln!("WS error: {}", err);
//             ControlFlow::Break(())
//         }
//     }
// }

// async fn handle_bridge_device_connection<S>(
//     mut ttyd_reader: ReadHalf<TcpStream>,
//     mut ttyd_writer: WriteHalf<TcpStream>,
//     ws_sink: Arc<Mutex<SplitSink<WebSocketStream<S>, Message>>>,
//     ws_stream: Arc<Mutex<SplitStream<WebSocketStream<S>>>>,
// ) where
//     S: AsyncRead + AsyncWrite + Unpin,
// {
//     loop {
//         select! {
//             // read from (TTYD) webpage and send through websocket
//             ctrlf = tcp_to_ws(&mut ttyd_reader, ws_sink.clone()) => {
//                 match ctrlf {
//                     ControlFlow::Continue(()) => {}
//                     ControlFlow::Break(()) => break,
//                 }
//             }
//             // read from websocket and send to TTDY webpage
//             ctrlf = ws_to_tcp(&mut ttyd_writer, ws_stream.clone()) => {
//                 match ctrlf {
//                     ControlFlow::Continue(()) => {}
//                     ControlFlow::Break(()) => break,
//                 }
//             }
//         }
//     }

//     println!("CONNECTION TERMINATED");
// }

// async fn tcp_to_ws<S>(
//     tcp_reader: &mut ReadHalf<TcpStream>,
//     ws_sink: Arc<Mutex<SplitSink<WebSocketStream<S>, Message>>>,
// ) -> ControlFlow<()>
// where
//     S: AsyncRead + AsyncWrite + Unpin,
// {
//     let mut buf = [0; 1024];
//     let n = tcp_reader.read(&mut buf).await.unwrap();

//     println!("{n}");

//     // the socket has been closed (connection closed normally)
//     if n == 0 {
//         return ControlFlow::Break(());
//     }

//     ws_sink
//         .lock()
//         .await
//         .send(Message::Binary(buf[0..n].into()))
//         .await
//         .unwrap();

//     ControlFlow::Continue(())
// }

// async fn ws_to_tcp<S>(
//     tcp_writer: &mut WriteHalf<TcpStream>,
//     ws_stream: Arc<Mutex<SplitStream<WebSocketStream<S>>>>,
// ) -> ControlFlow<()>
// where
//     S: AsyncRead + AsyncWrite + Unpin,
// {
//     let mut ws_stream = ws_stream.lock().await;
//     let res = ws_stream.next().await.unwrap();

//     match res {
//         Ok(msg) => {
//             if let Message::Binary(bytes) = msg {
//                 tcp_writer.write_all(&bytes).await.unwrap();
//                 tcp_writer.flush().await.unwrap();
//             }
//             ControlFlow::Continue(())
//         }
//         Err(err) => {
//             eprintln!("WS error: {}", err);
//             ControlFlow::Break(())
//         }
//     }
// }

async fn main_device() {
    println!("\nDEVICE\n");

    // open a websocket connection with the bridge
    let url = "ws://192.168.1.24:8080"; // ws://IP_ADDR_VM:PORT_WS_CONN
    let (ws_stream, _) = connect_async(url).await.expect("Failed to connect.");
    let (mut ws_sink, mut ws_stream) = ws_stream.split();

    println!("------------------");
    println!("connecting to ttyd");

    let ttyd_addr = "127.0.0.1:7681"; // TTYD
    let ttdy_stream = tokio::net::TcpStream::connect(ttyd_addr).await.unwrap();
    let (mut ttyd_reader, mut ttyd_writer) = tokio::io::split(ttdy_stream);

    // define a collection of connections, each one associated with an ID and a status (open = true, closed = false)
    let mut connections: HashMap<u32, bool> = HashMap::new();

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
                        // TODO: handle the reception of this message on the bridge side
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

                            // TODO: check that, in case no data is temporarily available but the connection is still open
                            // there is no deadlock condition

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

                                break;
                            }

                            // forward the data to the bridge via ws
                            let msg = DeviceMsg::Reply(id, buf[0..n].into());
                            let bytes =
                                bson::to_vec(&msg).expect("failed to serialize DeviceMsg to BSON");
                            ws_sink.send(Message::Binary(bytes)).await.unwrap();
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
