use backoff::ExponentialBackoff;
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info};
use tracing_subscriber::{prelude::*, EnvFilter};
use url::Url;

use forwarder::proto_message::{ProtoMessage, WebSocket};

#[derive(Debug, Parser)]
struct Cli {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    // /// Start a bridge instance
    // Bridge {
    //     /// Address the bridge will listen to new connections from devices
    //     #[clap(long)]
    //     listener_addr: SocketAddr,
    //     /// Address the bridge will listen to new connections from the browser
    //     #[clap(long)]
    //     browser_addr: SocketAddr,
    // },
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

    // let cli = Cli::parse();

    // match cli.command {
    //     Commands::Device { bridge_url } => {
    //         // "ws://192.168.122.36:8080"
    //         debug!(?bridge_url);
    //         let res = forwarder::forwarder::start(bridge_url).await;
    //         debug!(?res)
    //     } // Commands::Bridge {
    //       //     listener_addr,
    //       //     browser_addr,
    //       // } => {
    //       //     // "0.0.0.0:8080"
    //       //     // "127.0.0.1:9090"
    //       //     debug!(?listener_addr);
    //       //     debug!(?browser_addr);
    //       //     forwarder::bridge::start(listener_addr, browser_addr).await?;
    //       // }
    // }

    main_ping_pong().await;

    Ok(())
}

#[allow(dead_code)]
async fn main_ping_pong() {
    let (mut ws, http_res) = backoff::future::retry(ExponentialBackoff::default(), || async {
        println!(
            "creating websocket connection with {}",
            "ws://kaiki.local:4000/shell/websocket"
        );
        Ok(connect_async("ws://kaiki.local:4000/device/websocket?session=123cacca456").await?)
    })
    .await
    .expect("failed to perform exponential backoff1");

    debug!(?http_res);

    let socket_id = Vec::from("id1");
    let message = forwarder::proto_message::WebSocketMessage::ping(Vec::from("ciao coglione"));
    let proto_msg = ProtoMessage::new(forwarder::proto_message::Protocol::WebSocket(
        WebSocket::new(socket_id, message),
    ));

    let tung_msg = Message::Binary(proto_msg.encode().expect("failed to encode ProtoMessage"));

    ws.send(tung_msg)
        .await
        .expect("failed to send ping over websocket");

    let res = ws
        .next()
        .await
        .expect("websocket connection closed")
        .expect("failed to receive from websocket");
    match res {
        tokio_tungstenite::tungstenite::Message::Binary(data) => {
            let proto_msg =
                ProtoMessage::decode(&data).expect("failed to decode into ProtoMessage");
            info!("received {proto_msg:?}");
        }
        msg => error!("received wrong websocket message type: {msg}"),
    }

    ws.close(None).await.expect("failed to close websocket");

    info!("websocket connection closed");

    // SEND TEXT FRAME (and open 2 ws connections)
    /*
    let tung_msg = Message::Text(String::from("ciao frateme"));

    ws.send(tung_msg)
        .await
        .expect("failed to send ping over websocket");

    let (mut ws2, http_res) = backoff::future::retry(ExponentialBackoff::default(), || async {
        println!(
            "creating websocket connection with {}",
            "ws://kaiki.local:4000/shell/websocket"
        );
        Ok(connect_async("ws://kaiki.local:4000/shell/websocket").await?)
    })
    .await
    .expect("failed to perform exponential backoff1");

    debug!(?http_res);

    let tung_msg = Message::Text(String::from("ciao frateme 2"));

    ws2.send(tung_msg)
        .await
        .expect("failed to send ping over websocket");

    // let res = ws
    //     .next()
    //     .await
    //     .expect("websocket connection closed")
    //     .expect("failed to receive from websocket");
    // match res {
    //     tokio_tungstenite::tungstenite::Message::Text(data) => {
    //         info!("received {data}");
    //     }
    //     _ => error!("received wrong websocket message type"),
    // }

    ws.close(None).await.expect("failed to close websocket");
    ws2.close(None).await.expect("failed to close websocket");

    info!("websocket connection closed");

     */
}
