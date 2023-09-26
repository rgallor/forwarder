//! Implement the interaction with the [Astarte rust SDK](astarte_device_sdk).
//!
//! Module responsible for handling a connection between a Device and Astarte.

use std::{
    collections::HashMap,
    net::{self, AddrParseError},
    num::TryFromIntError,
};

use astarte_device_sdk::{
    error::Error as AstarteError,
    options::{AstarteOptions, OptionsError},
    store::memory::MemoryStore,
    types::AstarteType,
    AstarteAggregate, AstarteDeviceSdk,
};
use displaydoc::Display;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{error, info, instrument, warn};
use url::Url;

use crate::device::DeviceError;

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

/// Struct containing the configuration for an Astarte device.
#[derive(Serialize, Deserialize)]
pub struct DeviceConfig {
    realm: String,
    device_id: String,
    credentials_secret: String,
    pairing_url: String,
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
    ) -> Result<std::collections::HashMap<String, AstarteType>, AstarteError> {
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
            "ws://{}:{}?session_token={}",
            ip, value.port, value.session_token
        ))
        .map_err(Error::Parse)
    }
}

/// Read the device configuration file.
async fn read_device_config(device_cfg_path: &str) -> Result<DeviceConfig, Error> {
    let file = tokio::fs::read(device_cfg_path).await?;
    let file = std::str::from_utf8(&file)?;

    let cfg: DeviceConfig = serde_json::from_str(file).map_err(|_| Error::Serde)?;

    Ok(cfg)
}

/// Use the device configuration to create a new instance of an Astarte device and connect it to Astarte.
async fn create_astarte_device(cfg: &DeviceConfig) -> Result<AstarteDeviceSdk<MemoryStore>, Error> {
    let sdk_options = AstarteOptions::new(
        &cfg.realm,
        &cfg.device_id,
        &cfg.credentials_secret,
        &cfg.pairing_url,
    )
    .interface_directory("./forwarder/forwarder/interfaces")
    .map_err(Error::AstarteInterface)?
    .ignore_ssl_errors();

    let device = AstarteDeviceSdk::new(sdk_options).await?;

    Ok(device)
}

/// Parse an `HashMap` containig pairs (Endpoint, [`AstarteType`]) into an URL.
#[instrument(skip_all)]
fn retrieve_url(map: &HashMap<String, AstarteType>) -> Result<Url, Error> {
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

/// Handle a single Astarte event.
pub async fn handle_astarte_events(tx_url: UnboundedSender<Url>) -> Result<(), DeviceError> {
    let dev_config = read_device_config("../device.json").await?;
    let mut dev = create_astarte_device(&dev_config).await?;

    loop {
        // Wait for an aggregate datastream containing a host, a port number and a session token
        match dev.handle_events().await {
            Ok(data) => {
                match data.data {
                    astarte_device_sdk::Aggregation::Object(map) if data.path == "/shell" => {
                        let url = retrieve_url(&map)?;
                        info!("received url: {}", url);

                        // send the retrieved URL to the task responsible for the creation of a new connection
                        tx_url.send(url).expect("failed to send over channel");
                    }
                    _ => {
                        // when an error is returned, tx_url is dropped. This is fundamental because the task
                        // responsible for handling the connections will stop spawning other connections
                        // after having received None from the channel
                        return Err(Error::AstarteWrongAggregation.into());
                    }
                }
            }
            // If the device get disconnected from Astarte, it will loop untill it is reconnected
            // TODO: check that the reconnection is effectively performed
            Err(err @ AstarteError::ConnectionError(_)) => {
                warn!("Reconnecting after error, {:#?}", err);
            }
            Err(err) => {
                error!("Astarte error: {:?}", err);
                return Err(Error::AstarteHandleEvent(err).into());
            }
        }
    }
}
