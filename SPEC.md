# Open Display Protocol (ODP): version 0.1 draft

Status: **draft**, not frozen (2026-11-25). The reference implementation is this repository's `odp` crate.
This text is licensed under CC BY 4.0; the code in this repository under Apache-2.0.

Goals: a receiver can be written on any platform from this document alone. It must not depend on Omninput's private
formats (heartbeats on UDP 54321, the 39-byte input packets, `omninput-peer-v1`), which stay as they are for
Omninput-to-Omninput use. A host speaks both.

Terms: **host** sends a screen (or runs cinema); **receiver** shows it; **session** is one host–receiver link; **group**
is the set of receivers in one synced presentation.

## Versioning

- The protocol is `odp/<major>.<minor>`. Both sides send the versions they support in `hello`; they use the highest
  common one. A minor version only adds optional messages and fields; unknown fields and message types are ignored.
  A major version may break; hosts keep the previous major for at least a year.
- Capabilities, not versions, decide features (`caps` in L3): codecs, profiles, touch, audio, cinema, latency info.

## Layers

| Layer | What | Transport |
|---|---|---|
| L0 Discovery | finding receivers and hosts | DNS-SD over mDNS |
| L1 Control | one ordered, reliable channel per session | TCP (default port 54330) |
| L2 Pairing and keys | PIN/QR pairing, per-session keys, encryption | inside L1, then AEAD everywhere |
| L3 Session | capabilities, start/stop, mode, layout | L1 messages |
| L4 Media | video and audio packets with presentation times | UDP (port from L3), or the web profile |
| L5 Clock and sync | shared clock, latency reports, calibration | UDP (same socket as L4) |
| L6 Input | keyboard, pointer, touch from receiver to host | UDP (same socket), reliable keys via L1 |
| L7 Cinema | co-playback signalling | L1 messages |

### L0 Discovery
- Receivers advertise `_odp-receiver._tcp.local` with TXT records: `v=0.1`, `id=<receiver id>`, `fp=<key fingerprint>`,
  `name=<display name>`, `paired=0|1`, `role=receiver`.
- Hosts may advertise `_odp-host._tcp.local` (used by receivers for wake rules and to show "who can cast here").
- Receivers may also answer a unicast `probe` on L1's port, for networks that block multicast.
- Omninput hosts additionally see receivers in their own heartbeats (`role: "receiver"`); that is internal.

### L1 Control channel
- Framing: `u32` big-endian length (at most 64 KiB), then a JSON object, sealed on the control channel once the session
  keys exist. Each message has `t` (type) and, for requests, `id` (echoed in the reply). JSON (not CBOR) keeps it easy
  to implement and read everywhere; the messages are small.
- Message families: `hello`, `pair.*`, `session.*`, `clock.*` (bootstrap only), `input.key` (reliable key events),
  `cinema.*`, `error`, `bye`. Every reply to a request is either the matching `*.ok` or `error {code, text}`.

