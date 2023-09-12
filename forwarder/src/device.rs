use futures_util::StreamExt;
use tokio::select;
use tokio_tungstenite::connect_async;
use tracing::error;
use url::Url;

use crate::connection::{recv_tcp, recv_ws, Connections};

pub async fn start(bridge_url: Url) {
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
