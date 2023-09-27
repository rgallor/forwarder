//! Implement the interaction with the [Astarte rust SDK](astarte_device_sdk).
//!
//! Module responsible for handling a connection between a Device and Astarte.

use std::{
    collections::HashMap,
    net::{self, AddrParseError},
    num::TryFromIntError,
};

use astarte_device_sdk::{
    AstarteAggregate,
    error::Error as AstarteError,
    options::OptionsError,
    types::AstarteType,
};
use displaydoc::Display;
use thiserror::Error;
use tracing::instrument;
use url::Url;

/// Astarte errors.
#[non_exhaustive]
#[derive(Display, Error, Debug)]
pub enum Error {
    /// Error while loading the Astarte Interfaces, `{0}`.
    AstarteInterface(#[from] OptionsError),

    /// Error while creating an Astarte device, `{0}`.
    AstarteCreateDevice(#[from] AstarteError),

    /// Error while handling an Astarte event, `{0}`.
    AstarteHandleEvent(#[source] AstarteError),

    /// Received a wrong Astarte type.
    AstarteWrongAggregation,

    /// Error occurring when different fields from those of the mapping are received.
    AstarteWrongType,

    /// Error while reading a file, `{0}`.
    ReadFile(#[from] tokio::io::Error),

    /// Error while serializing/deserializing with serde.
    Serde,

    /// Error while parsing an url, `{0}`.
    Parse(#[from] url::ParseError),

    /// Received a wrong scheme, `{0}`.
    ParseScheme(String),

    /// Received a malformed IP address, `{0}`.
    ParseAddr(#[from] AddrParseError),

    /// Received a malformed port number, `{0}`.
    ParsePort(#[from] TryFromIntError),

    /// Missing url information, `{0}`.
    MissingUrlInfo(String),

    /// Error marshaling to UTF8, `{0}`.
    Utf8Error(#[from] std::str::Utf8Error),
}

/// Struct representing the fields of an aggregated object the Astarte server can send to the device.
#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    host: url::Host,
    port: u16,
    session_token: String,
}

impl AstarteAggregate for ConnectionInfo {
    fn astarte_aggregate(
        self,
    ) -> Result<HashMap<String, AstarteType>, AstarteError> {
        let mut hm = HashMap::new();
        hm.insert("host".to_string(), self.host.to_string().into());
        hm.insert("port".to_string(), AstarteType::Integer(self.port.into()));
        hm.insert("session_token".to_string(), self.session_token.into());
        Ok(hm)
    }
}

impl TryFrom<ConnectionInfo> for Url {
    type Error = Error;

    fn try_from(value: ConnectionInfo) -> Result<Self, Self::Error> {
        let ip = match value.host {
            url::Host::Domain(domain) => domain,
            // Note: the IP 127.0.0.1 cannot be used due to a low MSRV, therefore the IP is converted to a domain name
            url::Host::Ipv4(ipv4) if net::Ipv4Addr::LOCALHOST == ipv4 => "localhost".to_string(),
            host => host.to_string(),
        };

        Url::parse(&format!(
            "ws://{}:{}/path?session_token={}",
            ip, value.port, value.session_token
        )).map_err(Error::Parse)
    }
}


/// Parse an `HashMap` containing pairs (Endpoint, [`AstarteType`]) into an URL.
#[instrument(skip_all)]
pub fn retrieve_url(map: &HashMap<String, AstarteType>) -> Result<Url, Error> {
    let host: &AstarteType = map
        .get("host")
        .ok_or_else(|| Error::MissingUrlInfo("Missing host (IP or domain name)".to_string()))?;
    let port = map
        .get("port")
        .ok_or_else(|| Error::MissingUrlInfo("Missing port value".to_string()))?;
    let session_token = map
        .get("session_token")
        .ok_or_else(|| Error::MissingUrlInfo("Missing session_token".to_string()))?;

    let data = match (host, port, session_token) {
        (
            AstarteType::String(host),
            AstarteType::Integer(port),
            AstarteType::String(session_token),
        ) => {
            let session_token = session_token.to_owned();
            let host = url::Host::parse(host)?;
            let port: u16 = (*port).try_into()?;

            ConnectionInfo {
                host,
                port,
                session_token,
            }
        }
        _ => return Err(Error::AstarteWrongType),
    };

    data.try_into()
}
