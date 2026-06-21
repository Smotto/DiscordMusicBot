use crate::audio_pipe::{buffer_initial_ogg, PrefixedPipe};
use crate::playback::{GuildPlayback, PlaybackModes};

use serenity::all::{
    ChannelId, CommandDataOptionValue, CommandInteraction, Context, GuildId, Permissions,
    UserId,
};
use songbird::events::{CoreEvent, Event, EventContext, EventHandler as SongbirdEventHandler, TrackEvent};
use songbird::input::{AudioStream, ChildContainer, Compose, Input, LiveInput, YoutubeDl};
use songbird::input::codecs::{get_codec_registry, get_probe};
use songbird::input::core::io::{MediaSource, ReadOnlySource};
use songbird::tracks::{PlayMode, Track};
use songbird::{Call, Event as SongbirdEvent, Songbird};
use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Leave voice if nothing is queued or playing for this long.
const VOICE_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Slightly below 1.0 so songbird decodes and re-encodes through its Opus encoder.
/// Passthrough + DAVE E2E encryption triggers session invalidation (~8s in).
/// Loudness is normalized in the ffmpeg filter chain instead.
pub const PLAYBACK_VOLUME: f32 = 0.99;

pub struct VoiceIdleManager {
    timers: Mutex<HashMap<u64, tokio::task::AbortHandle>>,
    playback_modes: Arc<PlaybackModes>,
    stream_sessions: Arc<Mutex<HashMap<u64, crate::stream::StreamSession>>>,
}

impl VoiceIdleManager {
    pub fn new(
        playback_modes: Arc<PlaybackModes>,
        stream_sessions: Arc<Mutex<HashMap<u64, crate::stream::StreamSession>>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            timers: Mutex::new(HashMap::new()),
            playback_modes,
            stream_sessions,
        })
    }

    pub async fn cancel(&self, guild_id: GuildId) {
        if let Some(handle) = self.timers.lock().await.remove(&guild_id.get()) {
            handle.abort();
        }
    }

    pub fn playback_modes(&self) -> Arc<PlaybackModes> {
        self.playback_modes.clone()
    }

    pub fn stream_sessions(&self) -> Arc<Mutex<HashMap<u64, crate::stream::StreamSession>>> {
        self.stream_sessions.clone()
    }

    pub async fn schedule(
        self: &Arc<Self>,
        songbird: Arc<Songbird>,
        guild_id: GuildId,
        registered: Arc<Mutex<HashSet<u64>>>,
    ) {
        self.cancel(guild_id).await;

        let idle = Arc::clone(self);
        let playback_modes = self.playback_modes.clone();
        let stream_sessions = self.stream_sessions.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(VOICE_IDLE_TIMEOUT).await;
            if !is_voice_idle(
                &songbird,
                guild_id,
                &playback_modes,
                &stream_sessions,
                "idle_timer",
            )
            .await {
                return;
            }
            tracing::info!(
                "No playback for {} minutes in guild {guild_id} — leaving voice channel",
                VOICE_IDLE_TIMEOUT.as_secs() / 60
            );
            idle_disconnect(&songbird, guild_id, &registered, &stream_sessions).await;
            idle.cancel(guild_id).await;
        });

        self.timers
            .lock()
            .await
            .insert(guild_id.get(), handle.abort_handle());
    }
}

async fn is_voice_idle(
    songbird: &Arc<Songbird>,
    guild_id: GuildId,
    playback_modes: &PlaybackModes,
    stream_sessions: &Arc<Mutex<HashMap<u64, crate::stream::StreamSession>>>,
    diag_context: &str,
) -> bool {
    let mode = playback_modes.get(guild_id).await;
    if mode == GuildPlayback::Spotify {
        crate::audio_diag::log_idle_check(
            guild_id,
            diag_context,
            false,
            mode,
            stream_sessions.lock().await.contains_key(&guild_id.get()),
            false,
            true,
        )
        .await;
        return false;
    }
    let stream_active = stream_sessions.lock().await.contains_key(&guild_id.get());
    if stream_active {
        crate::audio_diag::log_idle_check(
            guild_id,
            diag_context,
            false,
            mode,
            true,
            false,
            true,
        )
        .await;
        return false;
    }

    let Some(handler_lock) = songbird.get(guild_id) else {
        crate::audio_diag::log_idle_check(
            guild_id,
            diag_context,
            false,
            mode,
            false,
            true,
            false,
        )
        .await;
        return false;
    };
    let handler = handler_lock.lock().await;
    let in_channel = handler.current_channel().is_some();
    let queue_empty = handler.queue().current_queue().is_empty();
    let would_idle = in_channel && queue_empty;
    crate::audio_diag::log_idle_check(
        guild_id,
        diag_context,
        would_idle,
        mode,
        false,
        queue_empty,
        in_channel,
    )
    .await;
    would_idle
}

