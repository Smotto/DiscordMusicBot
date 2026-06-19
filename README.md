# Discord Music Bot

A Discord music bot written in Rust, forked from the voice stack in [DiscordTranslator](C:\Development\DiscordTranslator).

Uses a **patched local Songbird** (`crates/songbird`) that fixes Discord voice gateway parsing issues (the root cause of `gateway response from Discord timed out` with stock crates.io Songbird).

## Prerequisites

### Rust

Install Rust via [rustup](https://rustup.rs/).

### System Dependencies

The bot requires **yt-dlp** and **ffmpeg** on `PATH`:

```bash
# Windows (winget)
winget install yt-dlp.yt-dlp Gyan.FFmpeg

# macOS (Homebrew)
brew install yt-dlp ffmpeg

# Linux (apt)
sudo apt install yt-dlp ffmpeg
```

## Setup

1. Create a bot on the [Discord Developer Portal](https://discord.com/developers/applications) and copy the token.

2. Create a `.env` file:

```env
DISCORD_TOKEN=your_bot_token_here
GUILD_ID=your_guild_id_here
```

`GUILD_ID` is used to register slash commands to your server on startup (faster than global registration).

3. Invite the bot with **Connect** and **Speak** permissions:

```
https://discord.com/api/oauth2/authorize?client_id=YOUR_CLIENT_ID&permissions=3146752&scope=bot%20applications.commands
```

## Running

```bash
cargo run
```

## Commands

| Command | Description |
|---------|-------------|
| `/play <query>` | Play a song (YouTube URL or search query) |
| `/skip` | Skip to the next song in queue |
| `/pause` | Pause the current song |
| `/resume` | Resume a paused song |
| `/stop` | Stop playback and leave the voice channel |

## Architecture

- **serenity** `EventHandler` (no poise) — same pattern as DiscordTranslator
- **songbird** from `crates/songbird` — vendored fork with voice gateway fixes
- **yt-dlp** via Songbird's `YoutubeDl` input for playback
