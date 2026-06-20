use crate::audio_pipe::{buffer_initial_bytes_with_timeout_min, PrefixedPipe};
use crate::playback::{GuildPlayback, PlaybackModes};
use serenity::all::GuildId;
use songbird::events::{CoreEvent, Event, EventContext, EventHandler as SongbirdEventHandler, TrackEvent};
use songbird::input::core::io::ReadOnlySource;
use songbird::input::RawAdapter;
use songbird::input::codecs::{get_codec_registry, get_probe};
use songbird::input::{ChildContainer, Input};
use songbird::tracks::Track;
use songbird::{Call, Event as SongbirdEvent, Songbird};
use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read};
use std::process::{Child, Stdio};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

/// Debounce overlapping recovery attempts (DAVE transitions, track end, user leave).
static CAPTURE_RECOVERY_INFLIGHT: LazyLock<Mutex<HashSet<u64>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
const CAPTURE_RECOVERY_DELAY: Duration = Duration::from_millis(800);

/// Default capture bus — must match the B* button lit on the Voicemeeter Input strip.
pub const DEFAULT_DEVICE: &str = "Voicemeeter Out B2 (VB-Audio Voicemeeter VAIO)";

const CAPTURE_SAMPLE_RATE: u32 = 48_000;
const CAPTURE_CHANNELS: u32 = 2;
/// ~85 ms of stereo f32le at 48 kHz — enough to open the stream without blocking.
const CAPTURE_PCM_PREBUFFER_BYTES: usize = 32 * 1024;

/// Windows dshow allows only one ffmpeg capture per device at a time.
static CAPTURE_DEVICE_LOCK: Mutex<()> = Mutex::new(());

fn with_device_capture<R, F: FnOnce() -> R>(f: F) -> R {
    let _guard = CAPTURE_DEVICE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f()
}

/// Device ffmpeg captures from (`STREAM_AUDIO_DEVICE` or [`DEFAULT_DEVICE`]).
pub fn capture_device() -> String {
    std::env::var("STREAM_AUDIO_DEVICE").unwrap_or_else(|_| DEFAULT_DEVICE.to_string())
}

pub fn capture_volume() -> f32 {
    std::env::var("STREAM_VOLUME")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.99)
}

/// Marker that a Spotify stream is active.
pub struct StreamSession;

/// Probe whether ffmpeg can hear audio on the configured device (run while Spotify plays).
pub fn probe_capture(device: &str, volume: f32, seconds: u64) -> Result<String, String> {
    with_device_capture(|| {
        let child = spawn_ffmpeg_pcm(device, volume)?;
        let mut container = ChildContainer::new(vec![child]);
        let mut buf = [0u8; 8192];
        let start = std::time::Instant::now();
        let deadline = Duration::from_secs(seconds);
        let mut total = 0usize;

        while start.elapsed() < deadline {
            match container.read(&mut buf) {
                Ok(0) => std::thread::sleep(Duration::from_millis(50)),
                Ok(n) => total += n,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(format!("Read error: {e}")),
            }
        }

        drop(container);

        Ok(format!(
            "device=`{device}`\nbytes_read={total}\npcm_stream={}\n\
             (Play Spotify first, then run probe. Need bytes > 0 and pcm_stream=true.)",
            total > 0
        ))
    })
}

/// f32le PCM — no container header, suitable for songbird's [`RawAdapter`].
fn spawn_ffmpeg_pcm(device: &str, volume: f32) -> Result<Child, String> {
    let mut ffmpeg = std::process::Command::new("ffmpeg");
    ffmpeg.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-thread_queue_size",
        "4096",
        "-f",
        "dshow",
        "-audio_buffer_size",
        "50",
        "-i",
        &format!("audio={device}"),
        "-af",
        &format!("aresample={CAPTURE_SAMPLE_RATE},volume={volume}"),
        "-ac",
        &CAPTURE_CHANNELS.to_string(),
        "-ar",
        &CAPTURE_SAMPLE_RATE.to_string(),
        "-c:a",
        "pcm_f32le",
        "-f",
        "f32le",
        "pipe:1",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());

    ffmpeg
        .spawn()
        .map_err(|e| format!("Failed to start ffmpeg: {e}"))
}

