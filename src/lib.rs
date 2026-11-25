//! The Open Display Protocol (ODP), version 0.1 draft: what a screen receiver and a host need to pair, set up an
//! encrypted session, keep a shared clock, and carry media and input. The protocol is described in `SPEC.md`; this
//! crate is its reference implementation, without any platform's capture, codecs or windows.
//!
//! - [`keys`]: long-term identities and fingerprints (L2)
//! - [`pair`]: PIN pairing (L2)
//! - [`session`]: session keys and sealing per channel (L2)
//! - [`clock`]: the shared clock (L5)
//! - [`media`]: media datagrams, fragmentation and reassembly (L4)

pub mod clock;
pub mod keys;
pub mod media;
pub mod pair;
pub mod session;

/// The protocol version this crate speaks.
pub const VERSION: &str = "odp/0.1";
