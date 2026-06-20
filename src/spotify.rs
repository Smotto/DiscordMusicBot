use crate::playback::{IntentionalTrack, QueuedSpotify, SpotifyQueues, SpotifySessionTracker, SpotifyTrackInfo};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Parsed Spotify URI info (track, album, or playlist).
#[derive(Clone, Debug, PartialEq, Eq)]
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

/// Resolve title (and artist for tracks) for queue labels and matching.
pub async fn fetch_link_metadata(
    client: &reqwest::Client,
    spotify: &SpotifyUri,
    input: &str,
) -> SpotifyTrackInfo {
    let title = fetch_oembed_title(client, spotify, input).await;
    let artist = match spotify.kind {
        SpotifyKind::Track => fetch_track_page_artist(client, &spotify.id).await,
        _ => None,
    };
    SpotifyTrackInfo {
        uri: spotify.clone(),
        title,
        artist: artist.map(|a| sanitize_artist(&a)).filter(|a| !a.is_empty()),
    }
}

async fn fetch_oembed_title(
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

/// Best-effort artist from the public track page (no API key).
async fn fetch_track_page_artist(client: &reqwest::Client, track_id: &str) -> Option<String> {
    let url = format!("https://open.spotify.com/track/{track_id}");
    let body = client
        .get(&url)
        .header("User-Agent", "Mozilla/5.0 (compatible; DiscordMusicBot/1.0)")
        .header("Accept-Language", "en")
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .text()
        .await
        .ok()?;
    parse_track_page_artist(&body)
}

/// Strip Spotify locale junk from artist strings (`en`, `en王翊恩`, etc.).
pub fn sanitize_artist(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    // SMTC sometimes glues a 2-letter locale prefix onto the name: "en王翊恩".
    if trimmed.len() > 2 {
        let mut chars = trimmed.chars();
        let a = chars.next().unwrap();
        let b = chars.next().unwrap();
        if a.is_ascii_alphabetic() && b.is_ascii_alphabetic() {
            let rest: String = chars.collect();
            if !rest.is_empty() && rest.chars().next().is_some_and(|c| !c.is_ascii_alphabetic()) {
                return rest.trim().to_string();
            }
        }
    }

    trimmed.to_string()
}

fn is_og_noise_segment(s: &str) -> bool {
    let lower = s.to_lowercase();
    if lower.is_empty() {
        return true;
    }
    // Locale tags: en, zh-hans, pt-br, …
    if lower.len() <= 8
        && lower
            .chars()
            .all(|c| c.is_ascii_alphabetic() || c == '-')
    {
        return true;
    }
    if s.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    lower.contains("song")
}

fn parse_track_page_artist(html: &str) -> Option<String> {
    // Prefer embedded JSON — more reliable than og:description segments.
    if let Some(idx) = html.find(r#""artists":[{"uri":"spotify:artist:"#) {
        let slice = &html[idx..idx.saturating_add(1200)];
        if let Some(name) = parse_json_string_field(slice, "name") {
            let clean = sanitize_artist(&name);
            if !clean.is_empty() && !is_og_noise_segment(&clean) {
                return Some(clean);
            }
        }
    }

    // og:description: "Listen to Song on Spotify. en · Artist · Album · …"
    if let Some(content) = parse_meta_content(html, "og:description") {
        if let Some(rest) = content.split(" on Spotify.").nth(1) {
            for segment in rest.split('·') {
                let artist = sanitize_artist(segment);
                if !artist.is_empty() && !is_og_noise_segment(&artist) {
                    return Some(artist);
                }
            }
        }
    }

    None
}

fn parse_meta_content(html: &str, property: &str) -> Option<String> {
    let needle = format!("property=\"{property}\" content=\"");
    let start = html.find(&needle)? + needle.len();
    let mut out = String::new();
    let mut escape = false;
    for c in html[start..].chars() {
        if escape {
            out.push(c);
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

/// Build canonical track info from live SMTC metadata after playback starts.
pub fn track_info_from_smtc(np: &crate::smtc::NowPlaying, uri: &SpotifyUri) -> SpotifyTrackInfo {
    SpotifyTrackInfo {
        uri: uri.clone(),
        title: np
            .title
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| to_uri(uri)),
        artist: np
            .artist
            .as_deref()
            .map(sanitize_artist)
            .filter(|a| !a.is_empty()),
    }
}

/// Format SMTC metadata for Discord messages.
pub fn format_now_playing(np: &crate::smtc::NowPlaying) -> String {
    np.title
        .as_deref()
        .map(|t| {
            np.artist
                .as_deref()
                .map(sanitize_artist)
                .filter(|a| !a.is_empty())
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
            out.push_str(&format!("{}. {}\n", i + 1, item.display()));
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

/// Open a queued track and confirm SMTC shows that song before returning.
async fn advance_local_playback_verified(
    expected: &SpotifyTrackInfo,
) -> Result<crate::smtc::NowPlaying, String> {
    use std::time::Duration;

    let before = crate::smtc::run_smtc(crate::smtc::now_playing)
        .await?
        .unwrap_or_default();

    // Pause so Spotify autoplay does not win the race when we open the queued URI.
    crate::smtc::run_smtc(crate::smtc::pause).await.ok();
    crate::smtc::wait_until_status(crate::smtc::PlaybackStatus::Paused, Duration::from_secs(5))
        .await
        .ok();

    let uri = to_uri(&expected.uri);
    tracing::info!(
        "Opening queued Spotify URI: {uri} (expect \"{}\")",
        expected.display()
    );
    open_spotify_uri(&uri)?;

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let (status, np) = crate::smtc::run_smtc(crate::smtc::playback_snapshot).await?;
        if status == crate::smtc::PlaybackStatus::Playing {
            if let Some(now) = np {
                if now.title.is_some()
                    && metadata_matches_track(&now, expected)
                    && crate::smtc::metadata_differs(&before, &now)
                {
                    return Ok(now);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    Err(format!(
        "Spotify did not start \"{}\" within 30s (autoplay may have taken over).",
        expected.display()
    ))
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
    track_info_from_smtc(np, fallback).display()
}

fn normalize_field(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn fields_equal(a: &str, b: &str) -> bool {
    normalize_field(a) == normalize_field(b)
}

/// True when SMTC metadata matches the expected track (strict title; artist when known).
pub fn metadata_matches_track(np: &crate::smtc::NowPlaying, expected: &SpotifyTrackInfo) -> bool {
    let Some(actual_title) = np.title.as_deref().filter(|t| !t.is_empty()) else {
        return false;
    };
    if !fields_equal(actual_title, &expected.title) {
        return false;
    }
    match (
        expected.artist.as_deref().filter(|a| !a.is_empty()),
        np.artist.as_deref().filter(|a| !a.is_empty()),
    ) {
        (Some(exp), Some(act)) => fields_equal(exp, act),
        _ => true,
    }
}

/// Debounce SMTC metadata watches after the bot changes tracks programmatically.
pub struct SpotifyPlaybackGuard {
    last_programmatic: Mutex<Option<Instant>>,
}

impl SpotifyPlaybackGuard {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            last_programmatic: Mutex::new(None),
        })
    }

    pub async fn note(&self) {
        *self.last_programmatic.lock().await = Some(Instant::now());
    }

    async fn recently_programmatic(&self) -> bool {
        self.last_programmatic
            .lock()
            .await
            .is_some_and(|t| t.elapsed() < Duration::from_secs(3))
    }
}

/// Poll faster when a queue is active; preempt before Spotify autoplay wins.
const QUEUE_WATCH_ACTIVE: Duration = Duration::from_millis(200);
const QUEUE_WATCH_IDLE: Duration = Duration::from_secs(2);
const QUEUE_PREEMPT_BEFORE_END: Duration = Duration::from_millis(2500);
const MIN_PLAYED_BEFORE_PREEMPT: Duration = Duration::from_secs(3);

/// True when SMTC title matches what the bot thinks is playing (ignore artist drift).
pub fn smtc_title_matches_intentional(np: &crate::smtc::NowPlaying, intentional: &IntentionalTrack) -> bool {
    np.title
        .as_deref()
        .filter(|t| !t.is_empty())
        .is_some_and(|t| fields_equal(t, &intentional.title))
}

async fn try_advance_queue(
    queues: &SpotifyQueues,
    guard: &SpotifyPlaybackGuard,
    session: &SpotifySessionTracker,
    reason: &str,
) -> bool {
    let Some(next) = queues.peek_front().await else {
        return false;
    };

    let live = crate::smtc::run_smtc(crate::smtc::now_playing)
        .await
        .ok()
        .flatten();

    // Spotify already on this queued track — adopt and pop without reopening.
    if live
        .as_ref()
        .is_some_and(|np| metadata_matches_track(np, &next))
    {
        guard.note().await;
        queues.pop_front().await;
        if let Some(np) = live {
            session
                .set_intentional(track_info_from_smtc(&np, &next.uri))
                .await;
        }
        tracing::info!(
            "Spotify queue advance ({reason}): already on \"{}\" — adopted",
            next.display()
        );
        return true;
    }

    // Bot thinks this URI is current — drop duplicate queue entry.
    if session
        .get_intentional()
        .await
        .is_some_and(|current| current.same_uri(&next.uri))
    {
        guard.note().await;
        queues.pop_front().await;
        tracing::info!(
            "Spotify queue advance ({reason}): duplicate \"{}\" — skipped reopen",
            next.display()
        );
        return true;
    }

    match advance_local_playback_verified(&next).await {
        Ok(np) => {
            guard.note().await;
            queues.pop_front().await;
            session
                .set_intentional(track_info_from_smtc(&np, &next.uri))
                .await;
            tracing::info!("Spotify queue advance ({reason}): {}", next.display());
            true
        }
        Err(e) => {
            tracing::warn!(
                "Spotify queue advance failed ({reason}) for \"{}\": {e}",
                next.display()
            );
            false
        }
    }
}

/// Skip via SMTC, or play the next queued URI if the user built a queue.
pub async fn skip_or_play_next(
    queues: &SpotifyQueues,
    guard: &SpotifyPlaybackGuard,
    session: &SpotifySessionTracker,
) -> Result<(), String> {
    if queues.peek_front().await.is_some() {
        if try_advance_queue(queues, guard, session, "skip").await {
            Ok(())
        } else {
            Err("Failed to start the next queued track.".into())
        }
    } else {
        crate::smtc::run_smtc(crate::smtc::skip_next).await
    }
}

/// When a track ends and the queue has entries, open the next link in Spotify.
pub fn spawn_queue_watcher(
    queues: Arc<SpotifyQueues>,
    guard: Arc<SpotifyPlaybackGuard>,
    session: Arc<SpotifySessionTracker>,
    stream_sessions: Arc<Mutex<HashMap<u64, crate::stream::StreamSession>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let interval = if queues.is_empty().await {
                QUEUE_WATCH_IDLE
            } else {
                QUEUE_WATCH_ACTIVE
            };
            tokio::time::sleep(interval).await;

            if stream_sessions.lock().await.is_empty() {
                break;
            }

            if guard.recently_programmatic().await {
                continue;
            }

            let intentional = session.get_intentional().await;
            let now = crate::smtc::run_smtc(crate::smtc::now_playing)
                .await
                .ok()
                .flatten();

            // Refresh artist metadata when title still matches (oEmbed vs SMTC drift).
            if let (Some(np), Some(intent)) = (&now, &intentional) {
                if smtc_title_matches_intentional(np, intent) && !metadata_matches_track(np, intent)
                {
                    session
                        .set_intentional(track_info_from_smtc(np, &intent.uri))
                        .await;
                }
            }

            if queues.is_empty().await {
                continue;
            }

            // Preempt before track end so Spotify autoplay does not start.
            if let Ok((crate::smtc::PlaybackStatus::Playing, _)) =
                crate::smtc::run_smtc(crate::smtc::playback_snapshot).await
            {
                if let Ok(Some(timeline)) =
                    crate::smtc::run_smtc(crate::smtc::playback_timeline).await
                {
                    if timeline.position >= MIN_PLAYED_BEFORE_PREEMPT
                        && timeline.remaining() <= QUEUE_PREEMPT_BEFORE_END
                    {
                        try_advance_queue(&queues, &guard, &session, "preempt").await;
                        continue;
                    }
                }
            }

            // Spotify moved to a different *title* — take back control from the queue.
            match (&now, &intentional) {
                (Some(np), Some(intent)) if !smtc_title_matches_intentional(np, intent) => {
                    try_advance_queue(&queues, &guard, &session, "autoplay-override").await;
                }
                (Some(_), None) => {
                    try_advance_queue(&queues, &guard, &session, "no-intentional").await;
                }
                _ => {}
            }
        }
    })
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
