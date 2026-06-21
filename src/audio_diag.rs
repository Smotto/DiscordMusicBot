//! Structured audio/voice diagnostics — filter with `RUST_LOG=audio_diag=info`.

use crate::playback::{GuildPlayback, PlaybackModes};
use crate::stream::StreamSession;
use serenity::all::{GuildId, VoiceState};
use songbird::Songbird;
use std::collections::HashMap;
use std::fmt::Write as _;
use tokio::sync::Mutex;

pub const TARGET: &str = "audio_diag";

pub fn event(msg: impl AsRef<str>) {
    tracing::info!(target: TARGET, "{}", msg.as_ref());
}

pub fn warn(msg: impl AsRef<str>) {
    tracing::warn!(target: TARGET, "{}", msg.as_ref());
}

fn voice_state_line(vs: &VoiceState) -> String {
    format!(
        "user={} ch={:?} mute={} deaf={} self_mute={} self_deaf={} stream={:?} video={} suppress={}",
        vs.user_id,
        vs.channel_id.map(|c| c.get()),
        vs.mute,
        vs.deaf,
        vs.self_mute,
        vs.self_deaf,
        vs.self_stream,
        vs.self_video,
        vs.suppress,
    )
}

pub fn log_gateway_voice_state(
    label: &str,
    before: Option<&VoiceState>,
    after: &VoiceState,
    bot_channel_id: Option<u64>,
    stream_active: bool,
    playback_mode: GuildPlayback,
) {
    let mut msg = format!("GATEWAY {label}: ");
    match before {
        Some(b) => {
            write!(msg, "before[{}] ", voice_state_line(b)).ok();
        }
        None => msg.push_str("before[none] "),
    }
    write!(msg, "after[{}]", voice_state_line(after)).ok();

    let user_left_bot = before
        .and_then(|b| b.channel_id)
        .map(|c| c.get())
        == bot_channel_id
        && after.channel_id.is_none();
    let user_left_any = before.and_then(|b| b.channel_id).is_some() && after.channel_id.is_none();
    let user_joined_bot = before.and_then(|b| b.channel_id).is_none()
        && after.channel_id.map(|c| c.get()) == bot_channel_id;

    write!(
        msg,
        " | bot_ch={bot_channel_id:?} stream_active={stream_active} mode={playback_mode:?} \
         flags:left_bot={user_left_bot} left_any={user_left_any} joined_bot={user_joined_bot}"
    )
    .ok();

    event(msg);
}

pub async fn snapshot(
    tag: &str,
    guild_id: GuildId,
    songbird: &Songbird,
    stream_sessions: &Mutex<HashMap<u64, StreamSession>>,
    playback_modes: &PlaybackModes,
) {
    let mode = playback_modes.get(guild_id).await;
    let stream_active = stream_sessions.lock().await.contains_key(&guild_id.get());

    let smtc = crate::smtc::run_smtc(crate::smtc::playback_snapshot)
        .await
        .ok()
        .map(|(status, np)| {
            format!(
                "smtc={status:?} title={:?}",
                np.and_then(|m| m.title)
            )
        })
        .unwrap_or_else(|| "smtc=unavailable".into());

    let Some(handler_lock) = songbird.get(guild_id) else {
        event(format!(
            "SNAPSHOT [{tag}] guild={guild_id} NO_SONGBIRD_CALL stream_active={stream_active} mode={mode:?} {smtc}"
        ));
        return;
    };

    let handler = handler_lock.lock().await;
    let bot_ch = handler.current_channel().map(|c| c.0.get());
    let driver_active = handler.is_voice_driver_active();
    let queue_len = handler.queue().current_queue().len();

    let mut track_lines = Vec::new();
    for (i, th) in handler.queue().current_queue().iter().enumerate() {
        match th.get_info().await {
            Ok(info) => track_lines.push(format!(
                "#{i}: playing={:?} ready={:?} done={}",
                info.playing,
                info.ready,
                info.playing.is_done()
            )),
            Err(e) => track_lines.push(format!("#{i}: info_err={e}")),
        }
    }

    event(format!(
        "SNAPSHOT [{tag}] guild={guild_id} bot_ch={bot_ch:?} driver_active={driver_active} \
         stream_active={stream_active} mode={mode:?} queue_len={queue_len} tracks=[{}] {smtc}",
        track_lines.join("; ")
    ));
}

pub fn log_track_event(guild_id: GuildId, event_name: &str, detail: &str) {
    event(format!("TRACK guild={guild_id} {event_name}: {detail}"));
}

pub fn log_voice_core(guild_id: GuildId, event_name: &str, detail: &str) {
    warn(format!("VOICE_CORE guild={guild_id} {event_name}: {detail}"));
}

pub fn log_stop(caller: &str, guild_id: GuildId, detail: &str) {
    warn(format!("STOP [{caller}] guild={guild_id}: {detail}"));
}

pub async fn log_idle_check(
    guild_id: GuildId,
    context: &str,
    would_idle: bool,
    mode: GuildPlayback,
    stream_active: bool,
    queue_empty: bool,
    in_channel: bool,
) {
    event(format!(
        "IDLE_CHECK [{context}] guild={guild_id} would_idle={would_idle} mode={mode:?} \
         stream_active={stream_active} queue_empty={queue_empty} in_channel={in_channel}"
    ));
}
