use backoff::ExponentialBackoff;
use displaydoc::Display;
use thiserror::Error;
use tokio::time::Duration;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Error as TungError;
use tokio_tungstenite::tungstenite::Message as TungMessage;
use tracing::{debug, error};
use url::Url;

use crate::astarte::{handle_astarte_events, Error as AstarteError};
use crate::connection::ConnectionError;
use crate::connections::Connections;
use crate::connections::WebSocketOperation;
use crate::proto_message::ProtoError;

#[non_exhaustive]
#[derive(Display, Error, Debug)]
pub enum DeviceError {
    /// Error performing exponential backoff when trying to connect with Edgehog, `{0}`.
    WebSocketConnect(#[from] TungError),
    /// Connection errors, `{0}`.
    Connection(#[from] ConnectionError),
    /// Protobuf error, `{0}`.
    Protobuf(#[from] ProtoError),
    /// Astarte error, `{0}`.
    Astarte(#[from] AstarteError),
}

pub async fn start() -> Result<(), DeviceError> {
    println!("\nDEVICE STARTED\n");

    // The device receives from Astarte the url of the edgehog-device-forwarder and other information to properely establish a ws connection with it
    // -> Es://<IP:PORT>/<PATH>?session_secret=<SESSION_SECRET>
    let (tx_url, mut rx_url) = tokio::sync::mpsc::unbounded_channel::<Url>();

    // define a task to handle astarte events and send the received URL to the task responsible for the creation of new connections
    let handle_astarte = tokio::spawn(handle_astarte_events(tx_url));

    // TODO: at the moment only 1 connection at a time is handled. Spawn a task for each connection to be handled.
    // receive url until an astarte error is received
    while let Some(edgehog_url) = rx_url.recv().await {
        let session_token = edgehog_url
            .query()
            .expect("session token must be present")
            .to_string();

        // try openning a websocket connection with edgehog using exponential backoff
        let (ws_stream, http_res) =
            backoff::future::retry(ExponentialBackoff::default(), || async {
                println!("creating websocket connection with {}", edgehog_url);
                Ok(connect_async(&edgehog_url).await?)
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
                        connections.reconnect(&edgehog_url).await?;
                    }
                    err => error!(?err),
                }
            }
        }
    }

    match handle_astarte.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(err),
        Err(err) => {
            error!("tokio join error, {err}");
            Ok(())
        }
    }
}
