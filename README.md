# ODP: the Open Display Protocol

A protocol for showing a computer's screen, sound and films on other screens on the same network (TVs, Raspberry Pis,
old PCs, tablets), with the keyboard, mouse and touch of those screens sent back. Anyone may build a receiver or a
host from it.

- [SPEC.md](SPEC.md): the protocol, version 0.1 draft (CC BY 4.0).
- `src/`: the reference implementation in Rust (Apache-2.0): identities and fingerprints, PIN pairing, encrypted
  sessions, the shared clock, and media packets. It has no capture, codecs or windows of its own: a receiver adds its
  platform's decoder and display, a host its capture and encoder.
- `receiver/`: the reference receiver (Apache-2.0): it listens on 54330-54332, pairs with the PIN it shows, runs a
  session, keeps the shared clock, and hands over each frame with the moment to show it; it sends the screen's keys
  and pointer back and passes Cinema on. A screen app adds only its decoder and window. Rust apps use it directly;
  others through its C interface (`ffi`), and Android through its Java interface (feature `android`,
  `app.omninput.screen.Odp`).
- `conformance/`: `odp-check`, a conformance host (Apache-2.0). Show a PIN on a receiver, then
  `cargo run --release -p odp-check -- <address:port> <pin> [--key 10]`: it pairs (after trying a wrong PIN), comes
  back as a paired host, casts 30 frames of H.264 colour bars in a loop, loses a frame, replays a packet, answers
  clock pings and asks for Cinema, then prints each check and the levels the receiver meets (Basic, Synced,
  Interactive, Cinema).

```
cargo test
```

## Status

Version 0.1 is a draft and will change before 1.0. A conformance host for testing receivers comes with 1.0.

## Licence

Code: Apache License 2.0 ([LICENSE](LICENSE)). Specification text: Creative Commons Attribution 4.0.
