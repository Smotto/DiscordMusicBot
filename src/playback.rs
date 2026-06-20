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

/// A Spotify link waiting to play after the current track.
#[derive(Clone, Debug)]
pub struct QueuedSpotify {
    pub uri: SpotifyUri,
    pub label: String,
}

/// Per-guild Spotify URI queue (separate from Songbird's YouTube track queue).
pub struct SpotifyQueues {
    inner: Mutex<HashMap<u64, VecDeque<QueuedSpotify>>>,
}

impl SpotifyQueues {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(HashMap::new()),
        })
    }

    /// Append a track; returns its 1-based queue position.
    pub async fn push(&self, guild_id: GuildId, uri: SpotifyUri, label: String) -> usize {
        let mut map = self.inner.lock().await;
        let queue = map.entry(guild_id.get()).or_default();
        queue.push_back(QueuedSpotify { uri, label });
        queue.len()
    }

    pub async fn pop_front(&self, guild_id: GuildId) -> Option<QueuedSpotify> {
        let mut map = self.inner.lock().await;
        map.get_mut(&guild_id.get())?.pop_front()
    }

    pub async fn len(&self, guild_id: GuildId) -> usize {
        self.inner
            .lock()
            .await
            .get(&guild_id.get())
            .map(|q| q.len())
            .unwrap_or(0)
    }

    pub async fn is_empty(&self, guild_id: GuildId) -> bool {
        self.len(guild_id).await == 0
    }

    pub async fn clear(&self, guild_id: GuildId) {
        self.inner.lock().await.remove(&guild_id.get());
    }

    pub async fn list(&self, guild_id: GuildId) -> Vec<QueuedSpotify> {
        self.inner
            .lock()
            .await
            .get(&guild_id.get())
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }
}
