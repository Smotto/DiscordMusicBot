use crate::{
    error::JsonError,
    model::{deserialize_binary_event, Event},
};

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serenity_voice_model::{serialize_binary_event, BinaryError};
use tokio::{
    net::TcpStream,
    time::{timeout, Duration},
};
#[cfg(feature = "tungstenite")]
use tokio_tungstenite::{
    tungstenite::{
        error::Error as TungsteniteError,
        protocol::{CloseFrame, WebSocketConfig as Config},
        Message,
    },
    MaybeTlsStream,
    WebSocketStream,
};
#[cfg(feature = "tws")]
use tokio_websockets::{
    CloseCode,
    Error as TwsError,
    Limits,
    MaybeTlsStream,
    Message,
    WebSocketStream,
};
use tracing::{info, instrument};
use url::Url;

pub struct WsStream(WebSocketStream<MaybeTlsStream<TcpStream>>);

impl WsStream {
    #[instrument]
    pub(crate) async fn connect(url: Url) -> Result<Self> {
        #[cfg(feature = "tungstenite")]
        let (stream, _) = tokio_tungstenite::connect_async_with_config::<Url>(
            url,
            Some(
                Config::default()
                    .max_message_size(None)
                    .max_frame_size(None),
            ),
            true,
        )
        .await?;
        #[cfg(feature = "tws")]
        let (stream, _) = tokio_websockets::ClientBuilder::new()
            .limits(Limits::unlimited())
            .uri(url.as_str())
            .unwrap() // Any valid URL is a valid URI.
            .connect()
            .await?;

        Ok(Self(stream))
    }

    pub(crate) async fn recv_event(&mut self) -> Result<Option<Event>> {
        self.recv_event_with_timeout(Duration::from_millis(500)).await
    }

    pub(crate) async fn recv_event_with_timeout(&mut self, timeout_duration: Duration) -> Result<Option<Event>> {
        let ws_message = if timeout_duration.is_zero() {
            // Non-blocking: try immediately
            match self.0.next().await {
                Some(Ok(v)) => Some(v),
                Some(Err(e)) => return Err(e.into()),
                None => None,
            }
        } else {
            match timeout(timeout_duration, self.0.next()).await {
                Ok(Some(Ok(v))) => Some(v),
                Ok(Some(Err(e))) => return Err(e.into()),
                Ok(None) | Err(_) => None,
            }
        };

        // Log binary message types before conversion so we can see DAVE events in release
        #[cfg(feature = "tungstenite")]
        if let Some(Message::Binary(ref bytes)) = ws_message {
            info!("WS binary received: {} bytes, first={:02X?}", bytes.len(), &bytes[..bytes.len().min(8)]);
        }

        convert_ws_message(ws_message)
    }

    pub(crate) async fn send_json(&mut self, value: &Event) -> Result<()> {
        let res = crate::json::to_string(value);
        let res = res.map(Message::text);
        Ok(res.map_err(Error::from).map(|m| self.0.send(m))?.await?)
    }

    pub(crate) async fn send_binary(&mut self, value: &Event) -> Result<()> {
        let res = serialize_binary_event(value);
        let res = res.map(Message::binary);

        Ok(res.map_err(Error::from).map(|m| self.0.send(m))?.await?)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Json(JsonError),

    /// The discord voice gateway does not support or offer zlib compression.
    /// As a result, only text messages are expected.
    UnexpectedBinaryMessage(Bytes),

    #[cfg(feature = "tungstenite")]
    Ws(Box<TungsteniteError>),
    #[cfg(feature = "tws")]
    Ws(TwsError),

    #[cfg(feature = "tungstenite")]
    WsClosed(Option<CloseFrame>),
    #[cfg(feature = "tws")]
    WsClosed(Option<CloseCode>),

    Binary(BinaryError),
}

impl From<JsonError> for Error {
    fn from(e: JsonError) -> Error {
        Error::Json(e)
    }
}

#[cfg(feature = "tungstenite")]
impl From<TungsteniteError> for Error {
    fn from(e: TungsteniteError) -> Error {
        Error::Ws(Box::new(e))
    }
}

#[cfg(feature = "tws")]
impl From<TwsError> for Error {
    fn from(e: TwsError) -> Self {
        Error::Ws(e)
    }
}

impl From<BinaryError> for Error {
    fn from(value: BinaryError) -> Self {
        Error::Binary(value)
    }
}

/// Strip the `seq` field from a voice gateway JSON payload using serde_json's RawValue.
/// Discord's voice gateway includes a `seq` field that the Event deserializer
/// doesn't properly consume, causing silent parse failures.
fn strip_seq_from_payload(payload: &str) -> String {
    // Parse as a raw JSON value, remove the "seq" key, then serialize back.
    // Discord's voice gateway includes a `seq` field that the Event deserializer
    // doesn't properly consume, causing silent parse failures.
    let mut value: crate::json::value::Value = match crate::json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return payload.to_string(),
    };
    if let Some(obj) = value.as_object_mut() {
        obj.remove("seq");
    }
    crate::json::to_string(&value).unwrap_or_else(|_| payload.to_string())
}

