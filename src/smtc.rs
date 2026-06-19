/// Windows SMTC (System Media Transport Controls) integration for controlling
/// the local Spotify desktop client.
///
/// On non-Windows platforms all functions return an error.
#[cfg(windows)]
mod impl_smtc {
    use windows::Media::Control::{
        GlobalSystemMediaTransportControlsSession,
        GlobalSystemMediaTransportControlsSessionManager,
        GlobalSystemMediaTransportControlsSessionMediaProperties,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus,
    };

    /// Find the Spotify SMTC session (if any).
    pub(super) fn find_spotify_session() -> Result<GlobalSystemMediaTransportControlsSession, String> {
        let sessions = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
            .map_err(|e| format!("Failed to request SMTC sessions: {e}"))?;

        let session_info = sessions
            .get()
            .map_err(|e| format!("SMTC session request failed: {e}"))?;

        let all_sessions = session_info
            .GetSessions()
            .map_err(|e| format!("Failed to list SMTC sessions: {e}"))?;

        // Collect into a Vec to iterate
        let session_vec: Vec<GlobalSystemMediaTransportControlsSession> = all_sessions.into_iter().collect();

        for session in session_vec {
            let source_app_id = session
                .SourceAppUserModelId()
                .map_err(|e| format!("Failed to get source app ID: {e}"))?;

            // Check if this is Spotify by source app ID
            let source_app_str: String = source_app_id.to_string();
            let is_spotify = source_app_str.to_lowercase().contains("spotify");

            if is_spotify {
                return Ok(session);
            }
        }

        Err("No Spotify session found in SMTC. Is Spotify running?".into())
    }

    pub(super) fn do_pause() -> Result<(), String> {
        let session = find_spotify_session()?;
        session
            .TryPauseAsync()
            .map_err(|e| format!("SMTC pause failed: {e}"))?
            .get()
            .map_err(|e| format!("SMTC pause async failed: {e}"))?;
        Ok(())
    }

    pub(super) fn do_resume() -> Result<(), String> {
        let session = find_spotify_session()?;
        session
            .TryPlayAsync()
            .map_err(|e| format!("SMTC resume failed: {e}"))?
            .get()
            .map_err(|e| format!("SMTC resume async failed: {e}"))?;
        Ok(())
    }

    pub(super) fn do_skip_next() -> Result<(), String> {
        let session = find_spotify_session()?;
        session
            .TrySkipNextAsync()
            .map_err(|e| format!("SMTC skip next failed: {e}"))?
            .get()
            .map_err(|e| format!("SMTC skip next async failed: {e}"))?;
        Ok(())
    }