async fn idle_disconnect(
    songbird: &Arc<Songbird>,
    guild_id: GuildId,
    registered: &Arc<Mutex<HashSet<u64>>>,
    stream_sessions: &Arc<Mutex<HashMap<u64, crate::stream::StreamSession>>>,
) {
    if stream_sessions.lock().await.contains_key(&guild_id.get()) {
        crate::audio_diag::warn(format!(
            "idle_disconnect guild={guild_id}: skipped — spotify stream session still active"
        ));
        return;
    }
    crate::audio_diag::log_stop(
        "idle_timeout",
        guild_id,
        "idle_disconnect — leaving voice after empty queue timeout",
    );
    let Some(handler_lock) = songbird.get(guild_id) else {
        return;
    };
    {
        let mut handler = handler_lock.lock().await;
        handler.queue().stop();
        handler.stop();
    }
    match songbird.leave(guild_id).await {
        Ok(()) => tracing::info!("Left voice channel in guild {guild_id} (idle timeout)"),
        Err(e) => tracing::warn!("Idle leave failed for guild {guild_id}: {e}"),
    }
    registered.lock().await.remove(&guild_id.get());
}

#[derive(Clone)]
struct DiagTrackEvents {
    guild_id: GuildId,
    songbird: Arc<Songbird>,
    stream_sessions: Arc<Mutex<HashMap<u64, crate::stream::StreamSession>>>,
    playback_modes: Arc<PlaybackModes>,
}

#[async_trait::async_trait]
impl SongbirdEventHandler for DiagTrackEvents {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<SongbirdEvent> {
        if let EventContext::Track(states) = ctx {
            for (state, handle) in *states {
                let detail = match &state.playing {
                    PlayMode::Play => format!("play uuid={:?}", handle.uuid()),
                    PlayMode::Errored(e) => format!("error uuid={:?} err={e:?}", handle.uuid()),
                    other if other.is_done() => format!("done uuid={:?} mode={other:?}", handle.uuid()),
                    other => format!("state uuid={:?} mode={other:?}", handle.uuid()),
                };
                crate::audio_diag::log_track_event(self.guild_id, "event", &detail);

                if state.playing.is_done() || matches!(state.playing, PlayMode::Errored(_)) {
                    crate::audio_diag::snapshot(
                        "track_end_or_error",
                        self.guild_id,
                        &self.songbird,
                        &self.stream_sessions,
                        &self.playback_modes,
                    )
                    .await;
                }
            }
        }
        None
    }
}

#[derive(Clone)]
struct DiagVoiceEvents {
    guild_id: GuildId,
    songbird: Arc<Songbird>,
    stream_sessions: Arc<Mutex<HashMap<u64, crate::stream::StreamSession>>>,
    playback_modes: Arc<PlaybackModes>,
}

#[async_trait::async_trait]
impl SongbirdEventHandler for DiagVoiceEvents {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<SongbirdEvent> {
        match ctx {
            EventContext::DriverDisconnect(data) => {
                crate::audio_diag::log_voice_core(
                    self.guild_id,
                    "DriverDisconnect",
                    &format!("kind={:?} reason={:?}", data.kind, data.reason),
                );
                crate::audio_diag::snapshot(
                    "driver_disconnect",
                    self.guild_id,
                    &self.songbird,
                    &self.stream_sessions,
                    &self.playback_modes,
                )
                .await;
            }
            EventContext::DriverReconnect(data) => {
                crate::audio_diag::log_voice_core(
                    self.guild_id,
                    "DriverReconnect",
                    &format!("server={}", data.server),
                );
                crate::audio_diag::snapshot(
                    "driver_reconnect",
                    self.guild_id,
                    &self.songbird,
                    &self.stream_sessions,
                    &self.playback_modes,
                )
                .await;
            }
            EventContext::DriverConnect(data) => {
                crate::audio_diag::log_voice_core(
                    self.guild_id,
                    "DriverConnect",
                    &format!("server={}", data.server),
                );
            }
            EventContext::ClientDisconnect(data) => {
                crate::audio_diag::log_voice_core(
                    self.guild_id,
                    "ClientDisconnect",
                    &format!("user={:?}", data.user_id),
                );
                crate::audio_diag::snapshot(
                    "client_disconnect",
                    self.guild_id,
                    &self.songbird,
                    &self.stream_sessions,
                    &self.playback_modes,
                )
                .await;
            }
            EventContext::ClientConnect(data) => {
                crate::audio_diag::log_voice_core(
                    self.guild_id,
                    "ClientConnect",
                    &format!("user={:?} ssrc={}", data.user_id, data.audio_ssrc),
                );
            }
            _ => {}
        }
        None
    }
}

