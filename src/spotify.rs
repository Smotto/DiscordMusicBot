/// Parsed Spotify URI info (track, album, or playlist).
pub enum SpotifyKind {
    Track,
    Album,
    Playlist,
}

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

/// Pause current playback, open the requested track in the desktop app, then wait until SMTC reports playing.
pub async fn start_local_playback(spotify: &SpotifyUri) -> Result<crate::smtc::NowPlaying, String> {
    use std::time::{Duration, Instant};

    let before = crate::smtc::run_smtc(crate::smtc::now_playing)
        .await?
        .unwrap_or_default();

    // Pause so `start` does not instantly resume the previous track.
    crate::smtc::run_smtc(crate::smtc::pause).await.ok();
    crate::smtc::wait_until_status(crate::smtc::PlaybackStatus::Paused, Duration::from_secs(5))
        .await
        .ok();

    let uri = to_uri(spotify);
    tracing::info!("Opening Spotify URI: {uri}");
    open_spotify_uri(&uri)?;

    let opened_at = Instant::now();
    crate::smtc::wait_until_playing(&before, opened_at, Duration::from_secs(30)).await
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
