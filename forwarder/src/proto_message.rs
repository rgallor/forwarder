use std::{collections::HashMap, str::FromStr};

use prost::Message as ProstMessage;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Client as ReqwClient, Response as ReqwResponse,
};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message as TungMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::error;

#[derive(Debug)]
pub struct ProtoMessage {
    protocol: Protocol,
}

impl ProtoMessage {
    pub fn new(protocol: Protocol) -> Self {
        Self { protocol }
    }

    pub fn protocol(self) -> Protocol {
        self.protocol
    }

    pub fn encode(self) -> Result<Vec<u8>, ()> {
        let protocol = match self.protocol {
            Protocol::Http(Http::Request(http_req)) => {
                let mut proto_req = proto::http::Request::default();
                proto_req.request_id = http_req.request_id;
                proto_req.path = http_req.path;
                proto_req.method = http_req.method;
                proto_req.querystring = http_req.querystring;
                proto_req.headers = http_req.headers;
                proto_req.payload = http_req.payload;

                proto::message::Protocol::Http(proto::Http {
                    message: Some(proto::http::Message::Request(proto_req)),
                })
            }
            Protocol::Http(Http::Response(http_res)) => {
                let mut proto_res = proto::http::Response::default();
                proto_res.request_id = http_res.request_id;
                proto_res.status_code = http_res.status_code as u32; // conversion from u16 to u32 is safe
                proto_res.headers = http_res.headers;
                proto_res.payload = http_res.payload;

                proto::message::Protocol::Http(proto::Http {
                    message: Some(proto::http::Message::Response(proto_res)),
                })
            }
            Protocol::WebSocket(ws) => {
                let mut proto_ws = proto::WebSocket::default();
                proto_ws.socket_id = ws.socket_id;

                let ws_message = match ws.message {
                    WebSocketMessage::Text(data) => proto::web_socket::Message::Text(data),
                    WebSocketMessage::Binary(data) => proto::web_socket::Message::Binary(data),
                    WebSocketMessage::Ping(data) => proto::web_socket::Message::Ping(data),
                    WebSocketMessage::Pong(data) => proto::web_socket::Message::Pong(data),
                    WebSocketMessage::Close { code, reason } => {
                        proto::web_socket::Message::Close(proto::web_socket::Close {
                            code: code as u32,
                            reason,
                        })
                    }
                };
                proto_ws.message = Some(ws_message);

                proto::message::Protocol::Ws(proto_ws)
            }
        };

        let mut msg = proto::Message::default();
        msg.protocol = Some(protocol);

        let mut buf = Vec::new();
        let buf_size = msg.encoded_len();
        buf.reserve(buf_size);

        msg.encode(&mut buf).expect("failed to serialize Message");

        Ok(buf)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ()> {
        let msg = proto::Message::decode(bytes).map_err(|_| ())?;
        ProtoMessage::try_from(msg)
    }
}

impl TryFrom<TungMessage> for ProtoMessage {
    type Error = (); // SHOULD BE TUNGSTENITE ERROR

    fn try_from(tung_msg: TungMessage) -> Result<Self, Self::Error> {
        Ok(Self::new(Protocol::WebSocket(WebSocket {
            socket_id: Vec::new(),
            message: WebSocketMessage::try_from(tung_msg)?,
        })))
    }
}

#[derive(Debug)]
pub enum Protocol {
    Http(Http),
    WebSocket(WebSocket),
}

#[derive(Debug)]
pub enum Http {
    Request(HttpRequest),
    Response(HttpResponse),
}

#[derive(Debug)]
pub struct HttpRequest {
    request_id: Vec<u8>,
    path: String,
    method: String,
    querystring: Option<String>,
    headers: HashMap<String, String>,
    payload: Option<Vec<u8>>,
}

impl HttpRequest {
    pub fn is_connection_upgrade(&self) -> bool {
        self.headers.contains_key("Upgrade")
    }

    pub async fn send(self) -> Result<(Vec<u8>, ReqwResponse), ()> {
        // TODO: the request could be created when deserializing the message coming from edgehog
        let query_params = self.querystring.map_or(String::from(""), |q| q);

        let url_str = String::from("http://localhost:7681/") + &self.path + &query_params;
        let url = url::Url::parse(&url_str).map_err(|err| {
            error!("failed to parse url_str into Url, {:?}", err);
            ()
        })?;

        // TODO: verify that try_from cannot be used to convert HashMap into HeaderMap
        let headers = hashmap_to_headermap(self.headers.iter());

        // TODO: chack that is good to set an empty Vec<u8> as payload
        let payload = self.payload.unwrap_or(Vec::new());

        let reqw_client = match self.method.to_ascii_uppercase().as_str() {
            "GET" => ReqwClient::new().get(url),
            "DELETE" => ReqwClient::new().delete(url),
            "HEAD" => ReqwClient::new().head(url),
            "PATCH" => ReqwClient::new().patch(url),
            "POST" => ReqwClient::new().post(url),
            "PUT" => ReqwClient::new().put(url),
            wrong_method => {
                error!("wrong method received, {}", wrong_method);
                return Err(());
            }
        };

        let http_res = reqw_client
            .headers(headers)
            .body(payload)
            .send()
            .await
            .map_err(|err| {
                error!(?err);
                ()
            })?;

        Ok((self.request_id, http_res))
    }

