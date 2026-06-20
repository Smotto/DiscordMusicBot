use crate::playback::{QueuedSpotify, SpotifyQueues};
use serenity::all::GuildId;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Parsed Spotify URI info (track, album, or playlist).
#[derive(Clone, Debug)]
pub enum SpotifyKind {
    Track,
    Album,
    Playlist,
}

#[derive(Clone, Debug)]
pub struct SpotifyUri {
    pub kind: SpotifyKind,
    pub id: String,
}

/// Parse a Spotify URL or `spotify:` URI into a structured form.
///
/// Accepts:
/// - `https://open.spotify.com/track/{id}?...`
/// - `https://open.spotify.com/album/{id}?...`
/// - `https://open.spotify.com/playlist/{id}?...`
/// - `spotify:track:{id}`
/// - `spotify:album:{id}`
/// - `spotify:playlist:{id}`
pub fn parse_spotify_input(input: &str) -> Result<SpotifyUri, String> {
    let input = input.trim();

    // Handle spotify: URI scheme
    if let Some(rest) = input.strip_prefix("spotify:") {
        let parts: Vec<&str> = rest.split(':').collect();
        if parts.len() == 2 {
            let (kind, id) = (parts[0], parts[1]);
            let kind = match kind {
                "track" => SpotifyKind::Track,
                "album" => SpotifyKind::Album,
                "playlist" => SpotifyKind::Playlist,
                _ => return Err(format!("Unsupported Spotify type: {kind}")),
            };
            if id.is_empty() {
                return Err("Spotify ID is empty.".into());
            }
            return Ok(SpotifyUri {
                kind,
                id: id.to_string(),
            });
        }
    }

    // Handle open.spotify.com URLs
    if let Some(rest) = input.strip_prefix("https://open.spotify.com/") {
        if let Some(question) = rest.find('?') {
            // Strip query params
            let path = &rest[..question];
            parse_spotify_path(path)
        } else {
            parse_spotify_path(rest)
        }
    } else if input.starts_with("http://open.spotify.com/") {
        // Also handle http (non-https)
        let rest = input.strip_prefix("http://open.spotify.com/").unwrap();
        if let Some(question) = rest.find('?') {
            parse_spotify_path(&rest[..question])
        } else {
            parse_spotify_path(rest)
        }
    } else if input.starts_with("http") {
        Err("Not a Spotify URL. Provide a Spotify track, album, or playlist link.".into())
    } else {
        Err("Not a Spotify URL or URI. Provide a Spotify track, album, or playlist link.".into())
    }
}

fn parse_spotify_path(path: &str) -> Result<SpotifyUri, String> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() < 2 {
        return Err("Spotify URL has no type or ID.".into());
    }

    let (kind, id) = (parts[0], parts[1]);
    let kind = match kind {
        "track" => SpotifyKind::Track,
        "album" => SpotifyKind::Album,
        "playlist" => SpotifyKind::Playlist,
        other => return Err(format!("Unsupported Spotify type: {other}")),
    };

    if id.is_empty() {
        return Err("Spotify ID is empty.".into());
    }

    Ok(SpotifyUri {
        kind,
        id: id.to_string(),
    })
}

/// Convert a parsed SpotifyUri back to a `spotify:kind:id` string.
pub fn to_uri(spotify: &SpotifyUri) -> String {
    let kind_str = match spotify.kind {
        SpotifyKind::Track => "track",
        SpotifyKind::Album => "album",
        SpotifyKind::Playlist => "playlist",
    };
    format!("spotify:{}:{}", kind_str, spotify.id)
}

/// `https://open.spotify.com/...` URL for oEmbed and sharing.
pub fn to_open_url(spotify: &SpotifyUri) -> String {
    let kind_str = match spotify.kind {
        SpotifyKind::Track => "track",
        SpotifyKind::Album => "album",
        SpotifyKind::Playlist => "playlist",
    };
    format!("https://open.spotify.com/{kind_str}/{}", spotify.id)
}

fn oembed_page_url(input: &str, spotify: &SpotifyUri) -> String {
    if input.starts_with("http://") || input.starts_with("https://") {
        input
            .split('?')
            .next()
            .unwrap_or(input)
            .to_string()
    } else {
        to_open_url(spotify)
    }
}

fn parse_json_string_field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = json.find(&needle)? + needle.len();
    let mut out = String::new();
    let mut escape = false;
    for c in json[start..].chars() {
        if escape {
            match c {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                'n' => out.push('\n'),
                't' => out.push('\t'),
                other => out.push(other),
            }
            escape = false;
        } else if c == '\\' {
            escape = true;
        } else if c == '"' {
            break;
        } else {
            out.push(c);
        }
    }
    Some(out).filter(|s| !s.is_empty())
}

