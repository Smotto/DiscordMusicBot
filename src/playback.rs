use serenity::all::GuildId;
use std::collections::HashMap;
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
        *self.inner.lock().await.get(&guild_id.get()).unwrap_or(&GuildPlayback::Idle)
    }

    pub async fn set(&self, guild_id: GuildId, mode: GuildPlayback) {
        self.inner.lock().await.insert(guild_id.get(), mode);
    }
}