struct IdleDisconnectOnEnd {
    songbird: Arc<Songbird>,
    guild_id: GuildId,
    idle: Arc<VoiceIdleManager>,
    registered: Arc<Mutex<HashSet<u64>>>,
    playback_modes: Arc<PlaybackModes>,
    stream_sessions: Arc<Mutex<HashMap<u64, crate::stream::StreamSession>>>,
}

#[async_trait::async_trait]
impl SongbirdEventHandler for IdleDisconnectOnEnd {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<SongbirdEvent> {
        let songbird = self.songbird.clone();
        let guild_id = self.guild_id;
        let idle = self.idle.clone();
        let registered = self.registered.clone();
        let playback_modes = self.playback_modes.clone();
        let stream_sessions = self.stream_sessions.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            crate::audio_diag::snapshot(
                "idle_disconnect_on_end",
                guild_id,
                &songbird,
                &stream_sessions,
                &playback_modes,
            )
            .await;
            if is_voice_idle(
                &songbird,
                guild_id,
                &playback_modes,
                &stream_sessions,
                "track_end_idle_check",
            )
            .await
            {
                crate::audio_diag::warn(format!(
                    "IdleDisconnectOnEnd guild={guild_id}: scheduling idle leave timer"
                ));
                idle.schedule(songbird, guild_id, registered).await;
            }
        });

        None
    }
}

pub fn user_voice_channel(
    ctx: &Context,
    user_id: UserId,
    guild_id: GuildId,
) -> Option<ChannelId> {
    ctx.cache
        .guild(guild_id)?
        .voice_states
        .get(&user_id)
        .and_then(|vs| vs.channel_id)
}

pub async fn require_ytdlp() -> Result<(), String> {
    match tokio::process::Command::new("yt-dlp").arg("--version").output().await {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout);
            tracing::debug!("yt-dlp version: {}", version.trim());
            Ok(())
        }
        Ok(output) => Err(format!(
            "yt-dlp returned an error: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Err(e) if e.kind() == ErrorKind::NotFound => Err(
            "yt-dlp is not installed or not on your PATH.\n\
             Install it, then restart your terminal:\n\
             • `winget install yt-dlp`\n\
             • or `scoop install yt-dlp`\n\
             • or download from https://github.com/yt-dlp/yt-dlp/releases"
                .into(),
        ),
        Err(e) => Err(format!("Failed to run yt-dlp: {e}")),
    }
}

pub async fn require_ffmpeg() -> Result<(), String> {
    match tokio::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout);
            tracing::debug!("ffmpeg version: {}", version.lines().next().unwrap_or("unknown"));
            Ok(())
        }
        Ok(output) => Err(format!(
            "ffmpeg returned an error: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Err(e) if e.kind() == ErrorKind::NotFound => Err(
            "ffmpeg is not installed or not on your PATH.\n\
             Install it, then restart your terminal:\n\
             • `winget install ffmpeg`\n\
             • or `scoop install ffmpeg`\n\
             • or download from https://ffmpeg.org/download.html"
                .into(),
        ),
        Err(e) => Err(format!("Failed to run ffmpeg: {e}")),
    }
}

/// Pipe yt-dlp into ffmpeg for a steady 48 kHz Opus/Ogg stream (smoother than raw HTTP).
async fn ytdlp_ffmpeg_input(query: &str, is_url: bool) -> Result<songbird::input::Input, String> {
    require_ffmpeg().await?;

    let target = if is_url {
        query.to_owned()
    } else {
        format!("ytsearch1:{query}")
    };

    tokio::task::spawn_blocking(move || spawn_ytdlp_ffmpeg_pipeline(&target))
        .await
        .map_err(|e| format!("Audio pipeline task failed: {e}"))?
}

