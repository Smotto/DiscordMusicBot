use crate::music;
use crate::playback::GuildPlayback;
use crate::spotify;
use crate::stream;
use serenity::all::{
    CommandInteraction, CommandOptionType, Context, Interaction, UserId,
};
use serenity::builder::{
    Builder, CreateCommand, CreateCommandOption, CreateInteractionResponse,
    CreateInteractionResponseMessage,
};
use serenity::prelude::*;
use songbird::{Config as SongbirdConfig, SerenityInit, Songbird};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

pub struct Handler {
    pub guild_id: Option<u64>,
    pub http_client: reqwest::Client,
    pub songbird: Arc<Songbird>,
    pub track_events_registered: Arc<Mutex<HashSet<u64>>>,
    pub voice_idle: Arc<music::VoiceIdleManager>,
    pub playback_modes: Arc<crate::playback::PlaybackModes>,
    pub stream_sessions: Arc<Mutex<HashMap<u64, stream::StreamSession>>>,
    pub spotify_queues: Arc<crate::playback::SpotifyQueues>,
    pub spotify_playback_guard: Arc<spotify::SpotifyPlaybackGuard>,
    pub spotify_session: Arc<crate::playback::SpotifySessionTracker>,
    pub spotify_queue_watcher: Arc<Mutex<Option<tokio::task::AbortHandle>>>,
    /// In-flight `/spotify play` tasks — aborted by `/stop`.
    pub spotify_play_tasks: Arc<Mutex<HashMap<u64, tokio::task::AbortHandle>>>,
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
    let playback_modes = crate::playback::PlaybackModes::new();
    let spotify_queues = crate::playback::SpotifyQueues::new();
    let spotify_playback_guard = spotify::SpotifyPlaybackGuard::new();
    let spotify_session = crate::playback::SpotifySessionTracker::new();
    let voice_idle = music::VoiceIdleManager::new(playback_modes.clone());
    let handler = Handler {
        guild_id,
        http_client: reqwest::Client::new(),
        songbird: songbird.clone(),
        track_events_registered: Arc::new(Mutex::new(HashSet::new())),
        voice_idle: voice_idle.clone(),
        playback_modes,
        stream_sessions: Arc::new(Mutex::new(HashMap::new())),
        spotify_queues,
        spotify_playback_guard,
        spotify_session,
        spotify_queue_watcher: Arc::new(Mutex::new(None)),
        spotify_play_tasks: Arc::new(Mutex::new(HashMap::new())),
    };

    let mut client = Client::builder(token, intents)
        .register_songbird_with(songbird)
        .event_handler(handler)
        .await?;

    tracing::info!("Music bot is starting...");
    tracing::info!("Spotify stream capture device: {}", crate::stream::capture_device());
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

        // YouTube commands
        let yt_commands = [
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
            ("skip", "Skip the current song / Spotify track", None),
            ("pause", "Pause playback (YouTube or Spotify)", None),
            ("resume", "Resume playback (YouTube or Spotify)", None),
            ("stop", "Stop playback and leave the voice channel", None),
        ];

        // Spotify subcommands
        let spotify_play_opt = CreateCommandOption::new(
            CommandOptionType::SubCommand,
            "play",
            "Play a Spotify track, album, or playlist",
        )
        .add_sub_option(
            CreateCommandOption::new(
                CommandOptionType::String,
                "url",
                "Spotify track, album, or playlist URL",
            )
            .required(true),
        );

