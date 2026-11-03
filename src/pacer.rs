//! Frame pacing and adaptive bitrate over a reliable, ordered transport.
//!
//! The browser acknowledges every frame as soon as it is decoded. That gives one round-trip
//! sample per frame (network plus decode) without any clock sync. Two rules come out of it:
//!
//! * **Pacing.** If the oldest unacknowledged frame is older than the best round trip seen
//!   recently plus a small allowance, the link is queueing. The encoder then skips frames
//!   *before* encoding them (so no reference frame is ever lost) until acks catch up. On TCP this
//!   is what keeps a slow link from building seconds of buffered video.
//! * **Bitrate.** Every 250 ms the target moves: down hard when queueing appears (to below the
//!   measured delivery rate), up gently while the link stays clear.

use std::collections::VecDeque;

struct Sent {
    id: u32,
    bytes: u32,
    at_us: u64,
}

pub struct Pacer {
    inflight: VecDeque<Sent>,
    rtt_samples: VecDeque<(u64, u64)>,
    acked: VecDeque<(u64, u32)>,
    pub srtt_us: f64,
    pub min_rtt_us: u64,
    pub target_bps: u64,
    pub max_bps: u64,
    pub min_bps: u64,
    pub frame_interval_us: u64,
    last_adjust_us: u64,
    blocked_since_us: Option<u64>,
    pub skipped: u64,
}

const RTT_WINDOW_US: u64 = 10_000_000;
const RATE_WINDOW_US: u64 = 1_000_000;
const ADJUST_EVERY_US: u64 = 250_000;
const MAX_INFLIGHT: usize = 90;

impl Pacer {
    pub fn new(start_bps: u64, min_bps: u64, max_bps: u64, fps: u32) -> Pacer {
        Pacer {
            inflight: VecDeque::new(),
            rtt_samples: VecDeque::new(),
            acked: VecDeque::new(),
            srtt_us: 0.0,
            min_rtt_us: 0,
            target_bps: start_bps.clamp(min_bps, max_bps),
            max_bps,
            min_bps,
            frame_interval_us: 1_000_000 / fps.max(1) as u64,
            last_adjust_us: 0,
            blocked_since_us: None,
            skipped: 0,
        }
    }

    pub fn inflight(&self) -> usize {
        self.inflight.len()
    }

    pub fn on_send(&mut self, id: u32, bytes: usize, now_us: u64) {
        self.inflight.push_back(Sent {
            id,
            bytes: bytes as u32,
            at_us: now_us,
        });
    }

    /// Returns the round trip for `id`, if it was in flight.
    pub fn on_ack(&mut self, id: u32, now_us: u64) -> Option<u64> {
        // Frames are acked in order; anything older than `id` was acked implicitly.
        let mut rtt = None;
        while let Some(f) = self.inflight.front() {
            if f.id.wrapping_sub(id) as i32 > 0 {
                break;
            }
            let f = self.inflight.pop_front().unwrap();
            self.acked.push_back((now_us, f.bytes));
            if f.id == id {
                rtt = Some(now_us.saturating_sub(f.at_us));
            }
        }
        if let Some(r) = rtt {
            self.srtt_us = if self.srtt_us == 0.0 {
                r as f64
            } else {
                self.srtt_us * 0.875 + r as f64 * 0.125
            };
            self.rtt_samples.push_back((now_us, r));
            while self
                .rtt_samples
                .front()
                .is_some_and(|(t, _)| t + RTT_WINDOW_US < now_us)
            {
                self.rtt_samples.pop_front();
            }
            self.min_rtt_us = self.rtt_samples.iter().map(|(_, r)| *r).min().unwrap_or(r);
        }
        while self
            .acked
            .front()
            .is_some_and(|(t, _)| t + RATE_WINDOW_US < now_us)
        {
            self.acked.pop_front();
        }
        rtt
    }

    fn allowance_us(&self) -> u64 {
        let base = if self.rtt_samples.is_empty() {
            50_000
        } else {
            self.min_rtt_us
        };
        base + (2 * self.frame_interval_us).max(25_000).max(base / 2)
    }

    /// May the next frame be encoded now?
    pub fn can_send(&mut self, now_us: u64) -> bool {
        let ok = match self.inflight.front() {
            None => true,
            Some(oldest) => {
                self.inflight.len() < MAX_INFLIGHT
                    && now_us.saturating_sub(oldest.at_us) <= self.allowance_us()
            }
        };
        if ok {
            self.blocked_since_us = None;
        } else if self.blocked_since_us.is_none() {
            self.blocked_since_us = Some(now_us);
            self.skipped += 1;
        }
        ok
    }

    /// When the oldest frame in flight will exceed the allowance (to time the next check).
    pub fn wait_hint_us(&self, now_us: u64) -> Option<u64> {
        let oldest = self.inflight.front()?;
        Some((oldest.at_us + self.allowance_us()).saturating_sub(now_us))
    }

