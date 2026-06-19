#![allow(missing_docs)]

use super::Interconnect;
use crate::{model::Event as GatewayEvent, ws::WsStream, ConnectionInfo};

pub enum WsMessage {
    Ws(Box<WsStream>),
    ReplaceInterconnect(Interconnect),
    SetKeepalive(f64),
    Speaking(bool),
    Deliver(GatewayEvent),
    /// Refresh endpoint/token/session without rebuilding the UDP voice path.
    UpdateInfo(ConnectionInfo),
}
