use crate::connection::{Id, WsMsg, WsTransmitted};

// Include the `proto` module, which is generated from proto.proto.
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/forwarder.proto.rs"));
}

impl TryFrom<proto::WsTransmitted> for WsTransmitted {
    type Error = ();

    fn try_from(value: proto::WsTransmitted) -> Result<Self, Self::Error> {
        let proto::WsTransmitted { id, msg } = value;

        let id = id.map(|id| id.try_into()).unwrap()?;
        let msg = msg.map(|ws_msg| ws_msg.into()).unwrap();

        let res = WsTransmitted::new(id, msg);
        Ok(res)
    }
}

impl TryFrom<proto::Id> for Id {
    type Error = (); // TODO: return a new error type

    fn try_from(value: proto::Id) -> Result<Self, Self::Error> {
        let proto::Id { port, host } = value;

        let port = u16::try_from(port).map_err(|_| ())?;

        Ok(Id::new(port, host))
    }
}

impl From<proto::ws_msg::WsMsgType> for WsMsg {
    fn from(value: proto::ws_msg::WsMsgType) -> Self {
        match value {
            proto::ws_msg::WsMsgType::NewConnection(_) => WsMsg::NewConnection,
            proto::ws_msg::WsMsgType::Eot(_) => WsMsg::Eot,
            proto::ws_msg::WsMsgType::CloseConnection(_) => WsMsg::CloseConnection,
            proto::ws_msg::WsMsgType::Data(proto::Data { data }) => WsMsg::Data(data),
        }
    }
}

impl From<WsMsg> for proto::ws_msg::WsMsgType {
    fn from(value: WsMsg) -> Self {
        match value {
            WsMsg::NewConnection => Self::NewConnection(proto::NewConnection {}),
            WsMsg::Eot => Self::Eot(proto::Eot {}),
            WsMsg::CloseConnection => Self::CloseConnection(proto::CloseConnection {}),
            WsMsg::Data(data) => Self::Data(proto::Data { data }),
        }
    }
}

impl From<proto::WsMsg> for WsMsg {
    fn from(value: proto::WsMsg) -> Self {
        value
            .ws_msg_type
            .map(|msg_type| msg_type.into())
            .expect("WsMsgType empty")
    }
}
