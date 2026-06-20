mod audio_pipe;
mod bot;
mod music;
mod playback;
mod smtc;
mod spotify;
mod spotify_status;
mod stream;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,songbird=debug,serenity::gateway=warn".into()),
        )
        .init();
    bot::run_bot().await
}
