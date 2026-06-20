use serenity::all::{ChannelId, MessageId};
use serenity::builder::{CreateMessage, EditMessage};
use std::sync::Arc;
use tokio::sync::Mutex;

/// One pinned-style status message per Spotify session (edited in place).
pub struct SpotifyStatusBoard {
    target: Mutex<Option<(ChannelId, MessageId)>>,
}

impl SpotifyStatusBoard {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            target: Mutex::new(None),
        })
    }

    pub async fn set(&self, channel_id: ChannelId, message_id: MessageId) {
        *self.target.lock().await = Some((channel_id, message_id));
    }

    pub async fn channel_id(&self) -> Option<ChannelId> {
        self.target.lock().await.map(|(channel_id, _)| channel_id)
    }

    pub async fn clear(&self) {
        *self.target.lock().await = None;
    }

    /// Update the live board, or post a new one if missing or deleted.
    pub async fn refresh(
        &self,
        http: &serenity::http::Http,
        fallback_channel: ChannelId,
        embed: serenity::builder::CreateEmbed,
    ) {
        let mut slot = self.target.lock().await;
        if let Some((channel_id, message_id)) = *slot {
            if channel_id
                .edit_message(http, message_id, EditMessage::new().embed(embed.clone()))
                .await
                .is_ok()
            {
                return;
            }
        }

        match fallback_channel
            .send_message(http, CreateMessage::new().embed(embed))
            .await
        {
            Ok(msg) => *slot = Some((fallback_channel, msg.id)),
            Err(e) => tracing::warn!("Failed to post Spotify status board: {e}"),
        }
    }

    pub async fn delete_message(&self, http: &serenity::http::Http) {
        let id = self.target.lock().await.take();
        if let Some((channel_id, message_id)) = id {
            let _ = channel_id.delete_message(http, message_id).await;
        }
    }
}