    pub async fn upgrade(
        self,
    ) -> Result<
        (
            Vec<u8>,
            WebSocketStream<MaybeTlsStream<TcpStream>>,
            tungstenite::http::Response<Option<Vec<u8>>>,
        ),
        (),
    > {
        // TODO: the request could be created when deserializing the message coming from edgehog
        let query_params = self.querystring.map_or(String::from(""), |q| q);

        // TODO: WSS schema ?
        let url = String::from("ws://localhost:7681/") + &self.path + &query_params;
        // let url = url::Url::parse(&url_str).map_err(|err| {
        //     error!("failed to parse url_str into Url, {:?}", err);
        //     ()
        // })?;

        let req = match self.method.to_ascii_uppercase().as_str() {
            "GET" => tungstenite::http::Request::get(url),
            "DELETE" => tungstenite::http::Request::delete(url),
            "HEAD" => tungstenite::http::Request::head(url),
            "PATCH" => tungstenite::http::Request::patch(url),
            "POST" => tungstenite::http::Request::post(url),
            "PUT" => tungstenite::http::Request::put(url),
            wrong_method => {
                error!("wrong method received, {}", wrong_method);
                return Err(());
            }
        };

        // ad the headers to the request
        let req =
            hashmap_to_headermap(self.headers.iter())
                .into_iter()
                .fold(req, |req, (key, val)| match key {
                    Some(key) => req.header(key, val),
                    None => req,
                });

        // TODO: no payload canbe used because tokio_tungstenite wants Request<()>
        // let payload = self.payload.unwrap_or(Vec::new());

        let req = req.body(()).expect("failed to build request");

        let (ws, res) = tokio_tungstenite::connect_async(req).await.map_err(|err| {
            error!(?err);
            ()
        })?;

        Ok((self.request_id, ws, res))
    }
}

fn hashmap_to_headermap<'a, I, S>(headers: I) -> HeaderMap
where
    I: Iterator<Item = (S, S)> + 'a,
    S: AsRef<str> + 'a,
{
    headers
        .map(|(name, val)| {
            (
                HeaderName::from_str(name.as_ref()),
                HeaderValue::from_str(val.as_ref()),
            )
        })
        // We ignore the errors here. If you want to get a list of failed conversions, you can use Iterator::partition
        // to help you out here
        .filter(|(k, v)| k.is_ok() && v.is_ok())
        .map(|(k, v)| (k.unwrap(), v.unwrap()))
        .collect()
}

#[derive(Debug)]
pub struct HttpResponse {
    request_id: Vec<u8>,
    status_code: u16,
    headers: HashMap<String, String>,
    payload: Option<Vec<u8>>,
}

impl HttpResponse {
    pub fn new(
        request_id: &[u8],
        status_code: u16,
        headers: HashMap<String, String>,
        payload: Option<Vec<u8>>,
    ) -> Self {
        Self {
            request_id: request_id.to_vec(),
            status_code,
            headers,
            payload,
        }
    }
}

pub fn headermap_to_hashmap<'a, I>(headers: I) -> HashMap<String, String>
where
    I: Iterator<Item = (&'a HeaderName, &'a HeaderValue)>,
{
    headers
        .map(|(name, val)| {
            (
                name.to_string(),
                val.to_str()
                    .expect("failed to create a string for HeaderValue")
                    .to_string(),
            )
        })
        .collect()
}

#[derive(Debug)]
pub struct WebSocket {
    socket_id: Vec<u8>,
    message: WebSocketMessage,
}

impl WebSocket {
    pub fn new(socket_id: Vec<u8>, message: WebSocketMessage) -> Self {
        Self { socket_id, message }
    }

    pub fn into_inner(self) -> (Vec<u8>, WebSocketMessage) {
        (self.socket_id, self.message)
    }

    pub fn close(socket_id: Vec<u8>, reason: Option<String>) -> Self {
        Self {
            socket_id,
            message: WebSocketMessage::close(1000, reason),
        }
    }
}
#[derive(Debug)]
pub enum WebSocketMessage {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close { code: u16, reason: Option<String> },
}

impl WebSocketMessage {
    pub fn text(data: String) -> Self {
        Self::Text(data)
    }

