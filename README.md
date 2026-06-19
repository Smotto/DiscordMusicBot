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
| `/join` | Join your current voice channel |
| `/play <query>` | Play a song (YouTube URL or search query) |
| `/skip` | Skip the current song / Spotify track |
| `/pause` | Pause playback (YouTube or Spotify) |
| `/resume` | Resume playback (YouTube or Spotify) |
| `/stop` | Stop playback and leave the voice channel |
| `/spotify play <url>` | Play a Spotify track, album, or playlist |
| `/spotify pause` | Pause Spotify playback |
| `/spotify resume` | Resume Spotify playback |
| `/spotify skip` | Skip to the next Spotify track |
| `/spotify previous` | Go back to the previous Spotify track |
| `/spotify now` | Show the currently playing Spotify track |

## Spotify (local, no API)

Control **Spotify desktop on the host PC** without any Spotify/Apple APIs. Audio reaches Discord via **VoiceMeeter → ffmpeg → Songbird**.

### Inputs vs outputs (the confusing part)

Windows shows **two different device lists**:

| List | Where you see it | Direction | Example |
|------|------------------|-----------|---------|
| **Playback** | Spotify output menu | App → VoiceMeeter | `Voicemeeter Input` |
| **Recording** | `ffmpeg -list_devices` (bot capture) | VoiceMeeter → app | `Voicemeeter Out B2` |

**Input** in the name = send audio *into* VoiceMeeter (Spotify picks this).  
**Out** in the name = pull audio *out* for the bot. You cannot put `Voicemeeter Input` in `STREAM_AUDIO_DEVICE`.

### Audio routing (recommended)

```text
Spotify  →  Voicemeeter Input
               ↓
         VoiceMeeter: on that strip, **B2** ON only  (A1 off; turn B1 off if mic uses it)
               ↓
Bot      →  Voicemeeter Out B2
               ↓
         Discord VC
```

No VB-Cable.

| Bus | Typical use |
|-----|-------------|
| **B2** | Spotify → Input strip routes here; bot captures **Out B2** |
| **B1** | Often used for mic — leave **off** on the Spotify strip |
| **A1** | Speakers — leave **off** on the Spotify strip (or you hear it twice) |

**Rule:** the green **B*** button on the Input strip must match **Out B*** in `.env` (your screenshot: **B2** on → use **Out B2**).

### Step-by-step

1. **Spotify** → Settings → Audio → **Voicemeeter Input (VB-Audio Voicemeeter VAIO)**
2. Open **VoiceMeeter**. Play a track — the **Voicemeeter Input** strip should move.
3. On **that strip**: **B2** on, **B1** off, **A1** off.
4. Bot default capture:

```env
STREAM_AUDIO_DEVICE=Voicemeeter Out B2 (VB-Audio Voicemeeter VAIO)
```

5. Test while Spotify is playing:

```powershell
ffmpeg -f dshow -i audio="Voicemeeter Out B2 (VB-Audio Voicemeeter VAIO)" -t 5 -f null -
```

### Setup checklist

1. Install **[VoiceMeeter](https://vb-audio.com/Voicemeeter/)**
2. Spotify output → **Voicemeeter Input**
3. Input strip → **B2** only
4. Confirm **Out B2** with ffmpeg (see above)
5. Optional `.env`:

```env
STREAM_AUDIO_DEVICE=Voicemeeter Out B2 (VB-Audio Voicemeeter VAIO)
STREAM_VOLUME=0.35
```

### How it works

- `/spotify play <url>` opens the Spotify link on your PC and starts streaming audio through the virtual cable to Discord
- `/spotify pause` `/spotify resume` `/spotify skip` `/spotify previous` use Windows SMTC to control Spotify playback
- `/spotify now` shows the currently playing track's metadata
- `/play <youtube>` after Spotify will stop the stream and switch to YouTube
- `/stop` stops everything and leaves the voice channel

### Limitations

- **Links only** — paste a Spotify track, album, or playlist URL (no search)
- Controls **your** Spotify account on the host PC
- Requires **Windows** for SMTC transport control (pause/resume/skip)
- Spotify Premium recommended for reliable playback control

## Architecture

- **serenity** `EventHandler` (no poise) — same pattern as DiscordTranslator
- **songbird** from `crates/songbird` — vendored fork with voice gateway fixes
- **yt-dlp** → **ffmpeg** → Songbird for YouTube playback (Ogg/Opus pipe)
- **VoiceMeeter (Input → Out B2) → ffmpeg** for Spotify playback (Ogg/Opus pipe)
- **Windows SMTC** for Spotify transport control (no API keys needed)

## Voice / DAVE troubleshooting

Discord voice now requires **DAVE** end-to-end encryption and **voice gateway v8** heartbeats. Our Songbird fork includes patches for both.

If you hit `4006 SessionInvalid`, dropouts, or channel flicker, see **[docs/voice-dave-fixes.md](docs/voice-dave-fixes.md)** for diagnosis and what we changed.