/// Resolve a human-readable title via Spotify's public oEmbed endpoint (no API key).
pub async fn fetch_link_title(
    client: &reqwest::Client,
    spotify: &SpotifyUri,
    input: &str,
) -> String {
    let page_url = oembed_page_url(input, spotify);
    match client
        .get("https://open.spotify.com/oembed")
        .query(&[("url", page_url.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(body) = resp.text().await {
                if let Some(title) = parse_json_string_field(&body, "title") {
                    return title;
                }
            }
        }
        Ok(resp) => {
            tracing::warn!("Spotify oEmbed HTTP {}", resp.status());
        }
        Err(e) => {
            tracing::warn!("Spotify oEmbed request failed: {e}");
        }
    }
    to_uri(spotify)
}

/// Format SMTC metadata for Discord messages.
pub fn format_now_playing(np: &crate::smtc::NowPlaying) -> String {
    np.title
        .as_deref()
        .map(|t| {
            np.artist
                .as_deref()
                .map(|a| format!("{t} — {a}"))
                .unwrap_or_else(|| t.to_string())
        })
        .unwrap_or_else(|| "Unknown track".into())
}

/// Build the `/spotify queue` response body.
pub fn format_queue_message(now_playing: Option<&str>, queue: &[QueuedSpotify]) -> String {
    let mut out = String::new();
    if let Some(np) = now_playing {
        out.push_str("**Now playing**\n");
        out.push_str(np);
        out.push_str("\n\n");
    }
    out.push_str("**Queue**\n");
    if queue.is_empty() {
        out.push_str("_Empty_");
    } else {
        for (i, item) in queue.iter().enumerate() {
            out.push_str(&format!("{}. {}\n", i + 1, item.label));
        }
    }
    out.trim_end().to_string()
}

/// Pause current playback, open the requested track in the desktop app, then wait until SMTC reports playing.
pub async fn start_local_playback(spotify: &SpotifyUri) -> Result<crate::smtc::NowPlaying, String> {
    use std::time::Duration;

    let before = crate::smtc::run_smtc(crate::smtc::now_playing)
        .await?
        .unwrap_or_default();

    // Pause so `start` does not instantly resume the previous track.
    crate::smtc::run_smtc(crate::smtc::pause).await.ok();
    crate::smtc::wait_until_status(crate::smtc::PlaybackStatus::Paused, Duration::from_secs(5))
        .await
        .ok();

    open_and_wait(spotify, &before).await
}

/// Open a queued track without pausing first — faster, avoids a blip of Spotify autoplay.
async fn advance_local_playback(spotify: &SpotifyUri) -> Result<crate::smtc::NowPlaying, String> {
    let before = crate::smtc::run_smtc(crate::smtc::now_playing)
        .await?
        .unwrap_or_default();
    open_and_wait(spotify, &before).await
}

async fn open_and_wait(
    spotify: &SpotifyUri,
    before: &crate::smtc::NowPlaying,
) -> Result<crate::smtc::NowPlaying, String> {
    use std::time::Duration;

    let uri = to_uri(spotify);
    tracing::info!("Opening Spotify URI: {uri}");
    open_spotify_uri(&uri)?;

    let opened_at = Instant::now();
    crate::smtc::wait_until_playing(before, opened_at, Duration::from_secs(30)).await
}

/// Human-readable label for Discord responses.
pub fn now_playing_label(np: &crate::smtc::NowPlaying, fallback: &SpotifyUri) -> String {
    if np.title.is_some() {
        format_now_playing(np)
    } else {
        to_uri(fallback)
    }
}

/// Debounce SMTC metadata watches after the bot changes tracks programmatically.
pub struct SpotifyPlaybackGuard {
    last_programmatic: Mutex<HashMap<u64, Instant>>,
}

impl SpotifyPlaybackGuard {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            last_programmatic: Mutex::new(HashMap::new()),
        })
    }

    pub async fn note(&self, guild_id: GuildId) {
        self.last_programmatic
            .lock()
            .await
            .insert(guild_id.get(), Instant::now());
    }

    async fn recently_programmatic(&self, guild_id: GuildId) -> bool {
        self.last_programmatic
            .lock()
            .await
            .get(&guild_id.get())
            .is_some_and(|t| t.elapsed() < Duration::from_secs(3))
    }
}

/// Poll faster when a queue is active; preempt ~1.2s before track end.
const QUEUE_WATCH_ACTIVE: Duration = Duration::from_millis(300);
const QUEUE_WATCH_IDLE: Duration = Duration::from_secs(2);
const QUEUE_PREEMPT_BEFORE_END: Duration = Duration::from_millis(1200);
const MIN_PLAYED_BEFORE_PREEMPT: Duration = Duration::from_secs(5);

