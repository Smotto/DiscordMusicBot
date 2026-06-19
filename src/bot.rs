use crate::music;
use serenity::all::{CommandInteraction, CommandOptionType, Context, Interaction, UserId};
use serenity::builder::{
    Builder, CreateCommand, CreateCommandOption, CreateInteractionResponse,
    CreateInteractionResponseMessage,
};
use serenity::prelude::*;
use songbird::{Config as SongbirdConfig, SerenityInit, Songbird};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

pub struct Handler {
    pub guild_id: Option<u64>,
    pub http_client: reqwest::Client,
    pub songbird: Arc<Songbird>,
    pub track_events_registered: Arc<Mutex<HashSet<u64>>>,
}

fn ack(msg: impl Into<String>) -> CreateInteractionResponse {
    CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().content(msg))
}

pub async fn run_bot() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let token = std::env::var("DISCORD_TOKEN").expect("Missing DISCORD_TOKEN in .env");
    let guild_id = std::env::var("GUILD_ID")
        .ok()
        .and_then(|s| s.parse().ok());

    let intents = GatewayIntents::non_privileged() | GatewayIntents::GUILD_MEMBERS;

    let songbird = Songbird::serenity_from_config(
        SongbirdConfig::default().gateway_timeout(Some(Duration::from_secs(30))),
    );
    let handler = Handler {
        guild_id,
        http_client: reqwest::Client::new(),
        songbird: songbird.clone(),
        track_events_registered: Arc::new(Mutex::new(HashSet::new())),
    };

    let mut client = Client::builder(token, intents)
        .register_songbird_with(songbird)
        .event_handler(handler)
        .await?;

    tracing::info!("Music bot is starting...");
    if let Err(e) = music::require_ytdlp().await {
        tracing::warn!("{e}");
    }
    if let Err(e) = music::require_ffmpeg().await {
        tracing::warn!("{e}");
    }
    client.start().await?;
    Ok(())
}

#[serenity::async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: serenity::all::Ready) {
        tracing::info!("Connected as {} (id={})", ready.user.name, ready.user.id);

        let commands = [
            (
                "join",
                "Join your current voice channel (test voice connection)",
                None,
            ),
            (
                "play",
                "Play a song (YouTube URL or search query)",
                Some(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "query",
                        "Song name or YouTube URL",
                    )
                    .required(true),
                ),
            ),
            ("skip", "Skip the current song", None),
            ("pause", "Pause playback", None),
            ("resume", "Resume playback", None),
            ("stop", "Stop playback and leave the voice channel", None),
        ];

        if let Some(guild_id) = self.guild_id {
            let guild = serenity::all::GuildId::new(guild_id);
            for (name, description, option) in commands {
                let mut builder = CreateCommand::new(name).description(description);
                if let Some(opt) = option {
                    builder = builder.add_option(opt);
                }
                match builder.execute(&ctx.http, (Some(guild), None)).await {
                    Ok(cmd) => tracing::info!("Registered /{} (id={})", cmd.name, cmd.id),
                    Err(e) => tracing::error!("Failed to register /{}: {e}", name),
                }
            }
        } else {
            tracing::warn!("GUILD_ID not set — slash commands were not registered");
        }
    }

    async fn voice_state_update(
        &self,
        ctx: Context,
        _before: Option<serenity::all::VoiceState>,
        after: serenity::all::VoiceState,
    ) {
        if after.user_id == ctx.cache.current_user().id {
            tracing::info!("Bot voice state update: channel={:?}", after.channel_id);
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let Some(cmd) = interaction.into_command() else {
            return;
        };

        let Some(guild_id) = cmd.guild_id else {
            let _ = cmd
                .create_response(&ctx.http, ack("This command only works in a server."))
                .await;
            return;
        };

        match cmd.data.name.as_str() {
            "join" => self.handle_join(&ctx, &cmd, guild_id).await,
            "play" => self.handle_play(&ctx, &cmd, guild_id).await,
            "skip" => {
                self.handle_result(&ctx, &cmd, "⏭ Skipped.", music::skip(&self.songbird, guild_id))
                    .await
            }
            "pause" => {
                self.handle_result(&ctx, &cmd, "⏸ Paused.", music::pause(&self.songbird, guild_id))
                    .await
            }
            "resume" => {
                self.handle_result(&ctx, &cmd, "▶ Resumed.", music::resume(&self.songbird, guild_id))
                    .await
            }
            "stop" => {
                self.handle_result(
                    &ctx,
                    &cmd,
                    "⏹ Stopped and left voice channel.",
                    music::stop(&self.songbird, guild_id),
                )
                .await
            }
            other => tracing::warn!("Unknown slash command: {other}"),
        }
    }
}

