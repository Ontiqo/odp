//! The Open Display Protocol (ODP), version 0.1 draft: what a screen receiver and a host need to pair, set up an
//! encrypted session, keep a shared clock, and carry media and input. The protocol is described in `SPEC.md`; this
//! crate is its reference implementation, without any platform's capture, codecs or windows.
//!
//! - [`keys`]: long-term identities and fingerprints (L2)
//! - [`pair`]: PIN pairing (L2)
//! - [`session`]: session keys and sealing per channel (L2)
//! - [`clock`]: the shared clock (L5)
//! - [`audio`]: sound frames, PCM or Opus (L4)
//! - [`media`]: media datagrams, fragmentation and reassembly (L4)
//! - [`control`]: the control channel's framing and messages (L1, L3, L5, L7)
//! - [`input`]: pointer and touch from a receiver (L6)
//! - [`link`]: the handshake over a byte stream, pairing included, and the encrypted control link (L1, L2)

pub mod audio;
pub mod clock;
pub mod control;
pub mod input;
pub mod keys;
pub mod link;
pub mod media;
pub mod pair;
pub mod session;

/// The protocol version this crate speaks.
pub const VERSION: &str = "odp/1.5";

/// The versions sent in `hello`, newest first: 1.1 adds game controllers and rooms, 1.2 the monitor's inputs, 1.3 the
/// answer to switching (all optional); 1.0 is 0.1 frozen, the same on
/// the wire, so all are listed for receivers and hosts that still say an older one. (The key-derivation labels keep
/// their literal "odp/0.1" text.)
pub const VERSIONS: [&str; 6] = [VERSION, "odp/1.3", "odp/1.2", "odp/1.1", "odp/1.0", "odp/0.1"];