fn promote_live_input(input: Input) -> Result<Input, String> {
    match input {
        Input::Live(live, rec) => {
            let promoted = live
                .promote(get_codec_registry(), get_probe())
                .map_err(|e| format!("Failed to parse Voicemeeter PCM stream: {e}"))?;
            Ok(Input::Live(promoted, rec))
        }
        other => Ok(other),
    }
}

/// Capture Voicemeeter → f32le PCM → songbird RawAdapter → parsed live input.
fn spawn_voicemeeter_playable(device: &str, volume: f32) -> Result<Input, String> {
    with_device_capture(|| {
        let child = spawn_ffmpeg_pcm(device, volume)?;
        let mut container = ChildContainer::new(vec![child]);
        let prefix = buffer_initial_bytes_with_timeout_min(
            &mut container,
            Duration::from_secs(15),
            CAPTURE_PCM_PREBUFFER_BYTES,
        )
        .map_err(|e| {
            format!(
                "No PCM audio from \"{device}\": {e}\n\
                 (Is Spotify playing? Is the correct Voicemeeter bus lit?)"
            )
        })?;

        tracing::info!(
            "Voicemeeter capture ready: {} bytes f32 PCM pre-buffered from \"{device}\"",
            prefix.len()
        );

        let prefixed = PrefixedPipe::new(prefix, container);
        let raw = RawAdapter::new(
            ReadOnlySource::new(prefixed),
            CAPTURE_SAMPLE_RATE,
            CAPTURE_CHANNELS,
        );
        let playable = promote_live_input(Input::from(raw))?;
        tracing::info!("Voicemeeter capture parsed for Discord playback");
        Ok(playable)
    })
}

async fn enqueue_guild_capture(
    songbird: &Arc<Songbird>,
    guild_id: GuildId,
    replace_existing: bool,
) -> Result<(), String> {
    let device = capture_device();
    let volume = capture_volume();

    if songbird.get(guild_id).is_none() {
        return Err("Bot is not in a voice channel.".into());
    }

    tracing::info!(
        "Spotify stream: opening capture on \"{device}\"{}",
        if replace_existing { " (recovery)" } else { "" }
    );
    let input = tokio::task::spawn_blocking(move || spawn_voicemeeter_playable(&device, volume))
        .await
        .map_err(|e| format!("Capture task failed: {e}"))??;

    let track = Track::from(input).volume(crate::music::PLAYBACK_VOLUME);

    let handler_lock = songbird
        .get(guild_id)
        .ok_or("Voice handler missing.")?;

    if replace_existing {
        let handler = handler_lock.lock().await;
        handler.queue().stop();
    }

    let mut handler = handler_lock.lock().await;
    if !handler.is_voice_driver_active() {
        tracing::info!("Voice driver inactive — reconnecting");
        if !handler.reconnect_voice_driver_if_inactive() {
            return Err("Voice connection lost. /join then /spotify play.".into());
        }
        drop(handler);
        crate::music::wait_for_voice_driver(songbird, guild_id, Duration::from_secs(15))
            .await?;
        handler = handler_lock.lock().await;
    }

    let track_handle = handler.enqueue(track).await;
    tracing::info!("Spotify capture enqueued for guild {guild_id}");

    match track_handle.get_info().await {
        Ok(info) => tracing::info!(
            "Spotify track after enqueue: playing={:?} ready={:?}",
            info.playing,
            info.ready
        ),
        Err(e) => tracing::warn!("Could not read Spotify track info: {e}"),
    }

    Ok(())
}

/// Capture VoiceMeeter output and stream it to Discord.
///
/// Caller must already have joined voice (`ensure_in_voice`) so DAVE can finish before capture.
pub async fn start_guild_stream(
    songbird: &Arc<Songbird>,
    guild_id: GuildId,
    idle: Arc<crate::music::VoiceIdleManager>,
    recovery_registered: &Arc<AsyncMutex<HashSet<u64>>>,
    stream_sessions: Arc<AsyncMutex<HashMap<u64, StreamSession>>>,
    playback_modes: Arc<PlaybackModes>,
) -> Result<StreamSession, String> {
    enqueue_guild_capture(songbird, guild_id, false).await?;
    idle.cancel(guild_id).await;

    if let Some(handler_lock) = songbird.get(guild_id) {
        let mut handler = handler_lock.lock().await;
        register_capture_recovery_events(
            &mut handler,
            guild_id,
            songbird.clone(),
            idle,
            stream_sessions,
            playback_modes,
            recovery_registered,
        )
        .await;
    }

    Ok(StreamSession)
}

