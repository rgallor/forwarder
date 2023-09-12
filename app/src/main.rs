use std::net::SocketAddr;

use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};

use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info};
use tracing_subscriber::{prelude::*, EnvFilter};
use url::Url;

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
            forwarder::device::start(bridge_url).await;
        }
        Commands::Bridge {
            listener_addr,
            browser_addr,
        } => {
            // "0.0.0.0:8080"
            // "127.0.0.1:9090"
            debug!(?listener_addr);
            debug!(?browser_addr);
            forwarder::bridge::start(listener_addr, browser_addr).await?;
        }
    }

    Ok(())
}

#[allow(dead_code)]
async fn main_ping_pong() {
    let (mut ws, http_res) = connect_async("http://noaccos.ovh:4000/shell/websocket")
        .await
        .expect("failed to open websocket");

    debug!(?http_res);

    ws.send(Message::Ping(b"ciao fra".to_vec()))
        .await
        .expect("failed to send ping over websocket");

    let res = ws
        .next()
        .await
        .expect("websocket connection closed")
        .expect("failed to receive from websocket");
    match res {
        tokio_tungstenite::tungstenite::Message::Pong(data) => {
            info!("received pong: {data:?}");
        }
        _ => error!("received wrong websocket message type"),
    }

    ws.close(None).await.expect("failed to close websocket");

    info!("websocket connection closed");
}
