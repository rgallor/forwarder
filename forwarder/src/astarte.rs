//! Implement the interaction with the [Astarte rust SDK](astarte_device_sdk).
//!
//! Module responsible for handling a connection between a Device and Astarte.

use std::{
    collections::HashMap,
    fmt::Display,
    net::{self, AddrParseError},
    num::TryFromIntError,
};

use astarte_device_sdk::{
    options::{AstarteOptions, AstarteOptionsError},
    types::AstarteType,
    AstarteAggregate, AstarteDeviceSdk, AstarteError,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::instrument;
use url::Url;

/// Astarte errors.
#[derive(Error, Debug)]
pub enum Error {
    /// Error while loading the Astarte Interfaces.
    #[error("Error while loading the Astarte Interfaces.")]
    AstarteInterface(#[from] AstarteOptionsError),

    /// Error while creating an Astarte device.
    #[error("Error while creating an Astarte device.")]
    AstarteCreateDevice(#[from] AstarteError),

    /// Error while handling an Astarte event.
    #[error("Error while handling an Astarte event.")]
    AstarteHandleEvent(#[source] AstarteError),

    /// Received a wrong Astarte type.
    #[error("Received Individual aggregation data type.")]
    AstarteWrongAggregation,

    /// Error occurring when different fields from those of the mapping are received.
    #[error(
        "Error occurring due to the different fields from those of the mapping have been received."
    )]
    AstarteWrongType,

    /// Error while reading a file.
    #[error("Error while reading a file.")]
    ReadFile(#[from] tokio::io::Error),

    /// Error while serializing/deserializing with serde.
    #[error("Error while serializing/deserializing with serde.")]
    Serde,

    /// Error while parsing an url.
    #[error("Error while parsing url.")]
    Parse(#[from] url::ParseError),

    /// Received a wrong scheme.
    #[error("Wrong scheme, {0}.")]
    ParseScheme(String),

    /// Received a malformed IP address.
    #[error("Error while parsing the ip address.")]
    ParseAddr(#[from] AddrParseError),

    /// Received a malformed port number.
    #[error("Error while parsing the port number.")]
    ParsePort(#[from] TryFromIntError),

    /// Missing url information.
    #[error("Missing url information.")]
    MissingUrlInfo(String),

    /// Error marshaling to UTF8.
    #[error("Error marshaling to UTF8.")]
    Utf8Error(#[from] std::str::Utf8Error),
}

/// Struct containing the configuration for an Astarte device.
#[derive(Serialize, Deserialize)]
pub struct DeviceConfig {
    realm: String,
    device_id: String,
    credentials_secret: String,
    pairing_url: String,
}

#[derive(Debug, Clone, Copy)]
enum Scheme {
    Ws,
    WsSecure,
}

impl Display for Scheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Scheme::Ws => write!(f, "ws"),
            Scheme::WsSecure => write!(f, "wss"),
        }
    }
}

impl TryFrom<&str> for Scheme {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "ws" => Ok(Self::Ws),
            "wss" => Ok(Self::WsSecure),
            _ => Err(Self::Error::ParseScheme(value.to_string())),
        }
    }
}

/// Struct representing the fields of an aggregated object the Astarte server can send to the device.
#[derive(Debug, Clone)]
pub struct DataAggObject {
    scheme: Scheme,
    host: url::Host,
    port: u16,
    session_token: String,
}

impl AstarteAggregate for DataAggObject {
    fn astarte_aggregate(
        self,
    ) -> Result<
        std::collections::HashMap<String, astarte_device_sdk::types::AstarteType>,
        AstarteError,
    > {
        let mut hm = HashMap::new();
        hm.insert("scheme".to_string(), self.scheme.to_string().try_into()?);
        hm.insert("host".to_string(), self.host.to_string().try_into()?);
        hm.insert("port".to_string(), AstarteType::Integer(self.port.into()));
        hm.insert("session_token".to_string(), self.session_token.try_into()?);
        Ok(hm)
    }
}

impl TryFrom<DataAggObject> for Url {
    type Error = Error;

    fn try_from(value: DataAggObject) -> Result<Self, Self::Error> {
        let ip = match value.host {
            url::Host::Domain(domain) => domain,
            // Note: the IP 127.0.0.1 cannot be used due to a low MSRV, therefore the IP is converted to a domain name
            url::Host::Ipv4(ipv4) if net::Ipv4Addr::LOCALHOST == ipv4 => {
                "localhost.local".to_string()
            }
            host => host.to_string(),
        };
        Url::parse(&format!(
            "{}://{}:{}?session_token={}",
            value.scheme, ip, value.port, value.session_token
        ))
        .map_err(Error::Parse)
    }
}

/// Struct responsible for handling the connection between a device and Astarte.
pub struct HandleAstarteConnection;

impl HandleAstarteConnection {
    /// Read the device configuration file.
    pub async fn read_device_config(&self, device_cfg_path: &str) -> Result<DeviceConfig, Error> {
        let file = tokio::fs::read(device_cfg_path).await?;
        let file = std::str::from_utf8(&file)?;

        let cfg: DeviceConfig = serde_json::from_str(file).map_err(|_| Error::Serde)?;

        Ok(cfg)
    }

    /// Use the device configuration to create a new instance of an Astarte device and connect it to Astarte.
    pub async fn create_astarte_device(
        &self,
        cfg: &DeviceConfig,
    ) -> Result<AstarteDeviceSdk, Error> {
        let sdk_options = AstarteOptions::new(
            &cfg.realm,
            &cfg.device_id,
            &cfg.credentials_secret,
            &cfg.pairing_url,
        )
        .interface_directory("./rust-remote-shell/interfaces")
        .map_err(Error::AstarteInterface)?
        .ignore_ssl_errors();

        let device = AstarteDeviceSdk::new(&sdk_options).await?;

        Ok(device)
    }

    /// Parse an `HashMap` containig pairs (Endpoint, [`AstarteType`]) into an URL.
    #[instrument(skip_all)]
    pub fn retrieve_url(map: &HashMap<String, AstarteType>) -> Result<Url, Error> {
        let scheme = map
            .get("scheme")
            .ok_or_else(|| Error::MissingUrlInfo("Missing scheme".to_string()))?;
        let host: &AstarteType = map
            .get("host")
            .ok_or_else(|| Error::MissingUrlInfo("Missing host (IP or domain name)".to_string()))?;
        let port = map
            .get("port")
            .ok_or_else(|| Error::MissingUrlInfo("Missing port value".to_string()))?;
        let session_token = map
            .get("session_token")
            .ok_or_else(|| Error::MissingUrlInfo("Missing session token value".to_string()))?;

        let data = match (scheme, host, port, session_token) {
            (
                AstarteType::String(scheme),
                AstarteType::String(host),
                AstarteType::Integer(port),
                AstarteType::String(session_token),
            ) => {
                let scheme = Scheme::try_from(scheme.as_ref())?;
                let host = url::Host::parse(host)?;
                let port: u16 = (*port).try_into()?;
                let session_token = session_token.to_owned();

                DataAggObject {
                    scheme,
                    host,
                    port,
                    session_token,
                }
            }
            _ => return Err(Error::AstarteWrongType),
        };

        data.try_into()
    }
}