### L2 Pairing and encryption
- Each side has a long-term **Ed25519** identity key; fingerprint = first 16 hex of SHA-256 of the public key,
  shown `XXXX-XXXX-XXXX-XXXX`. (Omninput's P-256 key stays for its own protocol; hosts hold one ODP key too.)
- **Pairing with a PIN:** the receiver shows a 6-digit PIN. Both sides run **SPAKE2** (Ed25519 group; host is side A
  with identity `odp host`, receiver side B with `odp receiver`) with the PIN, derive `h2r` and `r2h` keys from the
  shared secret with HKDF-SHA-256 (salt `odp/0.1 pair`), and each sends its identity key sealed with ChaCha20-Poly1305
  under its direction's key. A wrong PIN fails the seal check and reveals nothing more; the receiver locks pairing for
  30 s after 3 failures.
- **Pairing with a QR code:** the QR carries `odp://pair?id=…&fp=…&pin=…`; scanning it on the host pins the
  fingerprint as well as supplying the PIN.
- **Every session:** an X25519 ephemeral exchange, signed by both identity keys, then HKDF-SHA-256 derives separate keys
  for control, media and input in each direction (HKDF-SHA-256, salt = the signed transcript, info `odp/0.1 h2r media`
  and so on). All L1 messages after the exchange and all UDP packets are sealed with **ChaCha20-Poly1305**: a sealed
  message is the 8-byte counter, then ciphertext and tag; nonce = 4-byte channel tag (`odpc`, `odpm`, `odpi`) ‖ the
  counter. Receivers accept a counter once, within 1024 of the highest seen. A session lasts at most 2^32 messages
  per channel or 24 h; then a new exchange.
- Unpairing removes the stored key; a receiver can be reset to forget all hosts (kiosk: a button on the boot screen).

### L3 Session
- `hello {versions, id, name, role, caps}`. Receiver caps: `decode` (list of `{codec, profile, level, max_w, max_h,
  max_fps, hw}`), `display` (`w, h, refresh_rates, hdr, edid_latency{video_ms, audio_ms}`), `audio` (channels, rates),
  `input` (`keyboard, pointer, touch{max_contacts}`), `cinema` (`{drm: [widevine|playready|fairplay], level}` or
  absent), `profile` (`rt` | `web`).
- `session.start {mode: mirror|extend|cinema, video{codec, w, h, fps, bitrate}, audio{codec, rate, channels},
  udp_port, group}` → `session.ok {udp_port}`.
- `session.update` (resolution, bitrate, cadence 24/30/60, group membership), `session.pause`, `session.stop`.
- `session.layout {screen_id, w_mm, h_mm}` lets the host place the receiver in its screen layout.

### L4 Media
- UDP datagrams ≤ 1200 bytes (fits common MTUs, IPv4 and IPv6). Header: `version(1) kind(1) stream(1) flags(1)
  seq(4) pts_us(8) frame(4) frag_index(2) frag_count(2)`, then the sealed payload.
- Video: H.264 (baseline/main, required), HEVC (main, optional), AV1 (optional); Annex-B access units split into
  fragments; IDR on request (`session.keyframe`), intra refresh preferred; loss handling by NACK (`media.nack {seqs}`)
  within the jitter budget, then XOR-FEC (one parity per N fragments, N negotiated), then a keyframe request.
- Audio: Opus, 10 ms frames, 48 kHz, stereo or multichannel; PCM allowed for the `rt` profile on LAN only.
- `pts_us` is on the **shared clock** (L5): the time the frame should be visible / the sample should be heard.
- Congestion: the receiver reports loss and arrival jitter each second (`media.report`); the host adapts bitrate
  (simple delay-based control).
- **Web profile** (TV platforms whose apps can't use UDP: webOS, Tizen, browsers): L1 over WebSocket (JSON), media as
  fragmented MP4 over the same WebSocket for Media Source Extensions, or WebRTC with the same L2 keys as identity.
  Higher latency (100–300 ms) and no cinema; enough for mirroring to a TV.

### L5 Clock and sync
- `clock.ping {t1}` / `clock.pong {t1, t2, t3}` on the UDP socket about once a second; the receiver keeps the
  lowest-delay samples and reports `offset_us`, `rtt_us` and its estimate of drift (ppm).
- `sync.target {delay_us}` from the host: the end-to-end delay the group plays at.
- `sync.latency {video_us, audio_us, source: edid|measured|reported}` from the receiver: its own output latency.
- Calibration: `cal.chirp {at_us, signal: "odp-chirp-1", gain}` asks a receiver to play a known 50 ms
  logarithmic sweep at a clock time; the device with the microphone reports `cal.result {receiver, arrival_us,
  level_db}`. The host computes each receiver's latency and proposes rooms (similar levels).
- `sync.output {device, kind: hdmi|speaker|bluetooth|airplay|headphones, reported_us}` from a receiver whenever its audio
  output changes; a host discards that receiver's measured latency on a change of `device`.
- `sync.offset {us}` from the host: a manual picture/sound offset for receivers that can't be measured.
- Receivers resample audio by up to ±0.1 % to follow the host clock; video frames are presented on time and dropped or
  repeated only when more than half a frame off.

### L6 Input (receiver → host)
- Keyboard: USB HID usage codes (page 0x07) with modifiers, not platform key codes. Key down/up go on L1 (reliable,
  ordered) so a key is never stuck.
- Pointer: absolute position normalised to the receiver's screen (0..65535 each axis), buttons, wheel (1/120 notches),
  on UDP with a sequence filter.
- Touch: contacts `{id, x, y, pressure?, state: down|move|up}`, up to `max_contacts`, on UDP; a lost `up` is repaired
  by the receiver resending the full contact set every 100 ms while any contact is down.
- The host maps these onto its own input pipeline (Omninput: the existing injectors; touch becomes pointer input where
  the OS has no touch injection).

### L7 Cinema (co-playback signalling)
- `cinema.open {service, url, title?, position_ms, rate, state: playing|paused, at_us}`: the receiver opens the URL in its
  DRM-capable view, seeks and starts at the clock time.
- `cinema.control {state, position_ms, rate, at_us}`: play, pause, seek, rate, all scheduled on the shared clock.
- `cinema.status {position_ms, at_us, state, error?}` from each receiver once a second and on change; errors include
  `signin_required`, `stream_limit`, `unavailable`, `drm_unsupported`.
- `cinema.close`: return to the previous mode.
- Sign-in stays on the receiver; **no message carries credentials, cookies or tokens of a third-party service.**

## Conformance (for community receivers)
- A receiver is **ODP 0.1 Basic** if it does L0–L4 with H.264 and L5 clock pings; **Synced** if it adds L5 latency
  reports and calibration; **Interactive** if it adds L6; **Cinema** if it adds L7.
- The reference receiver and a conformance host (test streams, wrong-PIN and replay tests, clock-offset checks) are
  published with version 1.0.

## Open questions before 1.0
- Whether multichannel audio and HDR metadata belong in 1.0 or 1.1.
