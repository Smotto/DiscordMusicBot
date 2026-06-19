# Spotify + VoiceMeeter Setup

This document explains how to configure **another Windows PC** so the Discord music bot can play Spotify in voice chat. No Spotify API keys are required — the bot controls the **desktop Spotify app** on the host machine and captures its audio through **VoiceMeeter**.

**Commit:** `e3f3d4e` — *Add Spotify local playback streaming via Voicemeeter capture.*

---

## What the bot does

| Layer | Role |
|-------|------|
| **Discord** | `/spotify play` in a voice channel |
| **Spotify desktop** | Opens `spotify:track|album|playlist:…` on the host PC |
| **Windows SMTC** | Pause, resume, skip, and read now-playing metadata |
| **VoiceMeeter** | Routes Spotify output to a virtual capture bus |
| **ffmpeg (dshow)** | Captures the VoiceMeeter bus as live PCM |
| **Songbird** | Encodes and sends audio to Discord (DAVE-encrypted Opus) |

YouTube (`/play`) uses a separate path: **yt-dlp → ffmpeg → Discord**. Spotify reuses the same voice stack but captures local audio instead of downloading a stream.

---

## Requirements (per machine)

Install these on **every PC** that will run the bot with Spotify support:

| Requirement | Notes |
|-------------|-------|
| **Windows 10/11** | SMTC (System Media Transport Controls) is Windows-only |
| **Spotify desktop app** | Not the web player — the bot opens `spotify:` URIs |
| **Spotify Premium** | Recommended for reliable transport control |
| **VoiceMeeter** | [VB-Audio VoiceMeeter](https://vb-audio.com/Voicemeeter/) (free) |
| **ffmpeg** | On `PATH` — used for dshow capture |
| **Rust + bot repo** | Same as main README |

The bot process must run on the **same machine** as Spotify and VoiceMeeter. Discord users in other locations only send slash commands; audio is captured locally.

---

## The confusing part: Input vs Out

Windows exposes **two different device lists**. Mixing them up is the most common setup mistake.

| List | Where you see it | Direction | Example name |
|------|------------------|-----------|--------------|
| **Playback devices** | Spotify → Settings → Audio | App **sends into** VoiceMeeter | `Voicemeeter Input (VB-Audio Voicemeeter VAIO)` |
| **Recording devices** | ffmpeg `-list_devices`, bot `.env` | Bot **pulls out of** VoiceMeeter | `Voicemeeter Out B2 (VB-Audio Voicemeeter VAIO)` |

- **Input** = send audio *into* VoiceMeeter (Spotify picks this).
- **Out B2** = pull audio *out* on bus B2 (bot captures this).

**Do not** put `Voicemeeter Input` in `STREAM_AUDIO_DEVICE`. The bot reads from an **Out** device.

---

## Audio routing

Recommended layout (bus **B2** for Spotify; keep **B1** free if you use it for a mic):

```text
Spotify desktop
      │
      ▼  (Playback device)
Voicemeeter Input strip
      │
      ├── B2 ON  ──►  Voicemeeter Out B2  ──►  ffmpeg dshow  ──►  Discord VC
      ├── B1 OFF     (often used for mic — leave off on this strip)
      └── A1 OFF     (physical speakers — off avoids hearing Spotify twice)
```

**Rule:** the lit **B\*** button on the **Input strip** must match **Out B\*** in `.env`. If **B2** is on, capture `Voicemeeter Out B2`.

You do **not** need VB-Cable for this setup.

---

## Step-by-step setup (new computer)

### 1. Install software

1. Install [VoiceMeeter](https://vb-audio.com/Voicemeeter/) and reboot if prompted.
2. Install [Spotify desktop](https://www.spotify.com/download/).
3. Install ffmpeg (e.g. `winget install Gyan.FFmpeg`).
4. Clone the bot repo and configure `.env` (see main [README](../README.md)).

### 2. Route Spotify into VoiceMeeter

1. Open **Spotify** → **Settings** → **Audio**.
2. Set output to **Voicemeeter Input (VB-Audio Voicemeeter VAIO)**.
3. Play any track.
4. Open **VoiceMeeter** — the **Voicemeeter Input** strip meters should move.

### 3. Enable the correct bus

On the **Voicemeeter Input** strip (the one that moves when Spotify plays):

- Turn **B2** **ON** (or whichever bus you standardize on).
- Turn **B1** **OFF** unless you intentionally share that bus.
- Turn **A1** **OFF** on this strip (optional but avoids duplicate local playback).

### 4. Find the exact capture device name

Device strings must match **exactly** (including parentheses). List recording devices:

```powershell
ffmpeg -list_devices true -f dshow -i dummy
```

Look under **DirectShow audio devices** for something like:

```text
"Voicemeeter Out B2 (VB-Audio Voicemeeter VAIO)"
```

Copy that full string into `.env`.

Quick sanity check while Spotify is playing:

```powershell
ffmpeg -f dshow -i audio="Voicemeeter Out B2 (VB-Audio Voicemeeter VAIO)" -t 5 -f null -
```

You should see non-zero `size=` in the ffmpeg output. If size stays `0kB`, routing or the device name is wrong.

### 5. Configure the bot

Add to `.env` (optional — defaults match B2):

```env
STREAM_AUDIO_DEVICE=Voicemeeter Out B2 (VB-Audio Voicemeeter VAIO)
STREAM_VOLUME=0.99
```

| Variable | Default | Purpose |
|----------|---------|---------|
| `STREAM_AUDIO_DEVICE` | `Voicemeeter Out B2 (VB-Audio Voicemeeter VAIO)` | ffmpeg dshow capture source |
| `STREAM_VOLUME` | `0.99` | ffmpeg `volume=` filter before Discord (0.0–1.0+) |

### 6. Run the bot and verify

```powershell
cargo run
```

While Spotify is playing **locally** (not during an active `/spotify play`):

```
/spotify probe
```

Expected output:

```text
device=`Voicemeeter Out B2 (VB-Audio Voicemeeter VAIO)`
bytes_read=1332856
pcm_stream=true
```

- `bytes_read > 0` — ffmpeg is receiving audio.
- `pcm_stream=true` — capture path is healthy.

Then test full playback:

1. Join a voice channel.
2. `/spotify play url:https://open.spotify.com/track/…`
3. You should hear the track in Discord.

---

## Bot commands (Spotify)

| Command | Description |
|---------|-------------|
| `/spotify play url:<link>` | Open link in desktop Spotify and stream to Discord |
| `/spotify probe` | Test ffmpeg capture (run while Spotify plays, **not** during `/spotify play`) |
| `/spotify pause` | Pause Spotify (SMTC) |
| `/spotify resume` | Resume Spotify |
| `/spotify skip` | Next track |
| `/spotify previous` | Previous track |
| `/spotify now` | Show current track metadata |
| `/pause` / `/resume` / `/skip` | Work for whichever mode is active (YouTube or Spotify) |
| `/stop` | Stop playback and leave voice |

Use `/spotify play`, not `/play`, for Spotify URLs.

---

## How `/spotify play` works internally

```text
User: /spotify play url:…
  │
  ├─► Defer Discord response (3s limit)
  ├─► Join voice channel + wait for DAVE ready
  ├─► SMTC: pause → open spotify: URI → wait until Playing
  ├─► ffmpeg: dshow → stereo f32le PCM @ 48 kHz (pipe)
  ├─► Pre-buffer ~32 KB PCM
  ├─► Songbird RawAdapter (live stream, no WAV/Ogg container)
  ├─► Enqueue track → Discord voice (Opus over DAVE)
  └─► Edit Discord response: "Playing Spotify …"
```

**Why RawAdapter / f32le?** Live dshow capture has unknown length. WAV makes symphonia wait for EOF; Ogg can stall on page boundaries. Raw PCM + Songbird's `RawAdapter` is designed for open-ended streams.

**Single capture lock:** Windows dshow allows only one ffmpeg session per device. Do not run `/spotify probe` while `/spotify play` is active.

---

## Troubleshooting

| Symptom | Likely cause | Fix |
|---------|--------------|-----|
| Probe: `bytes_read=0` | Wrong bus or device name | B2 on Input strip; verify Out B2 name with ffmpeg `-list_devices` |
| Probe works, Discord silent | Voice/DAVE issue | Confirm YouTube `/play` works; see [voice-dave-fixes.md](voice-dave-fixes.md) |
| "Timed out starting Discord stream" | Capture or voice join slow | Run probe first; ensure bot joined VC before play |
| Spotify opens wrong track | Previous track still playing | Bot pauses via SMTC before opening URI — ensure desktop app is focused |
| `/spotify play` hangs after "parsed" | (Fixed in e3f3d4e) mutex deadlock on enqueue | Update to latest `master` |
| No SMTC / pause fails | Web player instead of desktop | Install and use Spotify desktop app |
| Device busy | Probe + play at same time | Stop play, wait a few seconds, probe again |

### Log lines to expect (success)

```text
Opening Spotify URI: spotify:track:…
Spotify stream: opening capture on "Voicemeeter Out B2 …"
Voicemeeter capture ready: 32768 bytes f32 PCM pre-buffered from "…"
Voicemeeter capture parsed for Discord playback
Spotify capture enqueued for guild …
Spotify track after enqueue: playing=…
```

---

## Using a different bus (B1, B3, …)

Any VoiceMeeter bus works as long as Input and Out match:

1. On the **Input strip**, enable only **B1** (for example).
2. Set `STREAM_AUDIO_DEVICE=Voicemeeter Out B1 (VB-Audio Voicemeeter VAIO)`.
3. Re-run `/spotify probe`.

---

## Limitations

- **Windows only** for Spotify control (SMTC).
- **Links only** — track, album, or playlist URLs; no in-bot search.
- Controls **the host PC's** Spotify account, not per-Discord-user accounts.
- One capture device at a time per machine.
- Album/playlist links open in Spotify; Discord hears whatever Spotify plays next (skip with `/spotify skip`).

---

## Related docs

- [README](../README.md) — bot setup, YouTube commands, invite link
- [voice-dave-fixes.md](voice-dave-fixes.md) — Discord voice / DAVE troubleshooting
