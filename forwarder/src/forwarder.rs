use std::time::Duration;

use backoff::ExponentialBackoff;
use displaydoc::Display;
use thiserror::Error as ThisError;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Error as TungError;
use tracing::{debug, error, info, instrument};
use tungstenite::Message as TungMessage;
use url::Url;

use crate::astarte::{ConnectionInfo, Error as AstarteError};
use crate::connection::ConnectionError;
use crate::connections::{Connections, ConnectionsHandler, WebSocketOperation};
use crate::proto_message::ProtoError;

#[non_exhaustive]
#[derive(Display, ThisError, Debug)]
pub enum Error {
    /// Error performing exponential backoff when trying to connect with Edgehog, `{0}`.
    WebSocketConnect(#[from] TungError),
    /// Trying to perform an operation over a non-existing connection
    NonExistingConnection,
    /// Connection errors, `{0}`.
    Connection(#[from] ConnectionError),
    /// Protobuf error, `{0}`.
    Protobuf(#[from] ProtoError),
    /// Astarte error, `{0}`.
    Astarte(#[from] AstarteError),
}

#[derive(Debug, Default)]
pub enum ForwarderState {
    #[default]
    Waiting,
    Connected(Url),
}

#[derive(Debug, Default)]
pub struct Forwarder {
    state: ForwarderState,
    connections: Option<ConnectionsHandler>,
}

impl Forwarder {
    /// Check if the device has already established a WebSocekt connection with edgehog
    fn is_connected(&self) -> bool {
        match self.state {
            ForwarderState::Connected(_) => true,
            _ => false,
        }
    }

    fn set_connections(&mut self, con_handler: ConnectionsHandler) {
        self.connections = Some(con_handler)
    }

    /// Method called to establish a new connection between the device and edgehog.
    #[instrument(skip_all)]
    pub async fn connect(&mut self, cinfo: ConnectionInfo) -> Result<(), Error> {
        if self.is_connected() {
            info!("connection with edgehog already established");
            return Ok(());
        }

        let edgehog_url = Url::try_from(cinfo)?;

        let session_token = edgehog_url
            .query()
            .expect("session token must be present")
            .to_string();

        // TODO: check what happens when a wrong URL is passed
        // try openning a websocket connection with edgehog using exponential backoff
        let (ws_stream, http_res) =
            backoff::future::retry(ExponentialBackoff::default(), || async {
                println!("creating websocket connection with {}", edgehog_url);
                Ok(connect_async(&edgehog_url).await?)
            })
                .await
                .map_err(Error::WebSocketConnect)?;

        debug!(?http_res);

        let con_handler = Connections::new(session_token, ws_stream, edgehog_url);

        self.set_connections(con_handler);

        Ok(())
    }

    pub async fn handle_connections(&mut self) -> Result<(), Error> {
        let connections = self.connections.as_mut().ok_or_else(|| Error::NonExistingConnection)?;

        loop {
            let op = connections
                // TODO: decide how to set the timeout interval to send Ping frame
                .select_ws_op(Duration::from_secs(5))
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

                        let edgehog_url = connections.get_url();
                        connections.reconnect(edgehog_url).await?;
                    }
                    err => error!(?err),
                }
            }
        }

        Ok(())
    }

    // TODO: think about implementing a disconnect() method that can be called from outside the library
    // to close the websocekt connection between the device and edgehog (therefore close all sessions/connections)
}