#[inline]
pub(crate) fn convert_ws_message(message: Option<Message>) -> Result<Option<Event>> {
    #[cfg(feature = "tungstenite")]
    match message {
        Some(Message::Text(ref payload)) => {
            // Try normal deserialization first, then try with seq field stripped.
            // Discord's voice gateway includes a `seq` field that the Event deserializer
            // doesn't properly consume, causing silent parse failures.
            let event = if serde_json::from_str::<Event>(payload).is_ok() {
                serde_json::from_str::<Event>(payload).ok()
            } else {
                // Strip the "seq":<number> field from the payload
                let stripped = strip_seq_from_payload(payload);
                info!("seq-stripped: {} -> {}", payload, stripped);
                serde_json::from_str::<Event>(&stripped).ok()
            };
            if event.is_none() {
                info!("Failed to parse voice JSON event. Payload: {payload}");
            }
            return Ok(event);
        },
        Some(Message::Binary(bytes)) => {
            // Discord sends server-to-client DAVE binary events with format:
            // [sequence_number: u16][opcode: u8][payload]
            // But deserialize_binary_event reads data[0] as the opcode (expects no prefix).
            // Skip the 2-byte sequence number so the deserializer sees the real opcode.
            let data = if bytes.len() >= 3 {
                &bytes[2..]
            } else {
                &bytes[..]
            };

            // Discard binary messages with unknown opcodes - Discord may send
            // internal protocol messages we don't need to process.
            // Only error on actual parse failures, not unrecognized opcodes.
            match deserialize_binary_event(data) {
                Ok(event) => return Ok(Some(event)),
                Err(crate::model::BinaryError::InvalidOpcode(_)) => {
                    info!("Discarding unknown binary opcode: {:02X?}", &bytes[..bytes.len().min(16)]);
                    return Ok(None);
                },
                Err(e) => {
                    info!("Unexpected binary: {e}. Bytes: {:02X?}", &bytes[..bytes.len().min(16)]);
                    return Ok(None);
                },
            }
        },
        Some(Message::Close(Some(frame))) => {
            return Err(Error::WsClosed(Some(frame)));
        },
        // Ping/Pong message behaviour is internally handled by tungstenite.
        _ => return Ok(None),
    };

    #[cfg(feature = "tws")]
    match message {
        Some(ref message) if message.is_text() => {
            return if let Some(text) = message.as_text() {
                let event = serde_json::from_str(text)
                    .ok()
                    .or_else(|| {
                        let stripped = strip_seq_from_payload(text);
                        serde_json::from_str::<Event>(&stripped).ok()
                    });
                if event.is_none() {
                    info!("Failed to parse voice JSON event. Payload: {text}");
                }
                Ok(event)
            } else {
                Ok(None)
            };
        },
        Some(message) if message.is_binary() => {
            let payload = message.into_payload();
            let data = if payload.len() >= 3 {
                &payload[2..]
            } else {
                &payload[..]
            };
            match deserialize_binary_event(data) {
                Ok(event) => return Ok(Some(event)),
                Err(crate::model::BinaryError::InvalidOpcode(_)) => {
                    info!("Discarding unknown binary opcode: {:02X?}", &payload[..payload.len().min(16)]);
                    return Ok(None);
                },
                Err(e) => {
                    info!("Unexpected binary: {e}. Bytes: {:02X?}", &payload[..payload.len().min(16)]);
                    return Ok(None);
                },
            }
        },
        Some(message) if message.is_close() => {
            return Err(Error::WsClosed(message.as_close().map(|(c, _)| c)));
        },
        // ping/pong; will also be internally handled by tokio-websockets.
        _ => return Ok(None),
    };
}
