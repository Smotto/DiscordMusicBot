# Voice & DAVE Fixes (June 2026)

This document explains the voice playback issues we hit while building this bot, how we diagnosed them, and what we changed in the vendored Songbird fork.

**Commit:** `798901f` — *Fix stable DAVE voice playback without disruptive channel reconnects.*

---

## Symptoms

| Symptom | When |
|---------|------|
| Voice join + DAVE handshake succeed, track starts | Always worked |
| `4006 SessionInvalid` ~13.8s after connect | Every connection, very reproducible |
| Audio dropouts / bot appears to leave and rejoin VC | During broken recovery attempts |
| `gateway response from Discord timed out` | Earlier issue (stock Songbird parsing) |

The ~**13.75s** timing matched Discord's voice **heartbeat interval** (`13750ms` in Hello), not track load time or play duration.

---

## Root cause: Voice Gateway v8 heartbeats

We connect with **voice gateway version 8** (`VOICE_GATEWAY_VERSION = 8` in `crates/songbird/src/constants.rs`).

Since v8, every heartbeat (opcode 3) must include the last seen sequence number:

```json
{
  "op": 3,
  "d": {
    "t": 6490088333504143,
    "seq_ack": 13
  }
}
```

Upstream Songbird 0.6.0 still sent the **pre-v8** format (bare nonce only). Our fork stripped `seq` from incoming JSON to fix deserialization, but **never tracked or echoed it back**.

Discord tolerated the connection through DAVE setup and initial playback, then closed with **4006** on the first heartbeat.

### Fix

**`crates/songbird/src/ws.rs`**

- Track `last_seq` on `WsStream` from JSON `seq` fields and from the 2-byte prefix on binary DAVE frames.
- Send v8 heartbeats via `send_text` with `{ "t": nonce, "seq_ack": last_seq }` (`-1` if none seen yet).
- Normalize v8 Heartbeat ACK payloads (`{"t": nonce}` → bare integer) so `serenity-voice-model` can deserialize opcode 6.

**`crates/songbird/src/driver/tasks/ws.rs`**

- Use `recv_event_with_seq_no_timeout()` and update `last_seq` on every inbound message.
- Seed `last_seq` from the handshake before the WS task takes over.

### Verification

After the fix, logs show periodic Heartbeat ACK (opcode 6) every ~13.75s and **no 4006**:

```
seq-stripped: {"op":6,"d":{"t":6490088333504143}} -> {"d":6490088333504143,"op":6}
```

Repeated every heartbeat interval while audio plays continuously.

---

## DAVE / RTP hardening

Discord now requires **DAVE** (end-to-end encryption) on voice. Several mixer/WS changes prevent invalid cleartext or mistimed media.

### Defer speaking and RTP until DAVE is ready

Discord invalidates sessions if cleartext Opus is sent on an E2E channel.

| Gate | File | Behavior |
|------|------|----------|
| `dave_media_allowed` | `driver/tasks/ws.rs`, `driver/tasks/mixer/mod.rs`, `driver/scheduler/task.rs` | No Speaking packet or RTP until `DaveExecuteTransition` |
| Re-assert speaking after transition | `driver/tasks/ws.rs` `execute_dave_transition` | Sends Speaking if microphone was already requested |
| Block cleartext in `prep_packet` | `driver/tasks/mixer/mod.rs` | Returns `WouldBlock` if DAVE active but not ready |

`dave_media_allowed` is set `false` on connect and flipped to `true` in `execute_dave_transition`.

### Disable Opus DTX

DAVE's `encrypt_opus` **passes through** the standard DTX silence frame `[0xF8, 0xFF, 0xFE]` unencrypted. Discord rejects that on E2E channels.

**`driver/tasks/mixer/mod.rs`** — `new_encoder()` calls `encoder.set_dtx(false)`.

### Disable Opus passthrough under DAVE

Passthrough sends pre-encoded Opus without re-encoding. With `PLAYBACK_VOLUME = 0.99` in `src/music.rs`, Songbird always decodes and re-encodes through its encoder on DAVE channels.

