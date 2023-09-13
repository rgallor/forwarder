use std::hash::Hash;

use backoff::ExponentialBackoff;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::{timeout, Duration};
use tokio::{select, sync::mpsc::UnboundedReceiver};
use tokio_tungstenite::tungstenite::Error as TungError;
use tokio_tungstenite::tungstenite::Message as TungMessage;
use tokio_tungstenite::{connect_async, WebSocketStream};
use tracing::{debug, error};
use url::Url;

use crate::connection::{recv_ws, Connections};
use crate::proto_message::ProtoMessage;

pub async fn start(mut bridge_url: Url) {
    println!("\nDEVICE\n");

    // The device will receive from Astarte the session_token, the url of the edgehog-device-forwarder and other information to properely establish a ws connection
    // -> Es://<IP:PORT>/<PATH>?session_secret=<SESSION_SECRET>
    // TODO: define a task responsible for receiving messages from astarte and use the main task to handle the sessions (use a channel for tasks communication)

    // TODO: define a struct Sessions that implements the Future trait, allowing to poll on every websocket (of each Session) waiting for the firs incoming message
    // let mut sessions = HashSet::new();

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
    .expect("failed to perform exponential backoff1");

    debug!(?http_res);

    let (mut session, mut rx_ws) = Session::new(session_token, ws_stream);

    // sessions.insert(Session::new(session_secret, ws_stream));

    loop {
        let op = session
            .select_ws_op(&mut rx_ws, Duration::from_secs(5))
            .await;

        match op {
            // receive from edgehog
            WebSocketOperation::Receive(msg) => match msg {
                Some(Ok(msg)) => recv_ws(msg, &mut session.connections).await,
                Some(Err(err)) => error!(?err),
                None => {
                    error!("stream closed");
                    break;
                }
            },
            // receive from TTYD
            WebSocketOperation::Send(tung_msg) => {
                let msg = tung_msg
                    .expect("rx_ws channel returned None, channel closed")
                    .encode()
                    .expect("failed to encode ProtoMessage");
                let msg = TungMessage::Binary(msg);

                session
                    .ws_stream
                    .send(msg)
                    .await
                    .expect("failed to send data on websocket toward device");
            }
            // in case no data is received in Xs over ws, send a ping.
            // if no pong is received withn Y seconds, close the connection gracefully
            WebSocketOperation::Ping => todo!(),
        }
    }
}

enum WebSocketOperation {
    Receive(Option<Result<TungMessage, TungError>>),
    Send(Option<ProtoMessage>),
    Ping,
}

#[derive(Debug)]
pub struct Session<T> {
    session_token: String,
    ws_stream: WebSocketStream<T>,
    connections: Connections,
}

impl<T> Hash for Session<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.session_token.hash(state);
    }
}

impl<T> PartialEq for Session<T> {
    fn eq(&self, other: &Self) -> bool {
        self.session_token == other.session_token
    }
}

impl<T> Eq for Session<T> {}

impl<T> Session<T> {
    pub fn new(
        session_secret: String,
        ws_stream: WebSocketStream<T>,
    ) -> (Self, UnboundedReceiver<ProtoMessage>) {
        let (connections, rx_ws) = Connections::new();

        let session = Self {
            session_token: session_secret,
            ws_stream,
            connections,
        };

        (session, rx_ws)
    }

    async fn select_ws_op(
        &mut self,
        rx_ws: &mut UnboundedReceiver<ProtoMessage>,
        timeout_ping: Duration,
    ) -> WebSocketOperation
    where
        T: AsyncRead + AsyncWrite + Unpin,
    {
        select! {
            res = timeout(timeout_ping, self.ws_stream.next()) => {
                match res {
                    Ok(msg) => WebSocketOperation::Receive(msg),
                    Err(_) => WebSocketOperation::Ping
                }
            }
            tung_msg = rx_ws.recv() => WebSocketOperation::Send(tung_msg)
        }
    }
}