impl Handler {
    fn user_channel(
        &self,
        ctx: &Context,
        user_id: UserId,
        guild_id: serenity::all::GuildId,
    ) -> Option<serenity::all::ChannelId> {
        music::user_voice_channel(ctx, user_id, guild_id)
    }

    async fn handle_join(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
        guild_id: serenity::all::GuildId,
    ) {
        let channel_id = match self.user_channel(ctx, cmd.user.id, guild_id) {
            Some(ch) => ch,
            None => {
                let _ = cmd
                    .create_response(&ctx.http, ack("You must be in a voice channel."))
                    .await;
                return;
            }
        };

        if let Err(e) = music::diagnose_voice_channel(ctx, guild_id, channel_id).await {
            let _ = cmd.create_response(&ctx.http, ack(e)).await;
            return;
        }

        let songbird = self.songbird.clone();
        let mut registered = self.track_events_registered.lock().await;
        match music::join_voice(&songbird, guild_id, channel_id, &mut registered).await {
            Ok(()) => {
                let _ = cmd
                    .create_response(
                        &ctx.http,
                        ack(format!("Joined voice channel <#{channel_id}>.")),
                    )
                    .await;
            }
            Err(e) => {
                let _ = cmd.create_response(&ctx.http, ack(e)).await;
            }
        }
    }

    async fn handle_play(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
        guild_id: serenity::all::GuildId,
    ) {
        let query = match music::option_string(cmd, "query") {
            Some(q) if !q.trim().is_empty() => q,
            _ => {
                let _ = cmd
                    .create_response(&ctx.http, ack("Please provide a song name or URL."))
                    .await;
                return;
            }
        };

        let channel_id = match self.user_channel(ctx, cmd.user.id, guild_id) {
            Some(ch) => ch,
            None => {
                let _ = cmd
                    .create_response(&ctx.http, ack("You must be in a voice channel."))
                    .await;
                return;
            }
        };

        if let Err(e) = music::diagnose_voice_channel(ctx, guild_id, channel_id).await {
            let _ = cmd.create_response(&ctx.http, ack(e)).await;
            return;
        }

        let _ = cmd
            .create_response(
                &ctx.http,
                CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new()),
            )
            .await;

        let mut registered = self.track_events_registered.lock().await;
        match music::play(
            self.songbird.clone(),
            guild_id,
            channel_id,
            query,
            self.http_client.clone(),
            &mut registered,
        )
        .await
        {
            Ok(title) => {
                let _ = cmd
                    .edit_response(
                        &ctx.http,
                        serenity::builder::EditInteractionResponse::new()
                            .content(format!("🎵 Added to queue: **{title}**")),
                    )
                    .await;
            }
            Err(e) => {
                let _ = cmd
                    .edit_response(
                        &ctx.http,
                        serenity::builder::EditInteractionResponse::new().content(e),
                    )
                    .await;
            }
        }
    }

    async fn handle_result(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
        success: &str,
        result: impl std::future::Future<Output = Result<(), String>>,
    ) {
        match result.await {
            Ok(()) => {
                let _ = cmd.create_response(&ctx.http, ack(success)).await;
            }
            Err(e) => {
                let _ = cmd.create_response(&ctx.http, ack(e)).await;
            }
        }
    }
}