    pub fn binary(data: Vec<u8>) -> Self {
        Self::Binary(data)
    }

    pub fn ping(data: Vec<u8>) -> Self {
        Self::Ping(data)
    }

    pub fn pong(data: Vec<u8>) -> Self {
        Self::Pong(data)
    }

    pub fn close(code: u16, reason: Option<String>) -> Self {
        Self::Close { code, reason }
    }
}

impl TryFrom<TungMessage> for WebSocketMessage {
    type Error = (); // SHOULD BE TUNGSTENITE ERROR

    fn try_from(tung_msg: TungMessage) -> Result<Self, Self::Error> {
        let msg = match tung_msg {
            tungstenite::Message::Text(data) => WebSocketMessage::text(data),
            tungstenite::Message::Binary(data) => WebSocketMessage::binary(data),
            tungstenite::Message::Ping(data) => WebSocketMessage::ping(data),
            tungstenite::Message::Pong(data) => WebSocketMessage::pong(data),
            tungstenite::Message::Close(data) => {
                let close_frame = data.expect("error unwrapping websocket close frame");
                let code = close_frame.code.into();
                let reason = Some(close_frame.reason.into_owned());

                WebSocketMessage::close(code, reason)
            }
            tungstenite::Message::Frame(_) => {
                error!("this kind of message should not be sent");
                return Err(());
            }
        };

        Ok(msg)
    }
}

impl From<WebSocketMessage> for TungMessage {
    fn from(ws_msg: WebSocketMessage) -> Self {
        match ws_msg {
            WebSocketMessage::Text(data) => TungMessage::Text(data),
            WebSocketMessage::Binary(data) => TungMessage::Binary(data),
            WebSocketMessage::Ping(data) => TungMessage::Ping(data),
            WebSocketMessage::Pong(data) => TungMessage::Pong(data),
            WebSocketMessage::Close { code, reason } => {
                TungMessage::Close(Some(tungstenite::protocol::CloseFrame {
                    code: code.into(),
                    reason: reason.unwrap().into(),
                }))
            }
        }
    }
}

// Include the `proto` module, which is generated from proto.proto.
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/edgehog_device_forwarder.rs"));
}

impl TryFrom<proto::Message> for ProtoMessage {
    type Error = ();

    fn try_from(value: proto::Message) -> Result<Self, Self::Error> {
        let proto::Message { protocol } = value;

        let protocol = protocol.map(|p| p.try_into()).unwrap()?;

        Ok(ProtoMessage::new(protocol))
    }
}

impl TryFrom<proto::message::Protocol> for Protocol {
    type Error = ();

    fn try_from(value: proto::message::Protocol) -> Result<Self, Self::Error> {
        let protocol = match value {
            proto::message::Protocol::Http(proto::Http {
                message: Some(proto::http::Message::Request(req)),
            }) => Protocol::Http(Http::Request(req.into())),
            proto::message::Protocol::Http(proto::Http {
                message: Some(proto::http::Message::Response(res)),
            }) => Protocol::Http(Http::Response(res.into())),
            proto::message::Protocol::Http(proto::Http { message: None }) => {
                return Err(());
            }
            proto::message::Protocol::Ws(ws) => Protocol::WebSocket(ws.try_into()?),
        };

        Ok(protocol)
    }
}

impl From<proto::http::Request> for HttpRequest {
    fn from(value: proto::http::Request) -> Self {
        let proto::http::Request {
            request_id,
            path,
            method,
            querystring,
            headers,
            payload,
        } = value;
        Self {
            request_id,
            path,
            method,
            querystring,
            headers,
            payload,
        }
    }
}

impl From<proto::http::Response> for HttpResponse {
    fn from(value: proto::http::Response) -> Self {
        let proto::http::Response {
            request_id,
            status_code,
            headers,
            payload,
        } = value;
        Self {
            request_id,
            status_code: status_code as u16,
            headers,
            payload,
        }
    }
}

impl TryFrom<proto::WebSocket> for WebSocket {
    type Error = ();

    fn try_from(value: proto::WebSocket) -> Result<Self, Self::Error> {
        let proto::WebSocket { socket_id, message } = value;

        let Some(msg) = message else { return Err(()) };

        let message = match msg {
            proto::web_socket::Message::Text(data) => WebSocketMessage::text(data),
            proto::web_socket::Message::Binary(data) => WebSocketMessage::binary(data),
            proto::web_socket::Message::Ping(data) => WebSocketMessage::ping(data),
            proto::web_socket::Message::Pong(data) => WebSocketMessage::pong(data),
            proto::web_socket::Message::Close(close) => {
                WebSocketMessage::close(close.code as u16, close.reason)
            }
        };

        Ok(Self { socket_id, message })
    }
}
