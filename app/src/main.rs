use std::net::SocketAddr;

use clap::{Parser, Subcommand};
use forwarder::connection::{ConnMsg, Connections, Id, Transmitted, WsMsg, WsTransmitted};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::{net::TcpStream, select, sync::mpsc::UnboundedReceiver};
use tokio_tungstenite::tungstenite::{Error as TungError, Message};
use tokio_tungstenite::{accept_async, connect_async, WebSocketStream};
use tracing::{debug, error, info, instrument, warn};
use tracing_subscriber::{prelude::*, EnvFilter};
use url::Url;

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

#[instrument(skip_all)]
async fn recv_tcp<T>(data: Option<Transmitted>, ws_stream: &mut WebSocketStream<T>)
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
async fn recv_ws(msg: Message, connections: &mut Connections) {
    if let Err(err) = connections.handle_msg(msg).await {
        error!(?err);
    }
}

async fn main_bridge(
    listener_addr: SocketAddr,
    browser_addr: SocketAddr,
) -> color_eyre::Result<()> {
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

async fn main_device(bridge_url: Url) {
    println!("\nDEVICE\n");

    // TODO: the device will receive from Astarte the sesssecret_token and other information to properely establish
    // a connection with the bridge

    let (mut ws_stream, _) = connect_async(bridge_url).await.unwrap();

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

#[derive(Debug, Parser)]
struct Cli {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start a bridge instance
    Bridge {
        /// Address the bridge will listen to new connections from devices
        #[clap(long)]
        listener_addr: SocketAddr,
        /// Address the bridge will listen to new connections from the browser
        #[clap(long)]
        browser_addr: SocketAddr,
    },
    /// Start a device instance
    Device {
        /// Url of the bridge the device wants to connect with
        #[clap(long)]
        bridge_url: Url,
    },
}

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    std::env::set_var("RUST_LOG", "debug");

    color_eyre::install().unwrap();
    tracing_subscriber::registry()
        .with(console_subscriber::spawn())
        .with(tracing_subscriber::fmt::layer())
        .with(EnvFilter::from_default_env())
        .try_init()?;

    let cli = Cli::parse();

    match cli.command {
        Commands::Device { bridge_url } => {
            // "ws://192.168.122.36:8080"
            debug!(?bridge_url);
            main_device(bridge_url).await;
        }
        Commands::Bridge {
            listener_addr,
            browser_addr,
        } => {
            // "0.0.0.0:8080"
            // "127.0.0.1:9090"
            debug!(?listener_addr);
            debug!(?browser_addr);
            main_bridge(listener_addr, browser_addr).await?;
        }
    }

    Ok(())
}
