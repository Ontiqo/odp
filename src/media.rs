//! L4 media: frames (an encoded video access unit, or an audio block) split into UDP datagrams of at most
//! [`MAX_DATAGRAM`] bytes. Each starts with a 24-byte header, sent as it is but authenticated (it is the seal's
//! associated data), followed by the sealed fragment:
//!
//! `version(1) kind(1) stream(1) flags(1) seq(4) pts_us(8) frame(4) frag_index(2) frag_count(2)`, big-endian.

use std::collections::HashMap;

use crate::session::{Opener, Sealer};

pub const VERSION: u8 = 1;
pub const HEADER: usize = 24;
/// Fits common links' MTU over IPv4 and IPv6, with room for UDP and IP headers.
pub const MAX_DATAGRAM: usize = 1200;
/// What a sealed fragment adds: the 8-byte counter and the 16-byte tag.
pub const SEAL_OVERHEAD: usize = 24;
pub const MAX_FRAGMENT: usize = MAX_DATAGRAM - HEADER - SEAL_OVERHEAD;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Video = 1,
    Audio = 2,
}

pub mod flags {
    /// A frame a decoder can start from (an IDR picture).
    pub const KEY: u8 = 1;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub stream: u8,
    pub flags: u8,
    pub seq: u32,
    /// When it should be seen or heard, on the shared clock (microseconds).
    pub pts_us: i64,
    pub frame: u32,
    pub frag_index: u16,
    pub frag_count: u16,
}

impl Header {
    pub fn to_bytes(&self) -> [u8; HEADER] {
        let mut b = [0u8; HEADER];
        b[0] = VERSION;
        b[1] = self.kind as u8;
        b[2] = self.stream;
        b[3] = self.flags;
        b[4..8].copy_from_slice(&self.seq.to_be_bytes());
        b[8..16].copy_from_slice(&self.pts_us.to_be_bytes());
        b[16..20].copy_from_slice(&self.frame.to_be_bytes());
        b[20..22].copy_from_slice(&self.frag_index.to_be_bytes());
        b[22..24].copy_from_slice(&self.frag_count.to_be_bytes());
        b
    }

    pub fn parse(b: &[u8]) -> Option<Header> {
        if b.len() < HEADER || b[0] != VERSION {
            return None;
        }
        let kind = match b[1] {
            1 => Kind::Video,
            2 => Kind::Audio,
            _ => return None,
        };
        let h = Header {
            kind,
            stream: b[2],
            flags: b[3],
            seq: u32::from_be_bytes(b[4..8].try_into().unwrap()),
            pts_us: i64::from_be_bytes(b[8..16].try_into().unwrap()),
            frame: u32::from_be_bytes(b[16..20].try_into().unwrap()),
            frag_index: u16::from_be_bytes(b[20..22].try_into().unwrap()),
            frag_count: u16::from_be_bytes(b[22..24].try_into().unwrap()),
        };
        (h.frag_count > 0 && h.frag_index < h.frag_count).then_some(h)
    }
}

/// Splits frames into sealed datagrams for one stream.
pub struct Packetizer {
    kind: Kind,
    stream: u8,
    seq: u32,
    frame: u32,
}

impl Packetizer {
    pub fn new(kind: Kind, stream: u8) -> Packetizer {
        Packetizer { kind, stream, seq: 0, frame: 0 }
    }

    /// The datagrams for one frame, sealed with `sealer`.
    pub fn frame(&mut self, data: &[u8], pts_us: i64, key: bool, sealer: &mut Sealer) -> Vec<Vec<u8>> {
        let chunks: Vec<&[u8]> = if data.is_empty() { vec![&[][..]] } else { data.chunks(MAX_FRAGMENT).collect() };
        let count = chunks.len().min(u16::MAX as usize) as u16;
        let frame = self.frame;
        self.frame = self.frame.wrapping_add(1);
        chunks
            .into_iter()
            .take(count as usize)
            .enumerate()
            .map(|(i, chunk)| {
                let h = Header { kind: self.kind, stream: self.stream, flags: if key { flags::KEY } else { 0 }, seq: self.seq, pts_us, frame, frag_index: i as u16, frag_count: count };
                self.seq = self.seq.wrapping_add(1);
                let header = h.to_bytes();
                let mut d = header.to_vec();
                d.extend_from_slice(&sealer.seal(&header, chunk));
                d
            })
            .collect()
    }
}

/// A whole frame back together.
#[derive(Debug, PartialEq)]
pub struct Frame {
    pub kind: Kind,
    pub stream: u8,
    pub frame: u32,
    pub pts_us: i64,
    pub key: bool,
    pub data: Vec<u8>,
}

/// Puts fragments back into frames. A frame still missing pieces when a newer frame of its stream completes is
/// dropped (the decoder asks for a key frame or waits for the next one).
#[derive(Default)]
pub struct Reassembler {
    pending: HashMap<(u8, u32), Pending>,
    /// Frames completed or given up, per stream: older fragments are ignored.
    last_done: HashMap<u8, u32>,
    pub dropped: u64,
}