fn spawn_ytdlp_ffmpeg_pipeline(target: &str) -> Result<Input, String> {
    let mut ytdl = std::process::Command::new("yt-dlp");
    ytdl.args([
        "-f",
        "bestaudio[protocol^=http]/bestaudio/best",
        "--no-playlist",
        "--retries",
        "10",
        "--fragment-retries",
        "10",
        "--http-chunk-size",
        "10485760",
        "-o",
        "-",
        target,
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());

    let mut ytdl_child = ytdl
        .spawn()
        .map_err(|e| format!("Failed to start yt-dlp download pipe: {e}"))?;

    let ytdl_stdout = ytdl_child
        .stdout
        .take()
        .ok_or("yt-dlp did not provide a stdout pipe")?;

    let mut ffmpeg = std::process::Command::new("ffmpeg");
    ffmpeg
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-thread_queue_size",
            "4096",
            "-i",
            "pipe:0",
            "-af",
            "volume=0.35",
            "-c:a",
            "libopus",
            "-b:a",
            "128k",
            "-application",
            "audio",
            "-frame_duration",
            "20",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-flush_packets",
            "1",
            "-f",
            "ogg",
            "pipe:1",
        ])
        .stdin(ytdl_stdout)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let ffmpeg_child = ffmpeg
        .spawn()
        .map_err(|e| format!("Failed to start ffmpeg transcode pipe: {e}"))?;

    tracing::debug!("Started yt-dlp → ffmpeg audio pipeline for {target}");

    let mut container = ChildContainer::new(vec![ytdl_child, ffmpeg_child]);
    let prefix = buffer_initial_ogg(&mut container)?;
    tracing::debug!(
        "Pre-buffered {} bytes of Ogg/Opus from ffmpeg for {target}",
        prefix.len()
    );

    let prefixed = PrefixedPipe::new(prefix, container);
    let audio_stream = AudioStream {
        input: Box::new(ReadOnlySource::new(prefixed)) as Box<dyn MediaSource>,
    };

    Ok(Input::Live(LiveInput::Raw(audio_stream), None))
}

pub async fn diagnose_voice_channel(
    ctx: &Context,
    guild_id: GuildId,
    channel_id: ChannelId,
) -> Result<(), String> {
    let bot_id = ctx.cache.current_user().id;
    let member = guild_id
        .member(&ctx.http, bot_id)
        .await
        .map_err(|e| format!("Failed to fetch bot member: {e}"))?;

    let channels = guild_id
        .channels(&ctx.http)
        .await
        .map_err(|e| format!("Failed to fetch channels: {e}"))?;
    let channel = channels
        .get(&channel_id)
        .ok_or("Voice channel not found.")?;

    let guild = guild_id
        .to_partial_guild(&ctx.http)
        .await
        .map_err(|e| format!("Failed to fetch guild: {e}"))?;
    let perms = guild.user_permissions_in(channel, &member);

    tracing::info!(
        "Voice channel {} kind={:?} perms={:?}",
        channel_id,
        channel.kind,
        perms
    );

    if !perms.contains(Permissions::VIEW_CHANNEL) {
        return Err("I need **View Channel** permission for that voice channel.".into());
    }
    if !perms.contains(Permissions::CONNECT) {
        return Err(
            "I need **Connect** permission. Check Server Settings → Roles for my role, \
             or channel-specific permission overwrites."
                .into(),
        );
    }
    if !perms.contains(Permissions::SPEAK) {
        return Err("I need **Speak** permission in that voice channel.".into());
    }

    if let Some(user_limit) = channel.user_limit {
        if user_limit > 0 {
            let in_channel = ctx
                .cache
                .guild(guild_id)
                .map(|g| {
                    g.voice_states
                        .values()
                        .filter(|vs| vs.channel_id == Some(channel_id))
                        .count()
                })
                .unwrap_or(0);

            if in_channel >= user_limit as usize {
                return Err("That voice channel is full.".into());
            }
        }
    }

    Ok(())
}