    pub(super) fn do_skip_previous() -> Result<(), String> {
        let session = find_spotify_session()?;
        session
            .TrySkipPreviousAsync()
            .map_err(|e| format!("SMTC skip previous failed: {e}"))?
            .get()
            .map_err(|e| format!("SMTC skip previous async failed: {e}"))?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(super) fn do_toggle() -> Result<(), String> {
        let session = find_spotify_session()?;
        session
            .TryTogglePlayPauseAsync()
            .map_err(|e| format!("SMTC toggle failed: {e}"))?
            .get()
            .map_err(|e| format!("SMTC toggle async failed: {e}"))?;
        Ok(())
    }

    pub(super) struct NowPlaying {
        pub title: Option<String>,
        pub artist: Option<String>,
        pub album: Option<String>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum PlaybackStatus {
        Closed,
        Opened,
        Changing,
        Stopped,
        Playing,
        Paused,
    }

    impl PlaybackStatus {
        fn from_raw(raw: GlobalSystemMediaTransportControlsSessionPlaybackStatus) -> Self {
            use GlobalSystemMediaTransportControlsSessionPlaybackStatus as S;
            match raw {
                S::Closed => Self::Closed,
                S::Opened => Self::Opened,
                S::Changing => Self::Changing,
                S::Stopped => Self::Stopped,
                S::Playing => Self::Playing,
                S::Paused => Self::Paused,
                _ => Self::Stopped,
            }
        }
    }

    pub(super) fn do_playback_snapshot(
    ) -> Result<(PlaybackStatus, Option<NowPlaying>), String> {
        let session = find_spotify_session()?;
        let info = session
            .GetPlaybackInfo()
            .map_err(|e| format!("SMTC playback info failed: {e}"))?;
        let status = PlaybackStatus::from_raw(
            info.PlaybackStatus()
                .map_err(|e| format!("SMTC playback status failed: {e}"))?,
        );
        Ok((status, do_now_playing()?))
    }

    pub(super) fn do_now_playing() -> Result<Option<NowPlaying>, String> {
        let session = find_spotify_session()?;
        let props: GlobalSystemMediaTransportControlsSessionMediaProperties = session
            .TryGetMediaPropertiesAsync()
            .map_err(|e| format!("SMTC get properties failed: {e}"))?
            .get()
            .map_err(|e| format!("SMTC get properties async failed: {e}"))?;

        let title = props.Title().ok().and_then(|v| {
            let s: String = v.try_into().ok()?;
            if s.is_empty() { None } else { Some(s) }
        });

        let artist = props.Artist().ok().and_then(|v| {
            let s: String = v.try_into().ok()?;
            if s.is_empty() { None } else { Some(s) }
        });

        let album = props.AlbumTitle().ok().and_then(|v| {
            let s: String = v.try_into().ok()?;
            if s.is_empty() { None } else { Some(s) }
        });

        Ok(Some(NowPlaying { title, artist, album }))
    }
}

#[cfg(not(windows))]
mod impl_smtc {
    pub(super) fn do_pause() -> Result<(), String> {
        Err("Spotify transport control requires Windows.".into())
    }
    pub(super) fn do_resume() -> Result<(), String> {
        Err("Spotify transport control requires Windows.".into())
    }
    pub(super) fn do_skip_next() -> Result<(), String> {
        Err("Spotify transport control requires Windows.".into())
    }
    pub(super) fn do_skip_previous() -> Result<(), String> {
        Err("Spotify transport control requires Windows.".into())
    }
    #[allow(dead_code)]
    pub(super) fn do_toggle() -> Result<(), String> {
        Err("Spotify transport control requires Windows.".into())
    }
    pub(super) struct NowPlaying {
        pub title: Option<String>,
        pub artist: Option<String>,
        pub album: Option<String>,
    }
    pub(super) fn do_playback_snapshot() -> Result<(PlaybackStatus, Option<NowPlaying>), String> {
        Err("Spotify transport control requires Windows.".into())
    }
    pub(super) enum PlaybackStatus {
        Stopped,
    }
    pub(super) fn do_now_playing() -> Result<Option<NowPlaying>, String> {
        Err("Spotify transport control requires Windows.".into())
    }
}

/// Pause Spotify playback via SMTC.
pub fn pause() -> Result<(), String> {
    impl_smtc::do_pause()
}

/// Resume Spotify playback via SMTC.
pub fn resume() -> Result<(), String> {
    impl_smtc::do_resume()
}

/// Skip to the next track via SMTC.
pub fn skip_next() -> Result<(), String> {
    impl_smtc::do_skip_next()
}

/// Skip to the previous track via SMTC.
pub fn skip_previous() -> Result<(), String> {
    impl_smtc::do_skip_previous()
}

/// Toggle play/pause via SMTC.
#[allow(dead_code)]
pub fn toggle() -> Result<(), String> {
    impl_smtc::do_toggle()
}

/// Metadata for the currently playing Spotify track.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NowPlaying {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackStatus {
    Playing,
    Paused,
    Stopped,
    Other,
}

fn map_playback_status(s: impl_smtc::PlaybackStatus) -> PlaybackStatus {
    match s {
        impl_smtc::PlaybackStatus::Playing => PlaybackStatus::Playing,
        impl_smtc::PlaybackStatus::Paused => PlaybackStatus::Paused,
        impl_smtc::PlaybackStatus::Stopped => PlaybackStatus::Stopped,
        _ => PlaybackStatus::Other,
    }
}

/// Returns whether SMTC metadata looks like a different track than before.
pub fn metadata_differs(before: &NowPlaying, now: &NowPlaying) -> bool {
    before.title != now.title || before.artist != now.artist
}

/// Get the current Spotify playback status and metadata.
pub fn playback_snapshot() -> Result<(PlaybackStatus, Option<NowPlaying>), String> {
    impl_smtc::do_playback_snapshot().map(|(status, np)| {
        (
            map_playback_status(status),
            np.map(|inner| NowPlaying {
                title: inner.title,
                artist: inner.artist,
                album: inner.album,
            }),
        )
    })
}

/// Poll SMTC until Spotify reports the desired playback status.
pub async fn wait_until_status(want: PlaybackStatus, timeout: std::time::Duration) -> Result<(), String> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let (status, _) = run_smtc(playback_snapshot).await?;
        if status == want {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    Err(format!(
        "Spotify did not reach {want:?} within {}s",
        timeout.as_secs()
    ))
}

/// Poll SMTC until Spotify is playing the opened content.
///
/// Accepts when metadata changes, or once the URI has had time to load and playback is active
/// (covers opening the same track again after a pause).
pub async fn wait_until_playing(
    before: &NowPlaying,
    opened_at: std::time::Instant,
    timeout: std::time::Duration,
) -> Result<NowPlaying, String> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let (status, np) = run_smtc(playback_snapshot).await?;
        if status == PlaybackStatus::Playing {
            if let Some(now) = np {
                if now.title.is_some()
                    && (metadata_differs(before, &now)
                        || opened_at.elapsed() >= std::time::Duration::from_millis(800))
                {
                    return Ok(now);
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    Err(format!(
        "Spotify did not start playing within {}s (still showing previous track?)",
        timeout.as_secs()
    ))
}

/// Get the currently playing track's metadata via SMTC.
pub fn now_playing() -> Result<Option<NowPlaying>, String> {
    impl_smtc::do_now_playing().map(|opt| {
        opt.map(|inner| NowPlaying {
            title: inner.title,
            artist: inner.artist,
            album: inner.album,
        })
    })
}

/// Run an SMTC operation on a blocking thread (COM apartments are single-threaded).
pub async fn run_smtc<F, R>(f: F) -> Result<R, String>
where
    F: FnOnce() -> Result<R, String> + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("SMTC task join error: {e}"))?
}
