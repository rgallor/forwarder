use std::env;

use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use tokio::net::TcpListener;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf},
    net::TcpStream,
    select,
};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::WebSocketStream;
use tungstenite::Message;

async fn main_host() {
    println!("\nHOST\n");

    let listener = TcpListener::bind("0.0.0.0:8080").await.unwrap();
    println!("WebSocket server listening on 0.0.0.0:8080");
    let (stream, _) = listener.accept().await.unwrap();
    let ws_stream = accept_async(stream).await.unwrap();
    let (mut ws_sink, mut ws_stream) = ws_stream.split();

    println!("waiting for browser connection");

    // wait user connection (from browser)
    let ttyd_addr = "127.0.0.1:9090";
    let ttyd_listener = TcpListener::bind(ttyd_addr).await.unwrap();
    let (ttyd_stream, _) = ttyd_listener.accept().await.unwrap(); // http://127.0.0.1:9090
    let (mut ttyd_reader, mut ttyd_writer) = tokio::io::split(ttyd_stream);

    loop {
        select! {
            // read from (TTYD) webpage and send through websocket
            _ = tcp_to_ws(&mut ttyd_reader, &mut ws_sink) => {}
            // read from websocket and send to TTDY webpage
            _ = ws_to_tcp(&mut ttyd_writer, &mut ws_stream) => {}
        }
    }
}

async fn main_device() {
    println!("\nDEVICE\n");

    let url = "ws://192.168.1.24:8080";
    let (ws_stream, _) = connect_async(url).await.expect("Failed to connect.");
    let (mut ws_sink, mut ws_stream) = ws_stream.split();

    // wait 1st message
    let msg = ws_stream.next().await.unwrap().unwrap();

    println!("connecting to ttyd");

    let ttyd_addr = "127.0.0.1:7681"; // TTYD
    let ttdy_stream = tokio::net::TcpStream::connect(ttyd_addr).await.unwrap();
    let (mut ttyd_reader, mut ttyd_writer) = tokio::io::split(ttdy_stream);

    ttyd_writer.write_all(&msg.into_data()).await.unwrap();
    ttyd_writer.flush().await.unwrap();

    loop {
        select! {
            // read from TTDY and send to WS
            _ = tcp_to_ws(&mut ttyd_reader, &mut ws_sink) => {}
            // wait for messages from WS and write them on TTYD
            _ = ws_to_tcp(&mut ttyd_writer, &mut ws_stream) => {}
        }
    }
}

async fn tcp_to_ws<S>(
    tcp_reader: &mut ReadHalf<TcpStream>,
    ws_sink: &mut SplitSink<WebSocketStream<S>, Message>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf = [0; 1024];

    println!("RECEIVED MESSAGE FROM TCP");
    let n = tcp_reader.read(&mut buf).await.unwrap();
    println!("{n}");
    ws_sink
        .send(Message::Binary(buf[0..n].into()))
        .await
        .unwrap();
    println!("MESSAGE FORWARDED TO WS");
}

async fn ws_to_tcp<S>(
    tcp_writer: &mut WriteHalf<TcpStream>,
    ws_stream: &mut SplitStream<WebSocketStream<S>>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let msg = ws_stream.next().await.unwrap().unwrap();
    println!("RECEIVED MESSAGE FROM WS");
    if let Message::Binary(bytes) = msg {
        tcp_writer.write_all(&bytes).await.unwrap();
        tcp_writer.flush().await.unwrap();
        println!("MESSAGE FORWARDED TO TCP");
    }
}

#[tokio::main]
async fn main() {
    let args = env::args().nth(1).expect("argument");

    match args.as_str() {
        "host" => main_host().await,
        "device" => main_device().await,
        _ => panic!(),
    }
}
