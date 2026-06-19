use crate::{
    error::JsonError,
    model::{deserialize_binary_event, Event},
};

use bytes::Bytes;
use futures::{SinkExt, StreamExt, TryStreamExt};
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

pub struct WsStream {
    inner: WebSocketStream<MaybeTlsStream<TcpStream>>,
    /// Last numbered voice-gateway sequence (v8 `seq_ack` for heartbeats/resume).
    last_seq: Option<i32>,
}

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

        Ok(Self {
            inner: stream,
            last_seq: None,
        })
    }

    #[inline]
    pub(crate) fn last_seq(&self) -> Option<i32> {
        self.last_seq
    }

    pub(crate) async fn recv_event(&mut self) -> Result<Option<Event>> {
        self.recv_event_with_timeout(Duration::from_millis(500)).await
    }

    pub(crate) async fn recv_event_with_seq_no_timeout(&mut self) -> Result<(Option<Event>, Option<u32>)> {
        let message = self.inner.try_next().await?;
        self.note_seq(convert_ws_message(message)?)
    }

    pub(crate) async fn recv_event_with_timeout(&mut self, timeout_duration: Duration) -> Result<Option<Event>> {
        Ok(self
            .recv_event_with_timeout_and_seq(timeout_duration)
            .await?
            .0)
    }

    pub(crate) async fn recv_event_with_timeout_and_seq(
        &mut self,
        timeout_duration: Duration,
    ) -> Result<(Option<Event>, Option<u32>)> {
        let ws_message = if timeout_duration.is_zero() {
            match self.inner.try_next().await? {
                Some(v) => Some(v),
                None => None,
            }
        } else {
            match timeout(timeout_duration, self.inner.next()).await {
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

        self.note_seq(convert_ws_message(ws_message)?)
    }

    fn note_seq(&mut self, parsed: (Option<Event>, Option<u32>)) -> Result<(Option<Event>, Option<u32>)> {
        if let Some(seq) = parsed.1 {
            self.last_seq = Some(seq as i32);
        }
        Ok(parsed)
    }

    pub(crate) async fn send_json(&mut self, value: &Event) -> Result<()> {
        let res = crate::json::to_string(value);
        let res = res.map(Message::text);
        Ok(res.map_err(Error::from).map(|m| self.inner.send(m))?.await?)
    }

    pub(crate) async fn send_text(&mut self, payload: &str) -> Result<()> {
        Ok(self.inner.send(Message::text(payload)).await?)
    }

    pub(crate) async fn send_binary(&mut self, value: &Event) -> Result<()> {
        let res = serialize_binary_event(value);
        let res = res.map(Message::binary);

        Ok(res.map_err(Error::from).map(|m| self.inner.send(m))?.await?)
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
fn normalize_voice_gateway_payload(payload: &str) -> (String, Option<u32>) {
    // Parse as a raw JSON value, remove the "seq" key, then serialize back.
    // Discord's voice gateway includes a `seq` field that the Event deserializer
    // doesn't properly consume, causing silent parse failures.
    let mut value: crate::json::value::Value = match crate::json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return (payload.to_string(), None),
    };
    let seq = value
        .get("seq")
        .and_then(|s| s.as_u64())
        .map(|n| n as u32);
    if let Some(obj) = value.as_object_mut() {
        obj.remove("seq");
        // Gateway v8 wraps heartbeat ACK nonce as `{"t": nonce}` instead of a bare integer.
        if obj.get("op").and_then(|o| o.as_u64()) == Some(6) {
            if let Some(d) = obj.get_mut("d") {
                if let Some(t) = d.get("t").and_then(|t| t.as_u64()) {
                    *d = crate::json::value::Value::Number(t.into());
                }
            }
        }
    }
    (
        crate::json::to_string(&value).unwrap_or_else(|_| payload.to_string()),
        seq,
    )
}

#[inline]
pub(crate) fn convert_ws_message(message: Option<Message>) -> Result<(Option<Event>, Option<u32>)> {
    #[cfg(feature = "tungstenite")]
    match message {
        Some(Message::Text(ref payload)) => {
            let payload = payload.as_str();
            // Try normal deserialization first, then try with seq field stripped.
            // Discord's voice gateway includes a `seq` field that the Event deserializer
            // doesn't properly consume, causing silent parse failures.
            let (event, seq) = if serde_json::from_str::<Event>(payload).is_ok() {
                (
                    serde_json::from_str::<Event>(payload).ok(),
                    crate::json::from_str::<crate::json::value::Value>(payload)
                        .ok()
                        .and_then(|v| v.get("seq").and_then(|s| s.as_u64()))
                        .map(|n| n as u32),
                )
            } else {
                let (normalized, seq) = normalize_voice_gateway_payload(payload);
                if normalized != payload {
                    info!("seq-stripped: {} -> {}", payload, normalized);
                }
                (serde_json::from_str::<Event>(&normalized).ok(), seq)
            };
            if event.is_none() {
                info!("Failed to parse voice JSON event. Payload: {payload}");
            }
            return Ok((event, seq));
        },
        Some(Message::Binary(bytes)) => {
            let seq = if bytes.len() >= 2 {
                Some(u16::from_be_bytes([bytes[0], bytes[1]]) as u32)
            } else {
                None
            };
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
                Ok(event) => return Ok((Some(event), seq)),
                Err(crate::model::BinaryError::InvalidOpcode(_)) => {
                    info!("Discarding unknown binary opcode: {:02X?}", &bytes[..bytes.len().min(16)]);
                    return Ok((None, seq));
                },
                Err(e) => {
                    info!("Unexpected binary: {e}. Bytes: {:02X?}", &bytes[..bytes.len().min(16)]);
                    return Ok((None, seq));
                },
            }
        },
        Some(Message::Close(Some(frame))) => {
            return Err(Error::WsClosed(Some(frame)));
        },
        // Ping/Pong message behaviour is internally handled by tungstenite.
        _ => return Ok((None, None)),
    };

    #[cfg(feature = "tws")]
    match message {
        Some(ref message) if message.is_text() => {
            return if let Some(text) = message.as_text() {
                let (event, seq) = serde_json::from_str::<Event>(text)
                    .ok()
                    .map(|ev| (Some(ev), None))
                    .unwrap_or_else(|| {
                        let (normalized, seq) = normalize_voice_gateway_payload(text);
                        (serde_json::from_str::<Event>(&normalized).ok(), seq)
                    });
                if event.is_none() {
                    info!("Failed to parse voice JSON event. Payload: {text}");
                }
                Ok((event, seq))
            } else {
                Ok((None, None))
            }
        },
        Some(message) if message.is_binary() => {
            let payload = message.into_payload();
            let seq = if payload.len() >= 2 {
                Some(u16::from_be_bytes([payload[0], payload[1]]) as u32)
            } else {
                None
            };
            let data = if payload.len() >= 3 {
                &payload[2..]
            } else {
                &payload[..]
            };
            match deserialize_binary_event(data) {
                Ok(event) => return Ok((Some(event), seq)),
                Err(crate::model::BinaryError::InvalidOpcode(_)) => {
                    info!("Discarding unknown binary opcode: {:02X?}", &payload[..payload.len().min(16)]);
                    return Ok((None, seq));
                },
                Err(e) => {
                    info!("Unexpected binary: {e}. Bytes: {:02X?}", &payload[..payload.len().min(16)]);
                    return Ok((None, seq));
                },
            }
        },
        Some(message) if message.is_close() => {
            return Err(Error::WsClosed(message.as_close().map(|(c, _)| c)));
        },
        // ping/pong; will also be internally handled by tokio-websockets.
        _ => return Ok((None, None)),
    };
}