struct Pending {
    header: Header,
    parts: Vec<Option<Vec<u8>>>,
    have: usize,
}

impl Reassembler {
    /// Takes one datagram; returns the frame it completes. Datagrams that don't open or parse are ignored.
    pub fn push(&mut self, datagram: &[u8], opener: &mut Opener) -> Option<Frame> {
        let h = Header::parse(datagram)?;
        if self.last_done.get(&h.stream).is_some_and(|&d| (h.frame.wrapping_sub(d) as i32) <= 0) {
            return None;
        }
        let part = opener.open(&datagram[..HEADER], &datagram[HEADER..])?;
        let p = self.pending.entry((h.stream, h.frame)).or_insert_with(|| Pending { header: h, parts: vec![None; h.frag_count as usize], have: 0 });
        if p.header.frag_count != h.frag_count {
            return None;
        }
        let slot = &mut p.parts[h.frag_index as usize];
        if slot.is_none() {
            *slot = Some(part);
            p.have += 1;
        }
        if p.have < p.parts.len() {
            return None;
        }
        let p = self.pending.remove(&(h.stream, h.frame)).unwrap();
        // Everything older on this stream is out of date now.
        let before = self.pending.len();
        self.pending.retain(|(s, f), _| *s != h.stream || (f.wrapping_sub(h.frame) as i32) > 0);
        self.dropped += (before - self.pending.len()) as u64;
        self.last_done.insert(h.stream, h.frame);
        Some(Frame {
            kind: p.header.kind,
            stream: h.stream,
            frame: h.frame,
            pts_us: p.header.pts_us,
            key: p.header.flags & flags::KEY != 0,
            data: p.parts.into_iter().flatten().flatten().collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::Identity;
    use crate::session::{Channel, Handshake, Role, Session};

    fn sessions() -> (Session, Session) {
        let (hi, ri) = (Identity::generate(), Identity::generate());
        let (h, r) = (Handshake::start(Role::Host, &hi), Handshake::start(Role::Receiver, &ri));
        let (ho, ro) = (h.offer(), r.offer());
        let (hs, rs) = (h.sign(&hi, &ro), r.sign(&ri, &ho));
        (h.finish(&ro, &rs, None).unwrap(), r.finish(&ho, &hs, None).unwrap())
    }

    #[test]
    fn headers_round_trip() {
        let h = Header { kind: Kind::Video, stream: 0, flags: flags::KEY, seq: 7, pts_us: 1_234_567, frame: 3, frag_index: 1, frag_count: 4 };
        assert_eq!(Header::parse(&h.to_bytes()), Some(h));
        let mut bad = h.to_bytes();
        bad[0] = 9;
        assert_eq!(Header::parse(&bad), None);
    }

    #[test]
    fn a_frame_crosses_in_fragments_in_any_order_and_fits_the_datagram_size() {
        let (mut host, mut receiver) = sessions();
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 253) as u8).collect();
        let mut p = Packetizer::new(Kind::Video, 0);
        let mut grams = p.frame(&data, 42_000, true, host.sealer(Channel::Media));
        assert!(grams.len() > 1 && grams.iter().all(|g| g.len() <= MAX_DATAGRAM));
        grams.reverse();
        let mut r = Reassembler::default();
        let mut out = None;
        for g in &grams {
            out = r.push(g, receiver.opener(Channel::Media)).or(out);
        }
        let f = out.unwrap();
        assert_eq!((f.data, f.pts_us, f.key), (data, 42_000, true));
    }

    #[test]
    fn an_incomplete_older_frame_is_dropped_when_a_newer_one_completes() {
        let (mut host, mut receiver) = sessions();
        let mut p = Packetizer::new(Kind::Video, 0);
        let old = p.frame(&vec![1u8; 3000], 0, false, host.sealer(Channel::Media));
        let new = p.frame(&vec![2u8; 100], 33_000, false, host.sealer(Channel::Media));
        let mut r = Reassembler::default();
        assert!(r.push(&old[0], receiver.opener(Channel::Media)).is_none());
        assert_eq!(r.push(&new[0], receiver.opener(Channel::Media)).unwrap().data, vec![2u8; 100]);
        assert_eq!(r.dropped, 1);
        assert!(r.push(&old[1], receiver.opener(Channel::Media)).is_none(), "too late now");
    }

    #[test]
    fn a_changed_header_or_payload_is_refused() {
        let (mut host, mut receiver) = sessions();
        let mut g = Packetizer::new(Kind::Audio, 1).frame(b"opus", 5, false, host.sealer(Channel::Media)).remove(0);
        g[10] ^= 1; // the presentation time
        assert!(Reassembler::default().push(&g, receiver.opener(Channel::Media)).is_none());
    }
}
