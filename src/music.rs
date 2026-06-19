use serenity::all::{
    ChannelId, CommandDataOptionValue, CommandInteraction, Context, GuildId, Permissions,
    UserId,
};
use songbird::events::{CoreEvent, Event, EventContext, EventHandler as SongbirdEventHandler, TrackEvent};
use songbird::input::{AudioStream, ChildContainer, Compose, Input, LiveInput, YoutubeDl};
use songbird::input::codecs::{get_codec_registry, get_probe};
use songbird::tracks::{PlayMode, Track};
use songbird::{Call, Event as SongbirdEvent, Songbird};
use std::collections::HashSet;
use std::io::{Cursor, ErrorKind, Read};
use std::process::Stdio;
use songbird::input::core::io::{MediaSource, ReadOnlySource};
use std::sync::Arc;
use std::time::Duration;

/// Playback volume on the track handle. Keep at 1.0 so songbird can Opus-passthrough;
/// loudness is normalized in the ffmpeg filter chain instead.
const PLAYBACK_VOLUME: f32 = 1.0;

struct LogTrackEvents;

#[async_trait::async_trait]
impl SongbirdEventHandler for LogTrackEvents {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<SongbirdEvent> {
        if let EventContext::Track(states) = ctx {
            for (state, _) in *states {
                match &state.playing {
                    PlayMode::Play => tracing::info!("Track started playing"),
                    PlayMode::Errored(e) => tracing::error!("Track playback error: {e:?}"),
                    other if other.is_done() => tracing::info!("Track finished: {other:?}"),
                    other => tracing::debug!("Track state: {other:?}"),
                }
            }
        }
        None
    }
}

struct LogVoiceEvents;

#[async_trait::async_trait]
impl SongbirdEventHandler for LogVoiceEvents {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<SongbirdEvent> {
        match ctx {
            EventContext::DriverDisconnect(data) => {
                tracing::warn!(
                    "Voice driver disconnected: kind={:?} reason={:?}",
                    data.kind,
                    data.reason
                );
            }
            EventContext::DriverReconnect(data) => {
                tracing::info!(
                    "Voice driver reconnected to {} (audio should continue)",
                    data.server
                );
            }
            EventContext::DriverConnect(data) => {
                tracing::info!("Voice driver connected to {}", data.server);
            }
            _ => {}
        }
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

/// Reads from the ffmpeg stdout until an Ogg page header is available so symphonia
/// does not probe an empty pipe (`probe reach EOF at 0 bytes`).
struct PrefixedPipe {
    prefix: Cursor<Vec<u8>>,
    pipe: ChildContainer,
}

impl Read for PrefixedPipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.prefix.read(buf)?;
        if n > 0 {
            return Ok(n);
        }
        self.pipe.read(buf)
    }
}

fn buffer_initial_ogg(container: &mut ChildContainer) -> Result<Vec<u8>, String> {
    const MIN_BYTES: usize = 4096;
    const TIMEOUT: Duration = Duration::from_secs(45);

    let start = std::time::Instant::now();
    let mut buf = vec![0u8; MIN_BYTES];
    let mut filled = 0usize;

    while filled < MIN_BYTES {
        if start.elapsed() > TIMEOUT {
            return Err(
                "Timed out waiting for audio from yt-dlp/ffmpeg (no Ogg data on stdout).".into(),
            );
        }

        match container.read(&mut buf[filled..]) {
            Ok(0) => {
                if filled >= 4 && buf[..filled].starts_with(b"OggS") {
                    buf.truncate(filled);
                    return Ok(buf);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(n) => {
                filled += n;
                if filled >= 4 && buf[..4] != *b"OggS" {
                    return Err("ffmpeg pipe did not produce Ogg/Opus output.".into());
                }
                if filled >= MIN_BYTES {
                    buf.truncate(filled);
                    return Ok(buf);
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("Error reading ffmpeg pipe: {e}")),
        }
    }

    buf.truncate(filled);
    Ok(buf)
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

    let prefixed = PrefixedPipe {
        prefix: Cursor::new(prefix),
        pipe: container,
    };
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

fn register_track_events(handler: &mut Call, registered: &mut HashSet<u64>, guild_id: GuildId) {
    if !registered.insert(guild_id.get()) {
        return;
    }

    handler.add_global_event(
        Event::Track(TrackEvent::Error),
        LogTrackEvents,
    );
    handler.add_global_event(
        Event::Track(TrackEvent::Playable),
        LogTrackEvents,
    );
    handler.add_global_event(Event::Track(TrackEvent::Play), LogTrackEvents);
    handler.add_global_event(Event::Track(TrackEvent::End), LogTrackEvents);
    handler.add_global_event(
        Event::Core(CoreEvent::DriverDisconnect),
        LogVoiceEvents,
    );
    handler.add_global_event(
        Event::Core(CoreEvent::DriverReconnect),
        LogVoiceEvents,
    );
}

pub async fn join_voice(
    songbird: &Arc<Songbird>,
    guild_id: GuildId,
    channel_id: ChannelId,
    registered: &mut HashSet<u64>,
) -> Result<(), String> {
    const MAX_ATTEMPTS: u32 = 3;
    let mut last_err = String::new();

    for attempt in 1..=MAX_ATTEMPTS {
        if songbird.get(guild_id).is_some() {
            tracing::info!("Clearing stale call before join attempt {attempt}");
            songbird.remove(guild_id).await.ok();
            registered.remove(&guild_id.get());
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
                    register_track_events(&mut call, registered, guild_id);
                }
                tracing::info!("Joined voice channel {channel_id} in guild {guild_id}");
                return Ok(());
            }
            Ok(Err(e)) => {
                last_err = e.to_string();
                tracing::warn!("Join attempt {attempt} failed: {last_err}");
                if attempt < MAX_ATTEMPTS && e.should_leave_server() {
                    songbird.remove(guild_id).await.ok();
                    registered.remove(&guild_id.get());
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

async fn ensure_in_voice(
    songbird: &Arc<Songbird>,
    guild_id: GuildId,
    channel_id: ChannelId,
    registered: &mut HashSet<u64>,
) -> Result<(), String> {
    if let Some(handle) = songbird.get(guild_id) {
        let mut call = handle.lock().await;
        if call.current_channel() == Some(channel_id.into()) {
            register_track_events(&mut call, registered, guild_id);
            return Ok(());
        }
        drop(call);
    }

    join_voice(songbird, guild_id, channel_id, registered).await
}

pub async fn play(
    songbird: Arc<Songbird>,
    guild_id: GuildId,
    channel_id: ChannelId,
    query: String,
    http_client: reqwest::Client,
    registered: &mut HashSet<u64>,
) -> Result<String, String> {
    require_ytdlp().await?;
    require_ffmpeg().await?;
    ensure_in_voice(&songbird, guild_id, channel_id, registered).await?;

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
    let track_handle = handler.enqueue(track).await;
    tracing::info!("Enqueued track: {title} (yt-dlp→ffmpeg pipe)");

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

pub async fn stop(songbird: &Arc<Songbird>, guild_id: GuildId) -> Result<(), String> {
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
        .map_err(|e| format!("Failed to leave voice channel: {e}"))
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
