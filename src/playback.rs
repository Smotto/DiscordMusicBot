use crate::spotify::SpotifyUri;
use serenity::all::GuildId;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::Mutex;

/// The current playback mode for a guild.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuildPlayback {
    /// Nothing is playing.
    Idle,
    /// YouTube playback active.
    Youtube,
    /// Spotify cable stream active.
    Spotify,
}

/// Per-guild playback mode map.
pub struct PlaybackModes {
    inner: Mutex<HashMap<u64, GuildPlayback>>,
}

impl PlaybackModes {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(HashMap::new()),
        })
    }

    pub async fn get(&self, guild_id: GuildId) -> GuildPlayback {
        *self
            .inner
            .lock()
            .await
            .get(&guild_id.get())
            .unwrap_or(&GuildPlayback::Idle)
    }

    pub async fn set(&self, guild_id: GuildId, mode: GuildPlayback) {
        self.inner.lock().await.insert(guild_id.get(), mode);
    }
}

/// Canonical Spotify item identity for queue display and matching.
#[derive(Clone, Debug)]
pub struct SpotifyTrackInfo {
    pub uri: SpotifyUri,
    pub title: String,
    pub artist: Option<String>,
    /// Discord display name of whoever queued or `/spotify play`'d this track.
    pub requested_by: Option<String>,
}

impl SpotifyTrackInfo {
    /// Discord-facing label — always `Title — Artist` when artist is known.
    pub fn display(&self) -> String {
        match self.artist.as_deref().filter(|a| !a.is_empty()) {
            Some(artist) => format!("{} — {}", self.title, artist),
            None => self.title.clone(),
        }
    }

    /// Numbered queue row with optional requester attribution.
    pub fn queue_line(&self, position: usize) -> String {
        let track = self.display();
        match self.requested_by.as_deref() {
            Some(user) => format!("`{position}.` {track} · **{user}**"),
            None => format!("`{position}.` {track}"),
        }
    }

    /// Now-playing embed line when the bot deliberately started this track.
    pub fn format_controlled_now_playing(&self) -> String {
        format_now_playing_attribution(&self.display(), self.requested_by.as_deref(), false)
    }

    pub fn same_uri(&self, other: &SpotifyUri) -> bool {
        self.uri.kind == other.kind && self.uri.id == other.id
    }
}

/// Embed field body for the active track (bot-requested vs Spotify autoplay).
pub fn format_now_playing_attribution(
    track: &str,
    requested_by: Option<&str>,
    autoplay: bool,
) -> String {
    if autoplay {
        format!("**{track}**\n*Spotify autoplay — not from bot queue*")
    } else if let Some(user) = requested_by {
        format!("**{track}**\nRequested by **{user}**")
    } else {
        format!("**{track}**")
    }
}

/// A Spotify link waiting to play after the current track.
pub type QueuedSpotify = SpotifyTrackInfo;

/// Shared Spotify queue — one desktop Spotify instance, one queue for the whole bot.
pub struct SpotifyQueues {
    inner: Mutex<VecDeque<QueuedSpotify>>,
}

impl SpotifyQueues {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(VecDeque::new()),
        })
    }

    /// Append a track; returns its 1-based queue position.
    pub async fn push(&self, item: SpotifyTrackInfo) -> usize {
        let mut queue = self.inner.lock().await;
        queue.push_back(item);
        queue.len()
    }

    pub async fn contains_uri(&self, uri: &SpotifyUri) -> bool {
        self.inner
            .lock()
            .await
            .iter()
            .any(|item| item.same_uri(uri))
    }

    pub async fn pop_front(&self) -> Option<QueuedSpotify> {
        self.inner.lock().await.pop_front()
    }

    pub async fn peek_front(&self) -> Option<QueuedSpotify> {
        self.inner.lock().await.front().cloned()
    }

    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    pub async fn clear(&self) {
        self.inner.lock().await.clear();
    }

    pub async fn list(&self) -> Vec<QueuedSpotify> {
        self.inner.lock().await.iter().cloned().collect()
    }
}

/// Track the song the bot deliberately started (vs Spotify autoplay drift).
pub type IntentionalTrack = SpotifyTrackInfo;

/// What Spotify is supposed to be playing right now (global — one SMTC session).
pub struct SpotifySessionTracker {
    intentional: Mutex<Option<IntentionalTrack>>,
}

impl SpotifySessionTracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            intentional: Mutex::new(None),
        })
    }

    pub async fn set_intentional(&self, track: IntentionalTrack) {
        *self.intentional.lock().await = Some(track);
    }

    pub async fn get_intentional(&self) -> Option<IntentionalTrack> {
        self.intentional.lock().await.clone()
    }

    pub async fn clear(&self) {
        *self.intentional.lock().await = None;
    }
}