        let spotify_commands = CreateCommand::new("spotify").description("Control Spotify on this PC")
            .add_option(spotify_play_opt)
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "pause",
                "Pause Spotify playback",
            ))
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "resume",
                "Resume Spotify playback",
            ))
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "skip",
                "Skip to the next Spotify track",
            ))
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "previous",
                "Go back to the previous Spotify track",
            ))
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "now",
                "Show the currently playing Spotify track",
            ))
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "queue",
                "Show queued Spotify tracks",
            ))
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "probe",
                "Test if the bot can hear your VoiceMeeter output (play Spotify first)",
            ));

        if let Some(guild_id) = self.guild_id {
            let http = ctx.http.clone();
            tokio::spawn(async move {
                let guild = serenity::all::GuildId::new(guild_id);

                for (name, description, option) in yt_commands {
                    let mut builder = CreateCommand::new(name).description(description);
                    if let Some(opt) = option {
                        builder = builder.add_option(opt);
                    }
                    match builder.execute(&http, (Some(guild), None)).await {
                        Ok(cmd) => tracing::info!("Registered /{} (id={})", cmd.name, cmd.id),
                        Err(e) => tracing::error!("Failed to register /{}: {e}", name),
                    }
                }

                match spotify_commands.execute(&http, (Some(guild), None)).await {
                    Ok(cmd) => tracing::info!("Registered /spotify (id={})", cmd.id),
                    Err(e) => tracing::error!("Failed to register /spotify: {e}"),
                }
            });
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
            "skip" => self.handle_skip_or_spotify_skip(&ctx, &cmd, guild_id).await,
            "pause" => self.handle_pause_or_spotify_pause(&ctx, &cmd, guild_id).await,
            "resume" => self.handle_resume_or_spotify_resume(&ctx, &cmd, guild_id).await,
            "stop" => self.handle_stop(&ctx, &cmd, guild_id).await,
            "spotify" => self.handle_spotify(&ctx, &cmd, guild_id).await,
            other => tracing::warn!("Unknown slash command: {other}"),
        }
    }
}

impl Handler {
    /// Abort a running `/spotify play` background task for this guild, if any.
    async fn spotify_stream_active(&self) -> bool {
        !self.stream_sessions.lock().await.is_empty()
    }

    async fn stop_spotify_queue_watcher(&self) {
        if let Some(handle) = self.spotify_queue_watcher.lock().await.take() {
            handle.abort();
        }
    }

