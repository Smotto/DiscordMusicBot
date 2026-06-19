#![allow(missing_docs)]

use crate::{
    driver::{connection::error::Error, Bitrate, Config},
    events::{context_data::DisconnectReason, EventData},
    tracks::{Track, TrackCommand, TrackHandle},
    ConnectionInfo,
};
use flume::{Receiver, Sender};

pub enum CoreMessage {
    ConnectWithResult(ConnectionInfo, Sender<Result<(), Error>>),
    RetryConnect(usize),
    SignalWsClosure(usize, ConnectionInfo, Option<DisconnectReason>),
    Disconnect,
    SetTrack(Option<Box<TrackContext>>),
    AddTrack(Box<TrackContext>),
    SetBitrate(Bitrate),
    AddEvent(EventData),
    RemoveGlobalEvents,
    SetConfig(Config),
    Mute(bool),
    Reconnect,
    /// Push refreshed voice gateway credentials to the live websocket task.
    UpdateConnectionInfo(ConnectionInfo),
    FullReconnect,
    RebuildInterconnect,
    #[cfg(feature = "receive")]
    MarkSsrcMapped(u32),
    Poison,
}

pub struct TrackContext {
    pub track: Track,
    pub handle: TrackHandle,
    pub receiver: Receiver<TrackCommand>,
}