pub async fn play_queued(
    guild_id: GuildId,
    item: &QueuedSpotify,
    guard: &SpotifyPlaybackGuard,
) -> Result<crate::smtc::NowPlaying, String> {
    guard.note(guild_id).await;
    advance_local_playback(&item.uri).await
}

/// Skip via SMTC, or play the next queued URI if the user built a queue.
pub async fn skip_or_play_next(
    guild_id: GuildId,
    queues: &SpotifyQueues,
    guard: &SpotifyPlaybackGuard,
) -> Result<(), String> {
    if let Some(next) = queues.pop_front(guild_id).await {
        tracing::info!("Spotify skip: playing queued {}", next.label);
        play_queued(guild_id, &next, guard).await?;
        Ok(())
    } else {
        crate::smtc::run_smtc(crate::smtc::skip_next).await
    }
}

/// When a track ends and the queue has entries, open the next link in Spotify.
pub fn spawn_queue_watcher(
    guild_id: GuildId,
    queues: Arc<SpotifyQueues>,
    guard: Arc<SpotifyPlaybackGuard>,
    stream_sessions: Arc<Mutex<HashMap<u64, crate::stream::StreamSession>>>,
) {
    tokio::spawn(async move {
        let mut baseline = crate::smtc::run_smtc(crate::smtc::now_playing)
            .await
            .ok()
            .flatten();

        loop {
            let interval = if queues.is_empty(guild_id).await {
                QUEUE_WATCH_IDLE
            } else {
                QUEUE_WATCH_ACTIVE
            };
            tokio::time::sleep(interval).await;

            if !stream_sessions.lock().await.contains_key(&guild_id.get()) {
                break;
            }

            if queues.is_empty(guild_id).await {
                baseline = crate::smtc::run_smtc(crate::smtc::now_playing)
                    .await
                    .ok()
                    .flatten();
                continue;
            }

            if guard.recently_programmatic(guild_id).await {
                continue;
            }

            // Preempt before Spotify autoplay advances to the next album/playlist track.
            if let Ok((crate::smtc::PlaybackStatus::Playing, _)) =
                crate::smtc::run_smtc(crate::smtc::playback_snapshot).await
            {
                if let Ok(Some(timeline)) =
                    crate::smtc::run_smtc(crate::smtc::playback_timeline).await
                {
                    if timeline.position >= MIN_PLAYED_BEFORE_PREEMPT
                        && timeline.remaining() <= QUEUE_PREEMPT_BEFORE_END
                    {
                        if let Some(next) = queues.pop_front(guild_id).await {
                            tracing::info!(
                                "Spotify queue preempt ({}ms left): {}",
                                timeline.remaining().as_millis(),
                                next.label
                            );
                            guard.note(guild_id).await;
                            if advance_local_playback(&next.uri).await.is_ok() {
                                baseline = crate::smtc::run_smtc(crate::smtc::now_playing)
                                    .await
                                    .ok()
                                    .flatten();
                                continue;
                            }
                        }
                    }
                }
            }

            // Fallback if preempt missed (e.g. no timeline from SMTC).
            let now = match crate::smtc::run_smtc(crate::smtc::now_playing).await {
                Ok(v) => v,
                Err(_) => continue,
            };

            let changed = match (&baseline, &now) {
                (Some(b), Some(n)) => crate::smtc::metadata_differs(b, n),
                (None, Some(n)) => n.title.is_some(),
                _ => false,
            };

            if changed {
                if let Some(next) = queues.pop_front(guild_id).await {
                    tracing::info!("Spotify queue auto-advance (metadata): {}", next.label);
                    guard.note(guild_id).await;
                    if advance_local_playback(&next.uri).await.is_ok() {
                        baseline = crate::smtc::run_smtc(crate::smtc::now_playing)
                            .await
                            .ok()
                            .flatten();
                        continue;
                    }
                }
            }

            baseline = now.or(baseline);
        }
    });
}

/// Open a Spotify URI on the local machine using the system's default handler.
///
/// On Windows this uses `cmd /C start`. On other platforms it returns an error.
pub fn open_spotify_uri(uri: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", uri])
            .spawn()
            .map_err(|e| format!("Failed to open Spotify URI: {e}"))?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = uri;
        Err("Spotify control requires Windows.".into())
    }
}

/// Open a Spotify search query via the `spotify:search:` URI scheme.
#[allow(dead_code)]
pub fn open_spotify_search(query: &str) -> Result<(), String> {
    let encoded = urlencode(query);
    open_spotify_uri(&format!("spotify:search:{encoded}"))
}

#[allow(dead_code)]
fn urlencode(s: &str) -> String {
    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}
