use backoff::ExponentialBackoff;
use displaydoc::Display;
use thiserror::Error;
use tokio::time::Duration;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Error as TungError;
use tokio_tungstenite::tungstenite::Message as TungMessage;
use tracing::{debug, error};
use url::Url;

use crate::connection::ConnectionError;
use crate::connections::Connections;
use crate::connections::WebSocketOperation;
use crate::proto_message::ProtoError;

#[non_exhaustive]
#[derive(Display, Error, Debug)]
pub enum DeviceError {
    /// Error performing exponential backoff when trying to connect with Edgehog, {0}
    WebSocketConnect(#[source] TungError),
    /// Connection errors
    Connection(#[from] ConnectionError),
    /// Protobuf error.
    Protobuf(#[from] ProtoError),
}

// The device will receive from Astarte the session_token, the url of the edgehog-device-forwarder and other information to properely establish a ws connection
// -> Es://<IP:PORT>/<PATH>?session_secret=<SESSION_SECRET>
// TODO: remove bridge_url + define a task responsible for receiving messages from astarte and use the main task to handle the connections (use a channel for tasks communication)
pub async fn start(mut bridge_url: Url) -> Result<(), DeviceError> {
    println!("\nDEVICE\n");

    let session_token = String::from("token1"); // TODO: remove once it is sent by Astarte
    bridge_url
        .query_pairs_mut()
        .append_pair("session_token", &session_token);

    // try openning a websocket connection with edgehog using exponential backoff
    let (ws_stream, http_res) = backoff::future::retry(ExponentialBackoff::default(), || async {
        println!("creating websocket connection with {}", bridge_url);
        Ok(connect_async(&bridge_url).await?)
    })
    .await
    .map_err(|err| DeviceError::WebSocketConnect(err))?;

    debug!(?http_res);

    let (mut connections, mut rx_ws) = Connections::new(session_token, ws_stream);

    loop {
        let op = connections
            // TODO: decide how to set the timeout interval to send Ping frame
            .select_ws_op(&mut rx_ws, Duration::from_secs(5))
            .await;

        let res = match op {
            // receive from edgehog
            WebSocketOperation::Receive(msg) => match msg {
                Some(Ok(msg)) => connections.handle_msg(msg).await,
                Some(Err(err)) => Err(err.into()),
                None => {
                    error!("stream closed, terminating device...");
                    break;
                }
            },
            // receive from TTYD
            WebSocketOperation::Send(tung_msg) => {
                let msg = tung_msg
                    .expect("rx_ws channel returned None, channel closed")
                    .encode()?;

                let msg = TungMessage::Binary(msg);

                connections.send(msg).await
            }
            // in case no data is received in Xs over ws, send a ping.
            // if no pong is received withn Y seconds, close the connection gracefully
            WebSocketOperation::Ping => {
                let msg = TungMessage::Ping(Vec::new());

                connections.send(msg).await
            }
        };

        // if the connection has been suddenly interrupted, try re-establishing it
        if let Err(err) = res {
            match err {
                ConnectionError::WebSocketSend(err)
                | ConnectionError::WebSocketNext(err)
                | ConnectionError::Reconnect(err) => {
                    error!(?err);
                    debug!("trying to reconnect");
                    connections.reconnect(&bridge_url).await?;
                }
                err => error!(?err),
            }
        }
    }

    Ok(())
}
