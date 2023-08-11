use std::{collections::HashMap, env, ops::ControlFlow, sync::Arc};

use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf},
    net::TcpStream,
    select,
};
use tokio::{net::TcpListener, sync::Mutex};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::WebSocketStream;
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

struct Connection {
    id: u32,
    tcp_writer: WriteHalf<TcpStream>,
    tcp_reader: ReadHalf<TcpStream>,
}

impl Connection {
    fn new(id: u32, tcp_stream: TcpStream) -> Self {
        let (tcp_reader, tcp_writer) = tokio::io::split(tcp_stream);

        Self {
            id,
            tcp_writer,
            tcp_reader,
        }
    }
}

async fn main_bridge() {
    // TODO: use tracing to log
    println!("\nBRIDGE\n");

    let listener = TcpListener::bind("0.0.0.0:8080").await.unwrap();
    println!("WebSocket server listening on 0.0.0.0:8080");
    let (stream, _) = listener.accept().await.unwrap();
    let ws_stream = accept_async(stream).await.unwrap();
    let (ws_sink, ws_stream) = ws_stream.split();

    let ws_sink = Arc::new(Mutex::new(ws_sink));
    let ws_stream = Arc::new(Mutex::new(ws_stream));

    println!("-------------------------------");
    println!("waiting for browser connections");

    let mut connections = HashMap::new();
    let mut id = 1u32;

    let ttyd_addr = "127.0.0.1:9090";
    let ttyd_listener = TcpListener::bind(ttyd_addr).await.unwrap();

    while let Ok((browser_stream, _)) = ttyd_listener.accept().await {
        println!("\nCONNECTION ACCEPTED\n");

        let connection = Connection::new(id, browser_stream);

        connections
            .insert(id, connection)
            .expect("failed to add connection to hashmap");

        // communicate to the device that a new connection has been established
        let msg = BridgeMsg::NewConnection(id);
        let msg_ser = bson::to_vec(&msg).unwrap();
        ws_sink
            .lock()
            .await
            .send(Message::Binary(msg_ser))
            .await
            .unwrap();

        // TODO: define something like: connection.handle() passing the Arc references to the ws between the bridge and the
        // device, handling both the reception and the sending of messages.
        // NB: the Message struct will carry binarized version of BridgeMsg and DeviceMsg
    }

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

async fn ws_forward<S>(
    ws_stream: Arc<Mutex<SplitStream<WebSocketStream<S>>>>,
    ws_sink: Arc<Mutex<SplitSink<WebSocketStream<S>, Message>>>,
) -> ControlFlow<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut ws_stream = ws_stream.lock().await;
    let res = ws_stream.next().await.unwrap();

    match res {
        Ok(msg) => {
            ws_sink.lock().await.send(msg).await.unwrap();
            ControlFlow::Continue(())
        }
        Err(err) => {
            eprintln!("WS error: {}", err);
            ControlFlow::Break(())
        }
    }
}

async fn handle_bridge_device_connection<S>(
    mut ttyd_reader: ReadHalf<TcpStream>,
    mut ttyd_writer: WriteHalf<TcpStream>,
    ws_sink: Arc<Mutex<SplitSink<WebSocketStream<S>, Message>>>,
    ws_stream: Arc<Mutex<SplitStream<WebSocketStream<S>>>>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        select! {
            // read from (TTYD) webpage and send through websocket
            ctrlf = tcp_to_ws(&mut ttyd_reader, ws_sink.clone()) => {
                match ctrlf {
                    ControlFlow::Continue(()) => {}
                    ControlFlow::Break(()) => break,
                }
            }
            // read from websocket and send to TTDY webpage
            ctrlf = ws_to_tcp(&mut ttyd_writer, ws_stream.clone()) => {
                match ctrlf {
                    ControlFlow::Continue(()) => {}
                    ControlFlow::Break(()) => break,
                }
            }
        }
    }

    println!("CONNECTION TERMINATED");
}

async fn tcp_to_ws<S>(
    tcp_reader: &mut ReadHalf<TcpStream>,
    ws_sink: Arc<Mutex<SplitSink<WebSocketStream<S>, Message>>>,
) -> ControlFlow<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf = [0; 1024];
    let n = tcp_reader.read(&mut buf).await.unwrap();

    println!("{n}");

    // the socket has been closed (connection closed normally)
    if n == 0 {
        return ControlFlow::Break(());
    }

    ws_sink
        .lock()
        .await
        .send(Message::Binary(buf[0..n].into()))
        .await
        .unwrap();

    ControlFlow::Continue(())
}

async fn ws_to_tcp<S>(
    tcp_writer: &mut WriteHalf<TcpStream>,
    ws_stream: Arc<Mutex<SplitStream<WebSocketStream<S>>>>,
) -> ControlFlow<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut ws_stream = ws_stream.lock().await;
    let res = ws_stream.next().await.unwrap();

    match res {
        Ok(msg) => {
            if let Message::Binary(bytes) = msg {
                tcp_writer.write_all(&bytes).await.unwrap();
                tcp_writer.flush().await.unwrap();
            }
            ControlFlow::Continue(())
        }
        Err(err) => {
            eprintln!("WS error: {}", err);
            ControlFlow::Break(())
        }
    }
}

async fn main_device() {
    println!("\nDEVICE\n");

    let url = "ws://192.168.1.24:8080"; // ws://IP_ADDR_VM:PORT_WS_CONN
    let (ws_stream, _) = connect_async(url).await.expect("Failed to connect.");
    let (mut ws_sink, mut ws_stream) = ws_stream.split();

    println!("------------------");
    println!("connecting to ttyd");

    let ttyd_addr = "127.0.0.1:7681"; // TTYD
    let ttdy_stream = tokio::net::TcpStream::connect(ttyd_addr).await.unwrap();
    let (mut ttyd_reader, mut ttyd_writer) = tokio::io::split(ttdy_stream);

    let mut connections: HashMap<u32, bool> = HashMap::new();

    // wait 1st connection (so to synchronize with the host before starting the interaction with TTYD)

    // TODO: listens for Bridge messages and handles the connections
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
                if let None = connections.insert(id, true) {
                    eprintln!("connection already existent")
                }
            }
            BridgeMsg::CloseConnection(id) => {
                // check if the connection exists. if not, it means it has never been opened -> print error.
                // if it exists, check if the status is true (still open) or false (closed).
                // if open, close it gracefully sending a CloseConnection DeviceMsg to the bridge, otherwise print error
                match connections.get_mut(&id) {
                    None => eprintln!("connection {id} does not exists"),
                    Some(status) if *status => {
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
                    Some(_status) => eprintln!("connection {id} has already been closed"),
                }
            }
            // forward the traffic to TTYD, wait response data from TTYD, and forward them to the bridge via ws
            BridgeMsg::Data(id, data) => {
                // check if the connection exists, eventually printing an error and returning
                match connections.get_mut(&id) {
                    None => eprintln!("connection {id} does not exists"),
                    Some(status) if *status => {
                        // forward the traffic to TTYD
                        ttyd_writer.write_all(data.as_slice()).await.unwrap();
                        ttyd_writer.flush().await.unwrap();

                        // wait data from TTYD
                        let mut buf = [0; 1024];
                        while let Ok(n) = ttyd_reader.read(&mut buf).await {
                            // the socket has been closed (connection closed normally)
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
                    Some(_status) => eprintln!("connection {id} has already been closed"),
                }
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let args = env::args().nth(1).expect("argument");

    match args.as_str() {
        "bridge" => main_bridge().await,
        "device" => main_device().await,
        _ => panic!(),
    }
}