    /// Bytes per second acknowledged over the last second.
    pub fn delivery_bps(&self, now_us: u64) -> u64 {
        let bytes: u64 = self
            .acked
            .iter()
            .filter(|(t, _)| t + RATE_WINDOW_US >= now_us)
            .map(|(_, b)| *b as u64)
            .sum();
        bytes * 8
    }

    /// Move the bitrate target; returns the new target when it changed.
    pub fn adjust(&mut self, now_us: u64) -> Option<u64> {
        if now_us < self.last_adjust_us + ADJUST_EVERY_US {
            return None;
        }
        self.last_adjust_us = now_us;
        let queue_us = (self.srtt_us as u64).saturating_sub(self.min_rtt_us);
        // Relays and Wi-Fi jitter by tens of ms without any real queue; scale the thresholds
        // with the base round trip so a 50 ms tunnel is not mistaken for congestion.
        let high = 40_000.max(self.min_rtt_us / 2);
        let low = 15_000.max(self.min_rtt_us / 4);
        let blocked = self
            .blocked_since_us
            .is_some_and(|t| now_us.saturating_sub(t) > 150_000);
        let old = self.target_bps;
        if blocked || queue_us > high {
            let delivered = self.delivery_bps(now_us);
            let mut next = self.target_bps * 3 / 4;
            if delivered > 0 {
                next = next.min(delivered * 85 / 100);
            }
            self.target_bps = next.max(self.min_bps);
        } else if queue_us < low && self.srtt_us > 0.0 {
            self.target_bps = (self.target_bps * 115 / 100).min(self.max_bps);
        }
        (self.target_bps != old).then_some(self.target_bps)
    }

    pub fn set_max(&mut self, max_bps: u64) {
        self.max_bps = max_bps.max(self.min_bps);
        self.target_bps = self.target_bps.min(self.max_bps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acks_measure_rtt_and_free_the_window() {
        let mut p = Pacer::new(10_000_000, 1_000_000, 50_000_000, 60);
        p.on_send(1, 1000, 0);
        p.on_send(2, 1000, 16_000);
        assert_eq!(p.on_ack(1, 5_000), Some(5_000));
        assert_eq!(p.inflight(), 1);
        // Acking 3 when 2 was never acked clears both.
        p.on_send(3, 1000, 32_000);
        assert_eq!(p.on_ack(3, 40_000), Some(8_000));
        assert_eq!(p.inflight(), 0);
        assert_eq!(p.min_rtt_us, 5_000);
    }

    #[test]
    fn blocks_when_frames_queue_up() {
        let mut p = Pacer::new(10_000_000, 1_000_000, 50_000_000, 60);
        p.on_send(1, 1000, 0);
        p.on_ack(1, 4_000); // 4 ms round trip
        p.on_send(2, 50_000, 10_000);
        assert!(p.can_send(20_000));
        // 4 ms + max(2 frames, 25 ms, rtt/2) = 37 ms allowance; at 60 ms the oldest frame is stuck.
        assert!(!p.can_send(70_000));
        assert_eq!(p.skipped, 1);
        p.on_ack(2, 71_000);
        assert!(p.can_send(71_000));
    }

    #[test]
    fn bitrate_drops_on_queueing_and_recovers() {
        let mut p = Pacer::new(20_000_000, 1_000_000, 40_000_000, 60);
        let mut t = 0;
        // Clear link: 5 ms round trips, target climbs.
        for id in 0..40 {
            p.on_send(id, 20_000, t);
            p.on_ack(id, t + 5_000);
            t += 16_000;
            p.adjust(t);
        }
        assert!(p.target_bps > 20_000_000);
        let high = p.target_bps;
        // Queueing: round trips grow to 200 ms.
        for id in 40..80 {
            p.on_send(id, 20_000, t);
            p.on_ack(id, t + 200_000);
            t += 16_000;
            p.adjust(t);
        }
        assert!(
            p.target_bps < high / 2,
            "target {} should have dropped from {}",
            p.target_bps,
            high
        );
        assert!(p.target_bps >= p.min_bps);
    }

    #[test]
    fn jittery_long_link_is_not_congestion() {
        // A 50 ms tunnel whose round trips wobble between 50 and 75 ms.
        let mut p = Pacer::new(10_000_000, 1_000_000, 40_000_000, 60);
        let mut t = 0;
        for id in 0..120 {
            p.on_send(id, 20_000, t);
            p.on_ack(id, t + 50_000 + (id as u64 % 6) * 5_000);
            t += 16_000;
            p.adjust(t);
        }
        assert!(
            p.target_bps >= 10_000_000,
            "target fell to {}",
            p.target_bps
        );
    }
}