**`driver/tasks/mixer/mod.rs`** — `do_passthrough` is false when `dave_protocol_version != 0`.

### RTP sequence only advances on send

**`driver/scheduler/live.rs`** — `advance_rtp_counters()` runs only when `packet_len > 0`, avoiding sequence gaps when DAVE gates block a tick.

### DAVE silence handling

When no PCM is mixed on a DAVE channel, send near-silent PCM (`1e-5`) for up to 5 frames instead of the standard Opus DTX silence packet.

---

## Recovery: what we tried vs. what works

### Anti-pattern: Gateway leave/rejoin

We initially handled 4006 by calling `resync_voice_gateway_session()`:

1. Send `channel=None` on the main gateway (bot visibly leaves VC)
2. Sleep 250ms
3. Rejoin the same channel

This caused **channel flicker**, raced with `VoiceServerUpdate`, and often reconnected with stale credentials. **Removed entirely.**

### Anti-pattern: `SessionInvalidPending`

Dropping the entire voice driver on 4006 and waiting for fresh gateway credentials triggered the leave/rejoin flow above. **Removed** — 4006 now uses in-driver `Reconnect` like other recoverable WS errors.

### Current behavior

| Event | Action | Visible to users? |
|-------|--------|-------------------|
| Transient voice WS error / 4006 | `CoreMessage::Reconnect` in driver | No — bot stays in VC |
| Driver dead on `/play` | `reconnect_voice_driver_if_inactive()` | No — in-driver `FullReconnect` |
| Admin kicks bot / `channel=None` | `leave_local()` | Yes (expected) |

**`src/music.rs`** — removed `SessionInvalidCredentialRefresh` and all gateway resync logic.

**`crates/songbird/src/handler.rs`** — removed `resync_voice_gateway_session`, `refresh_gateway_credentials`, and `awaiting_voice_credentials`.

---

## Files changed (summary)

| Area | Files |
|------|-------|
| Gateway v8 heartbeats | `crates/songbird/src/ws.rs`, `crates/songbird/src/driver/tasks/ws.rs` |
| DAVE gating & encoding | `crates/songbird/src/driver/connection/mod.rs`, `driver/tasks/mixer/mod.rs`, `driver/tasks/ws.rs`, `driver/scheduler/task.rs`, `driver/scheduler/live.rs` |
| Recovery simplification | `crates/songbird/src/handler.rs`, `driver/tasks/mod.rs`, `driver/tasks/message/core.rs`, `src/music.rs` |
| Driver API | `crates/songbird/src/driver/mod.rs` (`full_reconnect`) |

---

## Debugging checklist

If voice breaks again:

1. **Check heartbeat timing** — 4006 at ~`heartbeat_interval` ms after connect → suspect missing/wrong `seq_ack`.
2. **Look for `seq-stripped` logs** — confirm `seq` is present on numbered opcodes during handshake.
3. **Confirm Heartbeat ACK (op 6)** — should appear every ~13.75s without 4006 following.
4. **DAVE handshake** — `DaveMlsWelcome` → `DaveExecuteTransition` → `Changing to SpeakingState(MICROPHONE)` order matters.
5. **No `channel=None` during playback** — if you see it, something is still triggering gateway leave/rejoin.
6. **Run release with logs** — `RUST_LOG=info cargo run --release` (DAVE lines are at `info` in our fork).

---

## References

- [Discord voice connections](https://discord.com/developers/docs/topics/voice-connections) — gateway versions, heartbeats, `seq_ack`
- [DAVE protocol](https://daveprotocol.com/) — E2E encryption, silence packet rules
- [Songbird 0.6.0](https://github.com/serenity-rs/songbird/releases/tag/v0.6.0) — upstream DAVE support (missing v8 heartbeat `seq_ack` at time of writing)
- [davey crate](https://crates.io/crates/davey) — Rust DAVE implementation used by Songbird
