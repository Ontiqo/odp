//! L5 clock: the receiver pings the host about once a second (`clock.ping {t1}` / `clock.pong {t1, t2, t3}`, the
//! NTP four-timestamp exchange) and keeps the offset from the samples with the shortest round trip, which are the
//! ones least bent by queueing. Times are microseconds; each side uses its own monotonic clock.

/// One exchange: t1 the receiver sent, t2 the host received, t3 the host replied, t4 the receiver received.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub t1: i64,
    pub t2: i64,
    pub t3: i64,
    pub t4: i64,
}

impl Sample {
    /// Host clock minus receiver clock.
    pub fn offset(&self) -> i64 {
        ((self.t2 - self.t1) + (self.t3 - self.t4)) / 2
    }
    /// Time on the network, both ways (excluding the host's own turnaround).
    pub fn rtt(&self) -> i64 {
        (self.t4 - self.t1) - (self.t3 - self.t2)
    }
}

/// The receiver's estimate of the host's clock.
#[derive(Default)]
pub struct Clock {
    samples: std::collections::VecDeque<Sample>,
}

/// How many recent samples are kept.
const KEEP: usize = 32;
/// Of those, how many of the shortest round trips are averaged.
const BEST: usize = 4;

impl Clock {
    pub fn add(&mut self, s: Sample) {
        if s.rtt() < 0 {
            return; // impossible: a clock went backwards
        }
        self.samples.push_back(s);
        if self.samples.len() > KEEP {
            self.samples.pop_front();
        }
    }

    /// Host clock minus this clock, from the shortest round trips; None before the first sample.
    pub fn offset(&self) -> Option<i64> {
        let mut best: Vec<&Sample> = self.samples.iter().collect();
        if best.is_empty() {
            return None;
        }
        best.sort_by_key(|s| s.rtt());
        best.truncate(BEST);
        Some(best.iter().map(|s| s.offset()).sum::<i64>() / best.len() as i64)
    }

    /// The shortest round trip seen recently.
    pub fn rtt(&self) -> Option<i64> {
        self.samples.iter().map(Sample::rtt).min()
    }

    /// A host time (a presentation time) on this clock.
    pub fn to_local(&self, host_us: i64) -> Option<i64> {
        self.offset().map(|o| host_us - o)
    }

    /// Drift between the two clocks in parts per million, from the oldest and newest of the best samples; None until
    /// there are samples at least 10 s apart.
    pub fn drift_ppm(&self) -> Option<f64> {
        let first = self.samples.front()?;
        let last = self.samples.back()?;
        let span = (last.t4 - first.t4) as f64;
        if span < 10_000_000.0 {
            return None;
        }
        Some((last.offset() - first.offset()) as f64 / span * 1_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host clock `offset` ahead, with `out` and `back` microseconds on the wire.
    fn sample(at: i64, offset: i64, out: i64, back: i64) -> Sample {
        let t1 = at;
        let t2 = t1 + out + offset;
        let t3 = t2 + 50;
        let t4 = t3 - offset + back;
        Sample { t1, t2, t3, t4 }
    }

    #[test]
    fn a_symmetric_link_gives_the_exact_offset() {
        let s = sample(1_000, 5_000_000, 300, 300);
        assert_eq!((s.offset(), s.rtt()), (5_000_000, 600));
    }

    #[test]
    fn queueing_on_some_samples_doesnt_move_the_estimate() {
        let mut c = Clock::default();
        for i in 0..20 {
            // Most exchanges wait in a queue one way; a few get straight through.
            let (out, back) = if i % 5 == 0 { (200, 200) } else { (200 + 4_000, 200) };
            c.add(sample(i * 1_000_000, 7_000, out, back));
        }
        assert_eq!(c.offset(), Some(7_000));
        assert_eq!(c.rtt(), Some(400));
        assert_eq!(c.to_local(1_007_000), Some(1_000_000));
    }

    #[test]
    fn drift_shows_once_there_is_a_long_enough_span() {
        let mut c = Clock::default();
        c.add(sample(0, 1_000, 100, 100));
        assert_eq!(c.drift_ppm(), None);
        // 20 s later the host is 400 µs further ahead: 20 ppm.
        c.add(sample(20_000_000, 1_400, 100, 100));
        let d = c.drift_ppm().unwrap();
        assert!((d - 20.0).abs() < 0.5, "{d}");
    }
}