    async fn cancel_spotify_play(&self, guild_id: serenity::all::GuildId) {
        if let Some(handle) = self.spotify_play_tasks.lock().await.remove(&guild_id.get()) {
            handle.abort();
            tracing::info!("Aborted in-flight /spotify play for guild {guild_id}");
        }
    }

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
        match music::join_voice(
            &songbird,
            guild_id,
            channel_id,
            self.track_events_registered.clone(),
            self.voice_idle.clone(),
        )
        .await
        {
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

        let q_lower = query.to_lowercase();
        if q_lower.contains("open.spotify.com") || q_lower.starts_with("spotify:") {
            let _ = cmd
                .create_response(
                    &ctx.http,
                    ack(
                        "That's a Spotify link. Use `/spotify play` with the `url` option, not `/play`.\n\
                         Example: `/spotify play url:https://open.spotify.com/track/...`",
                    ),
                )
                .await;
            return;
        }

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

        // If Spotify mode is active, stop the stream first
        let current_mode = self.playback_modes.get(guild_id).await;
        if current_mode == GuildPlayback::Spotify {
            crate::stream::stop_guild_stream(
                &self.songbird,
                guild_id,
                &self.stream_sessions,
            )
            .await
            .ok();
        }

        match music::play(
            self.songbird.clone(),
            guild_id,
            channel_id,
            query,
            self.http_client.clone(),
            self.track_events_registered.clone(),
            self.voice_idle.clone(),
            &self.stream_sessions,
        )
        .await
        {
            Ok(title) => {
                // Set mode to Youtube on success
                self.playback_modes
                    .set(guild_id, GuildPlayback::Youtube)
                    .await;
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

    // --- Transport commands routed by mode ---

    async fn handle_skip_or_spotify_skip(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
        guild_id: serenity::all::GuildId,
    ) {
        let mode = self.playback_modes.get(guild_id).await;
        let spotify_active = self.spotify_stream_active().await;
        let result: Result<(), String> = if spotify_active {
            spotify::skip_or_play_next(
                &self.spotify_queues,
                &self.spotify_playback_guard,
                &self.spotify_session,
            )
            .await
        } else {
            match mode {
                GuildPlayback::Youtube => music::skip(&self.songbird, guild_id).await,
                GuildPlayback::Idle => Err("Nothing is playing.".into()),
                GuildPlayback::Spotify => Err("Nothing is playing.".into()),
            }
        };
        self.handle_result(
            ctx,
            cmd,
            if spotify_active {
                "⏭ Skipped Spotify track."
            } else {
                match mode {
                    GuildPlayback::Youtube => "⏭ Skipped.",
                    _ => "⏭ Skipped.",
                }
            },
            result,
        )
        .await;
    }

    async fn handle_pause_or_spotify_pause(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
        guild_id: serenity::all::GuildId,
    ) {
        let mode = self.playback_modes.get(guild_id).await;
        let spotify_active = self.spotify_stream_active().await;
        let result: Result<(), String> = if spotify_active {
            crate::smtc::run_smtc(crate::smtc::pause).await
        } else {
            match mode {
                GuildPlayback::Youtube => music::pause(&self.songbird, guild_id).await,
                GuildPlayback::Idle => Err("Nothing is playing.".into()),
                GuildPlayback::Spotify => Err("Nothing is playing.".into()),
            }
        };
        self.handle_result(
            ctx,
            cmd,
            if spotify_active {
                "⏸ Paused Spotify."
            } else {
                match mode {
                    GuildPlayback::Youtube => "⏸ Paused.",
                    _ => "⏸ Paused.",
                }
            },
            result,
        )
        .await;
    }

    async fn handle_resume_or_spotify_resume(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
        guild_id: serenity::all::GuildId,
    ) {
        let mode = self.playback_modes.get(guild_id).await;
        let spotify_active = self.spotify_stream_active().await;
        let result: Result<(), String> = if spotify_active {
            crate::smtc::run_smtc(crate::smtc::resume).await
        } else {
            match mode {
                GuildPlayback::Youtube => music::resume(&self.songbird, guild_id).await,
                GuildPlayback::Idle => Err("Nothing is playing.".into()),
                GuildPlayback::Spotify => Err("Nothing is playing.".into()),
            }
        };
        self.handle_result(
            ctx,
            cmd,
            if spotify_active {
                "▶ Resumed Spotify."
            } else {
                match mode {
                    GuildPlayback::Youtube => "▶ Resumed.",
                    _ => "▶ Resumed.",
                }
            },
            result,
        )
        .await;
    }

    async fn handle_stop(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
        guild_id: serenity::all::GuildId,
    ) {
        if let Err(e) = cmd
            .create_response(
                &ctx.http,
                CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new()),
            )
            .await
        {
            tracing::error!("Failed to defer /stop: {e}");
            return;
        }

        self.cancel_spotify_play(guild_id).await;

        let result = music::stop_all(
            &self.songbird,
            guild_id,
            self.track_events_registered.clone(),
            self.voice_idle.clone(),
            &self.stream_sessions,
            &self.playback_modes,
        )
        .await;

        let streams_empty = self.stream_sessions.lock().await.is_empty();
        if streams_empty {
            self.spotify_queues.clear().await;
            self.spotify_session.clear().await;
            self.stop_spotify_queue_watcher().await;
        }

        let content = match result {
            Ok(()) => "⏹ Stopped and left voice channel.".to_string(),
            Err(e) => e,
        };

        if let Err(e) = cmd
            .edit_response(
                &ctx.http,
                serenity::builder::EditInteractionResponse::new().content(content),
            )
            .await
        {
            tracing::warn!("Failed to update /stop response: {e}");
        }
    }

    // --- Spotify subcommand handler ---

    async fn handle_spotify(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
        guild_id: serenity::all::GuildId,
    ) {
        // Extract subcommand
        let subcommand = cmd
            .data
            .options
            .iter()
            .find(|o| matches!(o.value, serenity::all::CommandDataOptionValue::SubCommand { .. }))
            .map(|o| o.name.as_str())
            .unwrap_or("play"); // default

        match subcommand {
            "play" => self.handle_spotify_play(ctx, cmd, guild_id).await,
            "pause" => {
                let result = crate::smtc::run_smtc(crate::smtc::pause).await;
                self.handle_result(ctx, cmd, "⏸ Paused Spotify.", result).await;
            }
            "resume" => {
                let result = crate::smtc::run_smtc(crate::smtc::resume).await;
                self.handle_result(ctx, cmd, "▶ Resumed Spotify.", result).await;
            }
            "skip" => {
                let result = spotify::skip_or_play_next(
                    &self.spotify_queues,
                    &self.spotify_playback_guard,
                    &self.spotify_session,
                )
                .await;
                self.handle_result(ctx, cmd, "⏭ Skipped Spotify track.", result)
                    .await;
            }
            "previous" => {
                let result = crate::smtc::run_smtc(crate::smtc::skip_previous).await;
                self.handle_result(ctx, cmd, "⏮ Previous Spotify track.", result).await;
            }
            "now" => self.handle_spotify_now(ctx, cmd).await,
            "queue" => self.handle_spotify_queue(ctx, cmd, guild_id).await,
            "probe" => self.handle_spotify_probe(ctx, cmd).await,
            other => {
                let _ = cmd
                    .create_response(
                        &ctx.http,
                        ack(format!("Unknown Spotify subcommand: {other}")),
                    )
                    .await;
            }
        }
    }

    async fn handle_spotify_play(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
        guild_id: serenity::all::GuildId,
    ) {
        let url = match music::subcommand_option_string(cmd, "play", "url") {
            Some(u) if !u.trim().is_empty() => u,
            _ => {
                let _ = cmd
                    .create_response(
                        &ctx.http,
                        ack("Please provide a Spotify track, album, or playlist URL.\nUse: `/spotify play url:<link>`"),
                    )
                    .await;
                return;
            }
        };

        let spotify_uri = match spotify::parse_spotify_input(&url) {
            Ok(uri) => uri,
            Err(e) => {
                let _ = cmd.create_response(&ctx.http, ack(e)).await;
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

        // Discord requires a response within 3s — defer before any HTTP/voice/capture work.
        if let Err(e) = cmd
            .create_response(
                &ctx.http,
                CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new()),
            )
            .await
        {
            tracing::error!("Failed to defer /spotify play: {e}");
            return;
        }

        // One Spotify desktop app → queue whenever any voice stream is up or starting.
        let spotify_active = self.spotify_stream_active().await
            || !self.spotify_play_tasks.lock().await.is_empty();
        if spotify_active {
            let meta = spotify::fetch_link_metadata(&self.http_client, &spotify_uri, &url).await;
            if let Some(current) = self.spotify_session.get_intentional().await {
                if current.same_uri(&meta.uri) {
                    let _ = cmd
                        .edit_response(
                            &ctx.http,
                            serenity::builder::EditInteractionResponse::new().content(format!(
                                "Already playing **{}**.",
                                current.display()
                            )),
                        )
                        .await;
                    return;
                }
            }
            if let Ok(Some(np)) = crate::smtc::run_smtc(crate::smtc::now_playing).await {
                if spotify::metadata_matches_track(&np, &meta) {
                    let _ = cmd
                        .edit_response(
                            &ctx.http,
                            serenity::builder::EditInteractionResponse::new().content(format!(
                                "Already playing **{}**.",
                                meta.display()
                            )),
                        )
                        .await;
                    return;
                }
            }
            if self.spotify_queues.contains_uri(&meta.uri).await {
                let _ = cmd
                    .edit_response(
                        &ctx.http,
                        serenity::builder::EditInteractionResponse::new().content(format!(
                            "Already in queue: **{}**.",
                            meta.display()
                        )),
                    )
                    .await;
                return;
            }
            let position = self.spotify_queues.push(meta.clone()).await;
            let _ = cmd
                .edit_response(
                    &ctx.http,
                    serenity::builder::EditInteractionResponse::new().content(format!(
                        "🎵 Queued `#{position}`: **{}**\n\
                         Use `/skip` for the next track, or `/spotify queue` to see the list.",
                        meta.display()
                    )),
                )
                .await;
            return;
        }

        let device = crate::stream::capture_device();
        let stream_sessions = self.stream_sessions.clone();
        let songbird = self.songbird.clone();
        let registered = self.track_events_registered.clone();
        let idle = self.voice_idle.clone();
        let playback_modes = self.playback_modes.clone();
        let spotify_queues = self.spotify_queues.clone();
        let spotify_playback_guard = self.spotify_playback_guard.clone();
        let spotify_session = self.spotify_session.clone();
        let spotify_queue_watcher = self.spotify_queue_watcher.clone();
        let http = ctx.http.clone();
        let ctx = ctx.clone();
        let cmd = cmd.clone();
        let spotify_play_tasks = self.spotify_play_tasks.clone();

        self.cancel_spotify_play(guild_id).await;

        let play_task = tokio::spawn(async move {
            let cleanup = || {
                let spotify_play_tasks = spotify_play_tasks.clone();
                let guild_id = guild_id;
                async move {
                    spotify_play_tasks.lock().await.remove(&guild_id.get());
                }
            };
            let edit = |content: String| {
                let cmd = cmd.clone();
                let http = http.clone();
                async move {
                    if let Err(e) = cmd
                        .edit_response(
                            &http,
                            serenity::builder::EditInteractionResponse::new().content(content),
                        )
                        .await
                    {
                        tracing::warn!("Failed to update /spotify play response: {e}");
                    }
                }
            };

            if let Err(e) = music::diagnose_voice_channel(&ctx, guild_id, channel_id).await {
                edit(e).await;
                cleanup().await;
                return;
            }

            let current_mode = playback_modes.get(guild_id).await;
            if current_mode == GuildPlayback::Youtube {
                if let Some(handler_lock) = songbird.get(guild_id) {
                    let mut handler = handler_lock.lock().await;
                    handler.queue().stop();
                    handler.stop();
                }
            }

            edit("🎵 Joining voice channel…".to_string()).await;

            if let Err(e) = music::ensure_in_voice(
                &songbird,
                guild_id,
                channel_id,
                registered.clone(),
                idle.clone(),
            )
            .await
            {
                edit(format!("Failed to join voice channel:\n{e}")).await;
                cleanup().await;
                return;
            }

            if let Err(e) =
                music::wait_for_voice_driver(&songbird, guild_id, Duration::from_secs(20)).await
            {
                edit(format!("Voice connection did not come up:\n{e}")).await;
                cleanup().await;
                return;
            }

            edit("🎵 Pausing Spotify and opening your link…".to_string()).await;

            let now_playing = match spotify::start_local_playback(&spotify_uri).await {
                Ok(np) => np,
                Err(e) => {
                    edit(format!("Failed to start Spotify playback:\n{e}")).await;
                    cleanup().await;
                    return;
                }
            };

            let track = spotify::track_info_from_smtc(&now_playing, &spotify_uri);
            spotify_playback_guard.note().await;
            spotify_session.set_intentional(track.clone()).await;
            edit(format!(
                "🎵 Spotify is playing — waiting for audio on:\n\
                 `{device}`"
            ))
            .await;

            match tokio::time::timeout(
                Duration::from_secs(90),
                stream::start_guild_stream(&songbird, guild_id, idle),
            )
            .await
            {
                Ok(Ok(session)) => {
                    stream_sessions.lock().await.insert(guild_id.get(), session);
                    playback_modes
                        .set(guild_id, GuildPlayback::Spotify)
                        .await;
                    {
                        let mut slot = spotify_queue_watcher.lock().await;
                        if slot.is_none() {
                            let watcher = spotify::spawn_queue_watcher(
                                spotify_queues.clone(),
                                spotify_playback_guard.clone(),
                                spotify_session.clone(),
                                stream_sessions.clone(),
                            );
                            *slot = Some(watcher.abort_handle());
                        }
                    }
                    tracing::info!("Spotify cable stream active for guild {guild_id}");
                    let queued = spotify_queues.len().await;
                    let queue_note = if queued > 0 {
                        format!("\n`{queued}` track(s) queued — `/skip` for next.")
                    } else {
                        String::new()
                    };
                    edit(format!(
                        "🎵 Playing Spotify.\n\
                         `{}`\n\
                         Streaming via `{device}`.{queue_note}",
                        track.display(),
                    ))
                    .await;
                }
                Ok(Err(e)) => {
                    tracing::error!("Failed to start Spotify cable stream: {e}");
                    edit(format!(
                        "Spotify is playing locally but voice stream failed:\n{e}\n\n\
                         **VoiceMeeter checklist:**\n\
                         1. Spotify output → **Voicemeeter Input**\n\
                         2. When Spotify plays, the moving strip needs the bus lit that matches your `.env` Out device (e.g. **B2** for Out B2)\n\
                         3. Test: `/spotify probe` while Spotify is playing (not during `/spotify play`)"
                    ))
                    .await;
                }
                Err(_) => {
                    tracing::error!("Spotify stream timed out for guild {guild_id}");
                    edit(format!(
                        "Timed out starting Discord stream for `{}`.\n\
                         Audio on `{device}` or voice join took too long.\n\
                         Try `/spotify probe` while Spotify plays (not during `/spotify play`).",
                        track.display(),
                    ))
                    .await;
                }
            }
            cleanup().await;
        });

        self.spotify_play_tasks
            .lock()
            .await
            .insert(guild_id.get(), play_task.abort_handle());
    }

    async fn handle_spotify_probe(&self, ctx: &Context, cmd: &CommandInteraction) {
        let _ = cmd
            .create_response(
                &ctx.http,
                CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new()),
            )
            .await;

        let device = crate::stream::capture_device();
        let volume = crate::stream::capture_volume();
        let result =
            tokio::task::spawn_blocking(move || crate::stream::probe_capture(&device, volume, 4))
                .await;

        let msg = match result {
            Ok(Ok(report)) => format!(
                "**Capture probe** (Spotify should be playing):\n```\n{report}\n```"
            ),
            Ok(Err(e)) => format!("**Capture probe failed:**\n{e}"),
            Err(e) => format!("Probe task failed: {e}"),
        };

        let _ = cmd
            .edit_response(
                &ctx.http,
                serenity::builder::EditInteractionResponse::new().content(msg),
            )
            .await;
    }

    async fn handle_spotify_now(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
    ) {
        let result = crate::smtc::run_smtc(crate::smtc::now_playing).await;
        match result {
            Ok(Some(np)) => {
                let title = np.title.as_deref().unwrap_or("Unknown");
                let artist = np.artist.as_deref().unwrap_or("Unknown");
                let album = np.album.as_deref().unwrap_or("Unknown");
                let msg = format!(
                    "🎵 **{title}** by {artist}\nAlbum: {album}",
                    title = title,
                    artist = artist,
                    album = album,
                );
                let _ = cmd.create_response(&ctx.http, ack(msg)).await;
            }
            Ok(None) => {
                let _ = cmd
                    .create_response(&ctx.http, ack("No track currently playing in Spotify."))
                    .await;
            }
            Err(e) => {
                let _ = cmd.create_response(&ctx.http, ack(e)).await;
            }
        }
    }

    async fn handle_spotify_queue(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
        _guild_id: serenity::all::GuildId,
    ) {
        let _ = cmd
            .create_response(
                &ctx.http,
                CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new()),
            )
            .await;

        let now_playing = if self.spotify_stream_active().await {
            crate::smtc::run_smtc(crate::smtc::now_playing)
                .await
                .ok()
                .flatten()
                .map(|np| spotify::format_now_playing(&np))
        } else {
            None
        };

        let queue = self.spotify_queues.list().await;
        let body = spotify::format_queue_message(now_playing.as_deref(), &queue);

        let _ = cmd
            .edit_response(
                &ctx.http,
                serenity::builder::EditInteractionResponse::new().content(body),
            )
            .await;
    }

    async fn handle_result(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
        success: &str,
        result: Result<(), String>,
    ) {
        match result {
            Ok(()) => {
                let _ = cmd.create_response(&ctx.http, ack(success)).await;
            }
            Err(e) => {
                let _ = cmd.create_response(&ctx.http, ack(e)).await;
            }
        }
    }
}
