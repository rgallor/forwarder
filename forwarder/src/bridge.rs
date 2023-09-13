use std::net::SocketAddr;

use futures_util::{SinkExt, StreamExt};
use tokio::{
    net::{TcpListener, TcpStream},
    select,
    sync::mpsc::UnboundedReceiver,
};
use tokio_tungstenite::{accept_async, tungstenite::Error as TungError, WebSocketStream};
use tracing::{info, instrument};
use tungstenite::Message;

use crate::connection::{recv_ws, Connections, Id, Transmitted, WsMsg, WsTransmitted};

enum Receive {
    Connection(std::io::Result<(TcpStream, SocketAddr)>),
    Tcp(Option<Transmitted>),
    Ws(Option<Result<Message, TungError>>),
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
    let id = Id::new(addr.port(), host);
    connections.add_connection(browser_stream, id.clone()).await;

    info!("connection accepted: {}", id);

    // communicate to the device that a new connection has been establishedother
    let msg = WsTransmitted::new(id, WsMsg::NewConnection);

    let bytes = msg
        .encode()
        .expect("Failed to serialize WsTransmitted into items::WsTransmitted");

    ws_stream.send(Message::Binary(bytes)).await.unwrap();
}

pub async fn start(listener_addr: SocketAddr, browser_addr: SocketAddr) -> color_eyre::Result<()> {
    println!("\nBRIDGE\n");

    // use IP 0.0.0.0 so that it is possible to receive any traffic directed to port 8080
    // "0.0.0.0:8080"
    let listener = TcpListener::bind(&listener_addr).await?;
    println!("WebSocket server listening on {listener_addr}");
    let (stream, _) = listener.accept().await?;
    let mut ws_stream = accept_async(stream).await?;

    println!("-------------------------------");
    println!("waiting for browser connections");

    // open the browser on the following address
    //let browser_addr = "127.0.0.1:9090";
    let browser_listener = TcpListener::bind(&browser_addr).await?;

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