/// Re-open Voicemeeter capture after DAVE or voice events stop the active track.
pub async fn restart_guild_capture(
    songbird: &Arc<Songbird>,
    guild_id: GuildId,
    idle: Arc<crate::music::VoiceIdleManager>,
) -> Result<(), String> {
    enqueue_guild_capture(songbird, guild_id, true).await?;
    idle.cancel(guild_id).await;
    Ok(())
}

/// Schedule a debounced capture restart while a Spotify stream session is active.
pub async fn schedule_capture_recovery(
    reason: &'static str,
    songbird: Arc<Songbird>,
    guild_id: GuildId,
    idle: Arc<crate::music::VoiceIdleManager>,
    stream_sessions: Arc<AsyncMutex<HashMap<u64, StreamSession>>>,
    playback_modes: Arc<PlaybackModes>,
) {
    if !stream_sessions.lock().await.contains_key(&guild_id.get()) {
        return;
    }
    if playback_modes.get(guild_id).await != GuildPlayback::Spotify {
        return;
    }

    let acquired = {
        let mut inflight = CAPTURE_RECOVERY_INFLIGHT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        inflight.insert(guild_id.get())
    };
    if !acquired {
        return;
    }

    tokio::spawn(async move {
        tokio::time::sleep(CAPTURE_RECOVERY_DELAY).await;

        let still_active = stream_sessions.lock().await.contains_key(&guild_id.get())
            && playback_modes.get(guild_id).await == GuildPlayback::Spotify;

        if still_active {
            match restart_guild_capture(&songbird, guild_id, idle).await {
                Ok(()) => tracing::info!(
                    "Spotify capture recovered for guild {guild_id} ({reason})"
                ),
                Err(e) => tracing::warn!(
                    "Spotify capture recovery failed for guild {guild_id} ({reason}): {e}"
                ),
            }
        }

        CAPTURE_RECOVERY_INFLIGHT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&guild_id.get());
    });
}

#[derive(Clone)]
struct CaptureRecovery {
    reason: &'static str,
    guild_id: GuildId,
    songbird: Arc<Songbird>,
    idle: Arc<crate::music::VoiceIdleManager>,
    stream_sessions: Arc<AsyncMutex<HashMap<u64, StreamSession>>>,
    playback_modes: Arc<PlaybackModes>,
}

#[async_trait::async_trait]
impl SongbirdEventHandler for CaptureRecovery {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<SongbirdEvent> {
        schedule_capture_recovery(
            self.reason,
            self.songbird.clone(),
            self.guild_id,
            self.idle.clone(),
            self.stream_sessions.clone(),
            self.playback_modes.clone(),
        )
        .await;
        None
    }
}

/// Watch for capture track loss and restart Voicemeeter → Discord streaming.
pub async fn register_capture_recovery_events(
    handler: &mut Call,
    guild_id: GuildId,
    songbird: Arc<Songbird>,
    idle: Arc<crate::music::VoiceIdleManager>,
    stream_sessions: Arc<AsyncMutex<HashMap<u64, StreamSession>>>,
    playback_modes: Arc<PlaybackModes>,
    recovery_registered: &Arc<AsyncMutex<HashSet<u64>>>,
) {
    if !recovery_registered.lock().await.insert(guild_id.get()) {
        return;
    }

    let recovery = CaptureRecovery {
        reason: "track-event",
        guild_id,
        songbird,
        idle,
        stream_sessions,
        playback_modes,
    };

    handler.add_global_event(Event::Track(TrackEvent::End), recovery.clone());
    handler.add_global_event(Event::Track(TrackEvent::Error), recovery.clone());
    handler.add_global_event(
        Event::Core(CoreEvent::DriverDisconnect),
        CaptureRecovery {
            reason: "driver-disconnect",
            ..recovery
        },
    );
}

pub async fn stop_guild_stream(
    songbird: &Arc<Songbird>,
    guild_id: GuildId,
    sessions: &Arc<AsyncMutex<HashMap<u64, StreamSession>>>,
) -> Result<(), String> {
    sessions.lock().await.remove(&guild_id.get());

    if let Some(handler_lock) = songbird.get(guild_id) {
        let mut handler = handler_lock.lock().await;
        handler.queue().stop();
        handler.stop();
        tracing::info!("Stopped Spotify stream for guild {guild_id}");
    }

    Ok(())
}
