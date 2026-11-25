# ODP: the Open Display Protocol

A protocol for showing a computer's screen, sound and films on other screens on the same network (TVs, Raspberry Pis,
old PCs, tablets), with the keyboard, mouse and touch of those screens sent back. Anyone may build a receiver or a
host from it.

- [SPEC.md](SPEC.md): the protocol, version 0.1 draft (CC BY 4.0).
- `src/`: the reference implementation in Rust (Apache-2.0): identities and fingerprints, PIN pairing, encrypted
  sessions, the shared clock, and media packets. It has no capture, codecs or windows of its own: a receiver adds its
  platform's decoder and display, a host its capture and encoder.

```
cargo test
```

## Status

Version 0.1 is a draft and will change before 1.0. A conformance host for testing receivers comes with 1.0.

## Licence

Code: Apache License 2.0 ([LICENSE](LICENSE)). Specification text: Creative Commons Attribution 4.0.