async fn register_track_events(
    handler: &mut Call,
    registered: &Arc<Mutex<HashSet<u64>>>,
    guild_id: GuildId,
    songbird: Arc<Songbird>,
    idle: Arc<VoiceIdleManager>,
) {
    let mut set = registered.lock().await;
    if !set.insert(guild_id.get()) {
        return;
    }
    drop(set);

    let stream_sessions = idle.stream_sessions();
    let playback_modes = idle.playback_modes();

    let diag_track = DiagTrackEvents {
        guild_id,
        songbird: songbird.clone(),
        stream_sessions: stream_sessions.clone(),
        playback_modes: playback_modes.clone(),
    };
    let diag_voice = DiagVoiceEvents {
        guild_id,
        songbird: songbird.clone(),
        stream_sessions,
        playback_modes,
    };

    handler.add_global_event(Event::Track(TrackEvent::Error), diag_track.clone());
    handler.add_global_event(Event::Track(TrackEvent::Playable), diag_track.clone());
    handler.add_global_event(Event::Track(TrackEvent::Play), diag_track.clone());
    handler.add_global_event(Event::Track(TrackEvent::End), diag_track);
    handler.add_global_event(
        Event::Track(TrackEvent::End),
        IdleDisconnectOnEnd {
            songbird: diag_voice.songbird.clone(),
            guild_id,
            idle: idle.clone(),
            registered: registered.clone(),
            playback_modes: idle.playback_modes(),
            stream_sessions: idle.stream_sessions(),
        },
    );
    handler.add_global_event(
        Event::Core(CoreEvent::DriverDisconnect),
        diag_voice.clone(),
    );
    handler.add_global_event(
        Event::Core(CoreEvent::DriverReconnect),
        diag_voice.clone(),
    );
    handler.add_global_event(
        Event::Core(CoreEvent::DriverConnect),
        diag_voice.clone(),
    );
    handler.add_global_event(
        Event::Core(CoreEvent::ClientConnect),
        diag_voice.clone(),
    );
    handler.add_global_event(
        Event::Core(CoreEvent::ClientDisconnect),
        diag_voice,
    );
}

pub async fn join_voice(
    songbird: &Arc<Songbird>,
    guild_id: GuildId,
    channel_id: ChannelId,
    registered: Arc<Mutex<HashSet<u64>>>,
    idle: Arc<VoiceIdleManager>,
) -> Result<(), String> {
    if let Some(handle) = songbird.get(guild_id) {
        let mut call = handle.lock().await;
        if call.current_channel() == Some(channel_id.into()) {
            register_track_events(
                &mut call,
                &registered,
                guild_id,
                songbird.clone(),
                idle.clone(),
            )
            .await;
            call.reconnect_voice_driver_if_inactive();
            tracing::info!("Already in voice channel {channel_id} in guild {guild_id}");
            return Ok(());
        }
    }

    const MAX_ATTEMPTS: u32 = 3;
    let mut last_err = String::new();

    for attempt in 1..=MAX_ATTEMPTS {
        if songbird.get(guild_id).is_some() {
            tracing::info!("Clearing stale call before join attempt {attempt}");
            idle.cancel(guild_id).await;
            songbird.remove(guild_id).await.ok();
            registered.lock().await.remove(&guild_id.get());
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }

        tracing::info!(
            "Voice join attempt {attempt}/{MAX_ATTEMPTS} guild={guild_id} channel={channel_id}"
        );

        let sb = songbird.clone();
        let join_result = tokio::spawn(async move { sb.join(guild_id, channel_id).await }).await;

        match join_result {
            Ok(Ok(handle)) => {
                {
                    let mut call = handle.lock().await;
                    register_track_events(
                        &mut call,
                        &registered,
                        guild_id,
                        songbird.clone(),
                        idle.clone(),
                    )
                    .await;
                }
                tracing::info!("Joined voice channel {channel_id} in guild {guild_id}");
                idle.schedule(songbird.clone(), guild_id, registered).await;
                return Ok(());
            }
            Ok(Err(e)) => {
                last_err = e.to_string();
                tracing::warn!("Join attempt {attempt} failed: {last_err}");
                if attempt < MAX_ATTEMPTS && e.should_leave_server() {
                    idle.cancel(guild_id).await;
                    songbird.remove(guild_id).await.ok();
                    registered.lock().await.remove(&guild_id.get());
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                }
            }
            Err(e) => {
                last_err = format!("Join task panicked: {e}");
                tracing::error!("{last_err}");
            }
        }
    }

    Err(format!(
        "Failed to join after {MAX_ATTEMPTS} attempts: {last_err}\n\n\
         Check the console for `VoiceServerUpdate` / `Bot VoiceStateUpdate` lines. \
         If neither appears within 30s, Discord is not accepting the join request."
    ))
}

