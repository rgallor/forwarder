use std::{collections::HashMap, ops::Not, str::FromStr};

use displaydoc::Display;
use prost::Message as ProstMessage;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Client as ReqwClient, Response as ReqwResponse,
};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message as TungMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::error;
use tungstenite::http::Request as TungHttpRequest;
use tungstenite::Error as TungError;
use url::ParseError;

use proto::http::Message as ProtoHttpMessage;
use proto::http::Request as ProtoHttpRequest;
use proto::http::Response as ProtoHttpResponse;
use proto::message::Protocol as ProtoProtocol;
use proto::web_socket::Message as ProtoWsMessage;

#[derive(Display, Error, Debug)]
#[non_exhaustive]
pub enum ProtoError {
    /// Failed to deserialize from Protobuf, `{0}`.
    Decode(#[from] prost::DecodeError),
    /// Failed to serialize into Protobuf, `{0}`.
    Encode(#[from] prost::EncodeError),
    /// Received a wrong WebSocket frame.
    WrongWsFrame,
    /// Empty WebSocket message field.
    EmptyWsMessage,
    /// Empty HTTP message field.
    EmptyHttpMessage,
    /// Wrong HTTP method field.
    WrongHttpMethod(String),
    /// Http error, `{0}`.
    Http(#[from] tungstenite::http::Error),
    /// Reqwest error, `{0}`.
    Reqwest(#[from] reqwest::Error),
    /// Error parsing URL, `{0}`.
    ParseUrl(#[from] ParseError),
    /// Error performing exponential backoff when trying to connect with TTYD, {0}
    WebSocketConnect(#[from] TungError),
}

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

    pub fn encode(self) -> Result<Vec<u8>, ProtoError> {
        let protocol = match self.protocol {
            Protocol::Http(Http::Request(http_req)) => {
                let mut proto_req = ProtoHttpRequest::default();
                proto_req.request_id = http_req.request_id;
                proto_req.path = http_req.path;
                proto_req.method = http_req.method;
                proto_req.querystring = http_req
                    .querystring
                    .is_empty()
                    .not()
                    .then_some(http_req.querystring);
                proto_req.headers = http_req.headers;
                proto_req.payload = http_req
                    .payload
                    .is_empty()
                    .not()
                    .then_some(http_req.payload);

                ProtoProtocol::Http(proto::Http {
                    message: Some(ProtoHttpMessage::Request(proto_req)),
                })
            }
            Protocol::Http(Http::Response(http_res)) => {
                let mut proto_res = ProtoHttpResponse::default();
                proto_res.request_id = http_res.request_id;
                proto_res.status_code = http_res.status_code as u32; // conversion from u16 to u32 is safe
                proto_res.headers = http_res.headers;
                proto_res.payload = http_res
                    .payload
                    .is_empty()
                    .not()
                    .then_some(http_res.payload);

                ProtoProtocol::Http(proto::Http {
                    message: Some(ProtoHttpMessage::Response(proto_res)),
                })
            }
            Protocol::WebSocket(ws) => {
                let mut proto_ws = proto::WebSocket::default();
                proto_ws.socket_id = ws.socket_id;

                let ws_message = match ws.message {
                    WebSocketMessage::Text(data) => ProtoWsMessage::Text(data),
                    WebSocketMessage::Binary(data) => ProtoWsMessage::Binary(data),
                    WebSocketMessage::Ping(data) => ProtoWsMessage::Ping(data),
                    WebSocketMessage::Pong(data) => ProtoWsMessage::Pong(data),
                    WebSocketMessage::Close { code, reason } => {
                        ProtoWsMessage::Close(proto::web_socket::Close {
                            code: code as u32,
                            reason,
                        })
                    }
                };
                proto_ws.message = Some(ws_message);

                ProtoProtocol::Ws(proto_ws)
            }
        };

        let mut msg = proto::Message::default();
        msg.protocol = Some(protocol);

        let mut buf = Vec::new();
        let buf_size = msg.encoded_len();
        buf.reserve(buf_size);

        msg.encode(&mut buf)?;

        Ok(buf)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let msg = proto::Message::decode(bytes).map_err(|err| ProtoError::from(err))?;
        ProtoMessage::try_from(msg)
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
    querystring: String,
    headers: HashMap<String, String>,
    payload: Vec<u8>,
    port: u16,
}

impl HttpRequest {
    pub fn is_connection_upgrade(&self) -> bool {
        self.headers.contains_key("Upgrade")
    }

    pub async fn send(self) -> Result<(Vec<u8>, ReqwResponse), ProtoError> {
        // TODO: the request could be created when deserializing the message coming from edgehog
        let url_str = format!(
            "ws://localhost:{}/{}{}",
            self.port, self.path, self.querystring
        );
        let url = url::Url::parse(&url_str)?;

        // TODO: verify that try_from cannot be used to convert HashMap into HeaderMap
        let headers = hashmap_to_headermap(self.headers.iter());

        let reqw_client = match self.method.to_ascii_uppercase().as_str() {
            "GET" => ReqwClient::new().get(url),
            "DELETE" => ReqwClient::new().delete(url),
            "HEAD" => ReqwClient::new().head(url),
            "PATCH" => ReqwClient::new().patch(url),
            "POST" => ReqwClient::new().post(url),
            "PUT" => ReqwClient::new().put(url),
            wrong_method => {
                error!("wrong method received, {}", wrong_method);
                return Err(ProtoError::WrongHttpMethod(wrong_method.to_string()));
            }
        };

        let http_res = reqw_client
            .headers(headers)
            .body(self.payload)
            .send()
            .await?;

        Ok((self.request_id, http_res))
    }

    pub async fn upgrade(self) -> Result<HttpUpgrade, ProtoError> {
        // TODO: the request could be created when deserializing the message coming from edgehog
        // TODO: WSS schema ?
        let url = format!(
            "ws://localhost:{}/{}{}",
            self.port, self.path, self.querystring
        );

        let req = match self.method.to_ascii_uppercase().as_str() {
            "GET" => TungHttpRequest::get(url),
            "DELETE" => TungHttpRequest::delete(url),
            "HEAD" => TungHttpRequest::head(url),
            "PATCH" => TungHttpRequest::patch(url),
            "POST" => TungHttpRequest::post(url),
            "PUT" => TungHttpRequest::put(url),
            wrong_method => {
                error!("wrong method received, {}", wrong_method);
                return Err(ProtoError::WrongHttpMethod(wrong_method.to_string()));
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

        // TODO: no payload can be used because tokio_tungstenite wants Request<()>
        let req = req.body(())?;

        let (ws, res) = tokio_tungstenite::connect_async(req).await?;

        Ok(HttpUpgrade::new(self.request_id, ws, res))
    }
}

#[derive(Debug)]
pub struct HttpUpgrade {
    request_id: Vec<u8>,
    ws_stream_ttyd: WebSocketStream<MaybeTlsStream<TcpStream>>,
    http_res: tungstenite::http::Response<Option<Vec<u8>>>,
}

impl HttpUpgrade {
    pub fn new(
        request_id: Vec<u8>,
        ws_stream_ttyd: WebSocketStream<MaybeTlsStream<TcpStream>>,
        http_res: tungstenite::http::Response<Option<Vec<u8>>>,
    ) -> Self {
        Self {
            request_id,
            ws_stream_ttyd,
            http_res,
        }
    }

    pub fn into_inner(
        self,
    ) -> (
        Vec<u8>,
        WebSocketStream<MaybeTlsStream<TcpStream>>,
        tungstenite::http::Response<Option<Vec<u8>>>,
    ) {
        (self.request_id, self.ws_stream_ttyd, self.http_res)
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
    payload: Vec<u8>,
}

impl HttpResponse {
    pub fn new(
        request_id: &[u8],
        status_code: u16,
        headers: HashMap<String, String>,
        payload: Vec<u8>,
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

    pub fn is_close(&self) -> bool {
        matches!(self, WebSocketMessage::Close { code: _, reason: _ })
    }
}

impl TryFrom<TungMessage> for ProtoMessage {
    type Error = ProtoError;

    fn try_from(tung_msg: TungMessage) -> Result<Self, Self::Error> {
        Ok(Self::new(Protocol::WebSocket(WebSocket {
            socket_id: Vec::new(),
            message: WebSocketMessage::try_from(tung_msg)?,
        })))
    }
}

impl TryFrom<TungMessage> for WebSocketMessage {
    type Error = ProtoError;

    fn try_from(tung_msg: TungMessage) -> Result<Self, Self::Error> {
        let msg = match tung_msg {
            tungstenite::Message::Text(data) => WebSocketMessage::text(data),
            tungstenite::Message::Binary(data) => WebSocketMessage::binary(data),
            tungstenite::Message::Ping(data) => WebSocketMessage::ping(data),
            tungstenite::Message::Pong(data) => WebSocketMessage::pong(data),
            tungstenite::Message::Close(data) => {
                // instead of returning an error, here i build a default close frame in case no frame is passed
                let (code, reason) = match data {
                    Some(close_frame) => {
                        let code = close_frame.code.into();
                        let reason = Some(close_frame.reason.into_owned());
                        (code, reason)
                    }
                    None => (1000, None),
                };

                WebSocketMessage::close(code, reason)
            }
            tungstenite::Message::Frame(_) => {
                error!("this kind of message should not be sent");
                return Err(Self::Error::WrongWsFrame);
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
                    reason: reason.unwrap_or(String::from("")).into(),
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
    type Error = ProtoError;

    fn try_from(value: proto::Message) -> Result<Self, Self::Error> {
        let proto::Message { protocol } = value;

        let protocol = protocol.map(|p| p.try_into()).unwrap()?;

        Ok(ProtoMessage::new(protocol))
    }
}

impl TryFrom<ProtoProtocol> for Protocol {
    type Error = ProtoError;

    fn try_from(value: ProtoProtocol) -> Result<Self, Self::Error> {
        let protocol = match value {
            ProtoProtocol::Http(proto::Http {
                message: Some(ProtoHttpMessage::Request(req)),
            }) => Protocol::Http(Http::Request(req.into())),
            ProtoProtocol::Http(proto::Http {
                message: Some(ProtoHttpMessage::Response(res)),
            }) => Protocol::Http(Http::Response(res.into())),
            ProtoProtocol::Http(proto::Http { message: None }) => {
                return Err(Self::Error::EmptyHttpMessage);
            }
            ProtoProtocol::Ws(ws) => Protocol::WebSocket(ws.try_into()?),
        };

        Ok(protocol)
    }
}

impl From<ProtoHttpRequest> for HttpRequest {
    fn from(value: ProtoHttpRequest) -> Self {
        let ProtoHttpRequest {
            request_id,
            path,
            method,
            querystring,
            headers,
            payload,
            port,
        } = value;
        Self {
            request_id,
            path,
            method,
            querystring: querystring.map_or(String::from(""), |q| q),
            headers,
            payload: payload.unwrap_or(Vec::new()),
            port: port as u16,
        }
    }
}

impl From<ProtoHttpResponse> for HttpResponse {
    fn from(value: ProtoHttpResponse) -> Self {
        let ProtoHttpResponse {
            request_id,
            status_code,
            headers,
            payload,
        } = value;
        Self {
            request_id,
            status_code: status_code as u16,
            headers,
            payload: payload.map_or(Vec::new(), |p| p),
        }
    }
}

impl TryFrom<proto::WebSocket> for WebSocket {
    type Error = ProtoError;

    fn try_from(value: proto::WebSocket) -> Result<Self, Self::Error> {
        let proto::WebSocket { socket_id, message } = value;

        let Some(msg) = message else {
            return Err(Self::Error::EmptyWsMessage);
        };

        let message = match msg {
            ProtoWsMessage::Text(data) => WebSocketMessage::text(data),
            ProtoWsMessage::Binary(data) => WebSocketMessage::binary(data),
            ProtoWsMessage::Ping(data) => WebSocketMessage::ping(data),
            ProtoWsMessage::Pong(data) => WebSocketMessage::pong(data),
            ProtoWsMessage::Close(close) => {
                WebSocketMessage::close(close.code as u16, close.reason)
            }
        };

        Ok(Self { socket_id, message })
    }
}
