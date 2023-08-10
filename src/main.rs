use std::{env, ops::ControlFlow, sync::Arc};

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

async fn main_bridge() {
    println!("\nBRIDGE\n");

    let listener = TcpListener::bind("0.0.0.0:8080").await.unwrap();
    println!("WebSocket server listening on 0.0.0.0:8080");
    let (stream, _) = listener.accept().await.unwrap();
    let ws_stream = accept_async(stream).await.unwrap();
    let (ws_sink, ws_stream) = ws_stream.split();

    let ws_sink = Arc::new(Mutex::new(ws_sink));
    let ws_stream = Arc::new(Mutex::new(ws_stream));

    println!("waiting for browser connection");

    // wait user connection (from browser)
    let ttyd_addr = "127.0.0.1:9090";
    let ttyd_listener = TcpListener::bind(ttyd_addr).await.unwrap();

    // handle connections from http://127.0.0.1:9090
    let (ttyd_stream, _) = ttyd_listener.accept().await.unwrap();
    println!("\nCONNECTION ACCEPTED\n");

    let (ttyd_reader, ttyd_writer) = tokio::io::split(ttyd_stream);

    let ws_sink_clone = Arc::clone(&ws_sink);
    let ws_stream_clone = Arc::clone(&ws_stream);

    let handle = tokio::spawn(handle_bridge_device_connection(
        ttyd_reader,
        ttyd_writer,
        ws_sink_clone,
        ws_stream_clone,
    ));

    handle.await.unwrap();

    // upgrade communication to ws
    let (ttyd_stream, _) = ttyd_listener.accept().await.unwrap();
    let ws_ttyd_stream = accept_async(ttyd_stream).await.unwrap();
    println!("\nCONNECTION ACCEPTED\n");
    let (ws_ttyd_writer, ws_ttyd_reader) = ws_ttyd_stream.split();

    let ws_ttyd_writer = Arc::new(Mutex::new(ws_ttyd_writer));
    let ws_ttyd_reader = Arc::new(Mutex::new(ws_ttyd_reader));

    let handle = tokio::spawn(async move {
        loop {
            select! {
                // read from (TTYD) webpage and send through websocket
                ctrlf = ws_forward(ws_ttyd_reader.clone(), ws_sink.clone()) => {
                    match ctrlf {
                        ControlFlow::Continue(()) => {}
                        ControlFlow::Break(()) => break,
                    }
                }
                // read from websocket and send to TTDY webpage
                ctrlf = ws_forward(ws_stream.clone(), ws_ttyd_writer.clone()) => {
                    match ctrlf {
                        ControlFlow::Continue(()) => {}
                        ControlFlow::Break(()) => break,
                    }
                }
            }
        }

        println!("CONNECTION TERMINATED");
    });

    handle
        .await
        .expect("failed to handle ws between browser and bridge");
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

async fn main_device() {
    println!("\nDEVICE\n");

    let url = "ws://192.168.1.24:8080"; // ws://IP_ADDR_VM:PORT_WS_CONN
    let (ws_stream, _) = connect_async(url).await.expect("Failed to connect.");
    let (ws_sink, ws_stream) = ws_stream.split();

    let ws_sink = Arc::new(Mutex::new(ws_sink));
    let ws_stream = Arc::new(Mutex::new(ws_stream));

    // wait 1st message (so to synchronize with the host before starting the interaction with TTYD)
    let msg = ws_stream.lock().await.next().await.unwrap().unwrap();

    println!("connecting to ttyd");

    let ttyd_addr = "127.0.0.1:7681"; // TTYD
    let ttdy_stream = tokio::net::TcpStream::connect(ttyd_addr).await.unwrap();
    let (mut ttyd_reader, mut ttyd_writer) = tokio::io::split(ttdy_stream);

    ttyd_writer.write_all(&msg.into_data()).await.unwrap();
    ttyd_writer.flush().await.unwrap();

    loop {
        select! {
            // read from TTDY and send to WS
            ctrlf = tcp_to_ws(&mut ttyd_reader, ws_sink.clone()) => {
                match ctrlf {
                    ControlFlow::Continue(()) => {}
                    ControlFlow::Break(()) => break,
                }
            }
            // wait for messages from WS and write them on TTYD
            ctrlf = ws_to_tcp(&mut ttyd_writer, ws_stream.clone()) => {
                match ctrlf {
                    ControlFlow::Continue(()) => {}
                    ControlFlow::Break(()) => break,
                }
            }
        }
    }
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

#[tokio::main]
async fn main() {
    let args = env::args().nth(1).expect("argument");

    match args.as_str() {
        "bridge" => main_bridge().await,
        "device" => main_device().await,
        _ => panic!(),
    }
}