pub async fn ensure_in_voice(
    songbird: &Arc<Songbird>,
    guild_id: GuildId,
    channel_id: ChannelId,
    registered: Arc<Mutex<HashSet<u64>>>,
    idle: Arc<VoiceIdleManager>,
) -> Result<(), String> {
    if let Some(handle) = songbird.get(guild_id) {
        let mut call = handle.lock().await;
        if call.current_channel() == Some(channel_id.into()) {
            register_track_events(
                &mut call,
                &registered,
                guild_id,
                songbird.clone(),
                idle.clone(),
            )
            .await;
            call.reconnect_voice_driver_if_inactive();
            return Ok(());
        }
        drop(call);
    }

    join_voice(songbird, guild_id, channel_id, registered, idle).await
}

/// Poll until the voice driver reports an active UDP/WebSocket session.
pub async fn wait_for_voice_driver(
    songbird: &Arc<Songbird>,
    guild_id: GuildId,
    timeout: Duration,
) -> Result<(), String> {
    use std::time::Instant;

    let deadline = Instant::now() + timeout;
    let mut consecutive = 0u32;

    while Instant::now() < deadline {
        if let Some(handle) = songbird.get(guild_id) {
            let call = handle.lock().await;
            if call.is_voice_driver_active() {
                consecutive += 1;
                if consecutive >= 5 {
                    tracing::info!("Voice driver active for guild {guild_id}");
                    return Ok(());
                }
            } else {
                consecutive = 0;
            }
        } else {
            consecutive = 0;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Err("Voice driver did not become active in time.".into())
}

pub async fn play(
    songbird: Arc<Songbird>,
    guild_id: GuildId,
    channel_id: ChannelId,
    query: String,
    http_client: reqwest::Client,
    registered: Arc<Mutex<HashSet<u64>>>,
    idle: Arc<VoiceIdleManager>,
    stream_sessions: &Arc<Mutex<HashMap<u64, crate::stream::StreamSession>>>,
) -> Result<String, String> {
    require_ytdlp().await?;
    require_ffmpeg().await?;

    // Clear an active Spotify capture session before YouTube enqueue.
    if stream_sessions.lock().await.contains_key(&guild_id.get()) {
        crate::stream::stop_guild_stream("command:youtube_play", &songbird, guild_id, stream_sessions)
            .await
            .ok();
    }

    ensure_in_voice(
        &songbird,
        guild_id,
        channel_id,
        registered.clone(),
        idle.clone(),
    )
    .await?;

    let is_url = query.starts_with("http");
    let mut ytdl = if is_url {
        YoutubeDl::new(http_client.clone(), query.clone())
    } else {
        YoutubeDl::new_search(http_client, query.clone())
    };

    let metadata = ytdl
        .aux_metadata()
        .await
        .map_err(|e| format!("Failed to fetch audio from YouTube: {e}"))?;
    let title = metadata.title.unwrap_or_else(|| query.clone());

    tracing::info!("Resolved track: {title}");

    let input = ytdlp_ffmpeg_input(&query, is_url).await?;
    let input = input
        .make_playable_async(get_codec_registry(), get_probe())
        .await
        .map_err(|e| format!("Failed to buffer audio stream: {e}"))?;
    let track = Track::from(input).volume(PLAYBACK_VOLUME);

    let handler_lock = songbird
        .get(guild_id)
        .ok_or("Voice handler missing after join.")?;

    let mut handler = handler_lock.lock().await;
    if !handler.is_voice_driver_active() {
        tracing::info!("Voice driver inactive before enqueue — reconnecting");
        if !handler.reconnect_voice_driver_if_inactive() {
            return Err(
                "Voice connection was lost. Use /join again, then /play.".into(),
            );
        }
        drop(handler);
        tokio::time::sleep(Duration::from_millis(750)).await;
        handler = handler_lock.lock().await;
    }

    let track_handle = handler.enqueue(track).await;
    tracing::info!("Enqueued track: {title} (yt-dlp→ffmpeg pipe)");
    idle.cancel(guild_id).await;

    match track_handle.get_info().await {
        Ok(info) => tracing::debug!("Track info after enqueue: {:?}", info.playing),
        Err(e) => tracing::warn!("Could not read track info: {e}"),
    }

    Ok(title)
}

pub async fn skip(songbird: &Arc<Songbird>, guild_id: GuildId) -> Result<(), String> {
    let handler_lock = songbird
        .get(guild_id)
        .ok_or("I'm not connected to a voice channel.")?;
    let handler = handler_lock.lock().await;
    handler
        .queue()
        .skip()
        .map_err(|e| format!("Skip failed: {e}"))
}

pub async fn pause(songbird: &Arc<Songbird>, guild_id: GuildId) -> Result<(), String> {
    let handler_lock = songbird
        .get(guild_id)
        .ok_or("I'm not connected to a voice channel.")?;
    let handler = handler_lock.lock().await;
    handler
        .queue()
        .pause()
        .map_err(|e| format!("Pause failed: {e}"))
}

pub async fn resume(songbird: &Arc<Songbird>, guild_id: GuildId) -> Result<(), String> {
    let handler_lock = songbird
        .get(guild_id)
        .ok_or("I'm not connected to a voice channel.")?;
    let handler = handler_lock.lock().await;
    handler
        .queue()
        .resume()
        .map_err(|e| format!("Resume failed: {e}"))
}

/// Stop playback and leave voice (legacy; prefer [`stop_all`] for unified shutdown).
#[allow(dead_code)]
pub async fn stop(
    songbird: &Arc<Songbird>,
    guild_id: GuildId,
    registered: Arc<Mutex<HashSet<u64>>>,
    idle: Arc<VoiceIdleManager>,
) -> Result<(), String> {
    idle.cancel(guild_id).await;
    let handler_lock = songbird
        .get(guild_id)
        .ok_or("I'm not connected to a voice channel.")?;
    {
        let handler = handler_lock.lock().await;
        handler.queue().stop();
    }
    songbird
        .leave(guild_id)
        .await
        .map_err(|e| format!("Failed to leave voice channel: {e}"))?;
    registered.lock().await.remove(&guild_id.get());
    Ok(())
}

/// Stop all playback (YouTube + Spotify stream), leave voice, and set mode to Idle.
pub async fn stop_all(
    songbird: &Arc<Songbird>,
    guild_id: GuildId,
    registered: Arc<Mutex<HashSet<u64>>>,
    idle: Arc<VoiceIdleManager>,
    stream_sessions: &Arc<Mutex<std::collections::HashMap<u64, crate::stream::StreamSession>>>,
    playback_modes: &Arc<crate::playback::PlaybackModes>,
) -> Result<(), String> {
    crate::audio_diag::log_stop("command:stop_all", guild_id, "stop_all beginning");
    crate::stream::stop_guild_stream("command:stop_all", songbird, guild_id, stream_sessions)
        .await
        .ok();

    idle.cancel(guild_id).await;
    playback_modes
        .set(guild_id, crate::playback::GuildPlayback::Idle)
        .await;

    if let Some(handler_lock) = songbird.get(guild_id) {
        let mut handler = handler_lock.lock().await;
        crate::audio_diag::log_stop(
            "command:stop_all",
            guild_id,
            "calling handler.queue().stop() and handler.stop()",
        );
        handler.queue().stop();
        handler.stop();
    }

    if songbird.get(guild_id).is_some() {
        if let Err(e) = songbird.leave(guild_id).await {
            tracing::warn!("Failed to leave voice channel in guild {guild_id}: {e}");
        }
    }

    registered.lock().await.remove(&guild_id.get());
    Ok(())
}

pub fn option_string(cmd: &CommandInteraction, name: &str) -> Option<String> {
    cmd.data.options.iter().find(|o| o.name == name).and_then(|o| {
        if let CommandDataOptionValue::String(value) = &o.value {
            Some(value.clone())
        } else {
            None
        }
    })
}

/// Read a string option from a slash subcommand (e.g. `/spotify play url:...`).
pub fn subcommand_option_string(
    cmd: &CommandInteraction,
    subcommand: &str,
    name: &str,
) -> Option<String> {
    cmd.data
        .options
        .iter()
        .find(|o| o.name == subcommand)
        .and_then(|o| {
            if let CommandDataOptionValue::SubCommand(sub) = &o.value {
                sub.iter().find(|o| o.name == name).and_then(|o| {
                    if let CommandDataOptionValue::String(value) = &o.value {
                        Some(value.clone())
                    } else {
                        None
                    }
                })
            } else {
                None
            }
        })
}
