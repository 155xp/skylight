//! Frame pacing and adaptive bitrate over a reliable, ordered transport.
//!
//! The browser acknowledges every frame as soon as it is decoded. That gives one round-trip
//! sample per frame (network plus decode) without any clock sync. Two rules come out of it:
//!
//! * **Pacing.** A frame may only be encoded while the link is keeping up: the oldest
//!   unacknowledged frame is younger than the best recent round trip plus a small allowance, and
//!   no more frames are in flight than that round trip holds. Otherwise the encoder skips the frame
//!   *before* encoding it (so no reference frame is ever lost) and takes the newest image once
//!   acks catch up; damage accumulates meanwhile, so nothing is lost. On TCP this is what keeps a
//!   slow link from building seconds of buffered video.
//! * **Bitrate.** A delay-based controller: when queueing delay appears (even the best round trip
//!   of the last half second sits above the long-term minimum, or frames are stuck in flight), the
//!   target drops below the rate the browser actually acknowledged, and that rate is remembered as
//!   the path's ceiling for a while. A higher round trip counts as a queue only if this stream can
//!   be what fills it: it sends at least a third of its target, or the path delivers clearly less
//!   than it sends. A tunnel whose latency drifts by tens of milliseconds for seconds at a time, under
//!   a stream that uses little of its target, is not slowed down by cutting that target. While
//!   the link is clear it climbs: fast at first, carefully near the ceiling, then probing gently
//!   above it. It only climbs while the content uses the bitrate it has; a still screen says
//!   nothing about the path.

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
    sent: VecDeque<(u64, u32)>,
    pub srtt_us: f64,
    pub min_rtt_us: u64,
    pub target_bps: u64,
    pub max_bps: u64,
    pub min_bps: u64,
    pub frame_interval_us: u64,
    last_adjust_us: u64,
    last_cut_us: u64,
    /// The acknowledged rate at the last congestion, and until when it is trusted.
    ceiling: Option<(u64, u64)>,
    slow_start: bool,
    blocked_since_us: Option<u64>,
    /// When the last ack arrived (or the first frame went out, before any).
    last_ack_us: Option<u64>,
    pub skipped: u64,
    pub stalls: u64,
}

const RTT_WINDOW_US: u64 = 10_000_000;
/// Slack on top of the base round trip before the oldest frame in flight counts as stuck. A public
/// tunnel's round trips spread over 50 ms or more without any queue (its p99 sits about that far
/// above its best); a real queue is caught by the bitrate controller instead.
const ALLOWANCE_US: u64 = 60_000;
/// Queueing delay is judged from the best round trip over this window.
const QUEUE_WINDOW_US: u64 = 500_000;
const RATE_WINDOW_US: u64 = 1_000_000;
const ADJUST_EVERY_US: u64 = 200_000;
const CUT_EVERY_US: u64 = 300_000;
const CEILING_US: u64 = 15_000_000;
/// No ack at all for this long, with frames in flight, means the viewer stopped decoding (a hidden
/// tab, a decoder reset); stop waiting for it. On a slow path (a VPN exit, a relay) acks still
/// come, only late, and the limit grows with the round trip: a keyframe sent into a path that is
/// merely slow would only add to the queue that delays the acks.
const STALL_US: u64 = 3_000_000;

impl Pacer {
    pub fn new(start_bps: u64, min_bps: u64, max_bps: u64, fps: u32) -> Pacer {
        Pacer {
            inflight: VecDeque::new(),
            rtt_samples: VecDeque::new(),
            acked: VecDeque::new(),
            sent: VecDeque::new(),
            srtt_us: 0.0,
            min_rtt_us: 0,
            target_bps: start_bps.clamp(min_bps, max_bps),
            max_bps,
            min_bps,
            frame_interval_us: 1_000_000 / fps.max(1) as u64,
            last_adjust_us: 0,
            last_cut_us: 0,
            ceiling: None,
            slow_start: true,
            blocked_since_us: None,
            last_ack_us: None,
            skipped: 0,
            stalls: 0,
        }
    }

    pub fn inflight(&self) -> usize {
        self.inflight.len()
    }

    pub fn on_send(&mut self, id: u32, bytes: usize, now_us: u64) {
        if self.inflight.is_empty() {
            self.last_ack_us = Some(now_us);
        }
        self.inflight.push_back(Sent {
            id,
            bytes: bytes as u32,
            at_us: now_us,
        });
        self.sent.push_back((now_us, bytes as u32));
        trim(&mut self.sent, now_us, RATE_WINDOW_US);
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
            self.last_ack_us = Some(now_us);
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
            trim(&mut self.rtt_samples, now_us, RTT_WINDOW_US);
            self.min_rtt_us = self.rtt_samples.iter().map(|(_, r)| *r).min().unwrap_or(r);
        }
        trim(&mut self.acked, now_us, RATE_WINDOW_US);
        rtt
    }

    /// The viewer dropped whatever it had (a decoder reset); nothing in flight will be acked.
    pub fn forget_inflight(&mut self) {
        self.inflight.clear();
    }

    fn allowance_us(&self) -> u64 {
        let base = if self.rtt_samples.is_empty() {
            50_000
        } else {
            self.min_rtt_us
        };
        // A relay or Wi-Fi hop jitters by a good part of its round trip without any queue, so the
        // slack grows with the base round trip.
        base + (2 * self.frame_interval_us).max(ALLOWANCE_US).max(base)
    }

    /// The smallest round trip in the last `QUEUE_WINDOW_US`: jitter only adds to single
    /// samples, so this rises only when a queue has built up and every frame waits in it.
    fn recent_min_rtt_us(&self, now_us: u64) -> Option<u64> {
        self.rtt_samples
            .iter()
            .rev()
            .take_while(|(t, _)| t + QUEUE_WINDOW_US >= now_us)
            .map(|(_, r)| *r)
            .min()
    }

    /// How many frames may be in flight: as many as are sent within the allowance, plus slack.
    fn window(&self) -> usize {
        let frames = self.allowance_us() / self.frame_interval_us.max(1) + 2;
        frames.clamp(3, 40) as usize
    }

    /// May the next frame be encoded now?
    pub fn can_send(&mut self, now_us: u64) -> bool {
        let ok = match self.inflight.front() {
            None => true,
            Some(oldest) => {
                self.inflight.len() < self.window()
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

    /// True (once) when the viewer has stopped acknowledging altogether. The window is cleared so
    /// the stream can resume; the caller should send a keyframe, since frames were lost.
    pub fn stalled(&mut self, now_us: u64) -> bool {
        let limit = STALL_US.max(4 * self.srtt_us as u64).min(10_000_000);
        let stuck = !self.inflight.is_empty()
            && self.last_ack_us.is_some_and(|t| now_us.saturating_sub(t) > limit);
        if stuck {
            self.inflight.clear();
            self.blocked_since_us = None;
            self.stalls += 1;
        }
        stuck
    }

    /// When the oldest frame in flight will exceed the allowance (to time the next check).
    pub fn wait_hint_us(&self, now_us: u64) -> Option<u64> {
        let oldest = self.inflight.front()?;
        Some((oldest.at_us + self.allowance_us()).saturating_sub(now_us))
    }

    /// Bits per second acknowledged over the last second.
    pub fn delivery_bps(&self, now_us: u64) -> u64 {
        rate(&self.acked, now_us)
    }

    /// Bits per second the encoder produced over the last second.
    pub fn sent_bps(&self, now_us: u64) -> u64 {
        rate(&self.sent, now_us)
    }

    /// Move the bitrate target; returns the new target when it changed.
    pub fn adjust(&mut self, now_us: u64) -> Option<u64> {
        if now_us < self.last_adjust_us + ADJUST_EVERY_US {
            return None;
        }
        let since = now_us - self.last_adjust_us;
        self.last_adjust_us = now_us;
        if self.ceiling.is_some_and(|(_, until)| now_us > until) {
            self.ceiling = None;
        }
        let queue_us = self
            .recent_min_rtt_us(now_us)
            .unwrap_or(self.srtt_us as u64)
            .saturating_sub(self.min_rtt_us);
        // Relays and Wi-Fi jitter by tens of ms without any real queue; scale the thresholds
        // with the base round trip so a 50 ms tunnel is not mistaken for congestion.
        let high = 25_000.max(self.min_rtt_us / 3);
        let low = 10_000.max(self.min_rtt_us / 6);
        let blocked = self
            .blocked_since_us
            .is_some_and(|t| now_us.saturating_sub(t) > 150_000);
        let (sent, delivered) = (self.sent_bps(now_us), self.delivery_bps(now_us));
        // Whether a queue could be this stream's own doing (see the module comment).
        let ours = sent * 3 >= self.target_bps || delivered * 10 < sent * 8;
        let old = self.target_bps;
        if blocked || (queue_us > high && ours) {
            if now_us >= self.last_cut_us + CUT_EVERY_US {
                self.last_cut_us = now_us;
                self.slow_start = false;
                let mut next = self.target_bps * 85 / 100;
                // A delivered rate well under the target may just be light content; only a
                // rate near the target says something about the path.
                if delivered >= self.target_bps / 2 {
                    next = next.min(delivered * 85 / 100);
                    self.ceiling = Some((delivered, now_us + CEILING_US));
                }
                self.target_bps = next.max(self.min_bps);
            }
        } else if queue_us < low && self.srtt_us > 0.0 && sent * 2 >= self.target_bps {
            let t = self.target_bps;
            self.target_bps = match self.ceiling {
                _ if self.slow_start => t * 3 / 2,
                Some((c, _)) if t >= c * 9 / 10 => t + t * 2 / 100 * since / 1_000_000,
                _ => t * 106 / 100 + 100_000,
            }
            .min(self.max_bps);
        }
        (self.target_bps != old).then_some(self.target_bps)
    }

    pub fn set_max(&mut self, max_bps: u64) {
        self.max_bps = max_bps.max(self.min_bps);
        self.target_bps = self.target_bps.min(self.max_bps);
    }
}

fn trim(q: &mut VecDeque<(u64, impl Copy)>, now_us: u64, window_us: u64) {
    while q.front().is_some_and(|(t, _)| t + window_us < now_us) {
        q.pop_front();
    }
}

fn rate(q: &VecDeque<(u64, u32)>, now_us: u64) -> u64 {
    let bytes: u64 = q
        .iter()
        .filter(|(t, _)| t + RATE_WINDOW_US >= now_us)
        .map(|(_, b)| *b as u64)
        .sum();
    bytes * 8
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
        // 4 ms + max(2 frames, 60 ms, rtt) = 64 ms allowance; at 70 ms the oldest frame is stuck.
        assert!(p.can_send(70_000));
        assert!(!p.can_send(80_000));
        assert_eq!(p.skipped, 1);
        p.on_ack(2, 81_000);
        assert!(p.can_send(81_000));
    }

    #[test]
    fn bitrate_drops_on_queueing_and_recovers() {
        let mut p = Pacer::new(20_000_000, 1_000_000, 40_000_000, 60);
        let mut t = 0;
        // Clear link, content that uses the bitrate (40 kB per 16 ms is 20 Mbit/s): it climbs.
        for id in 0..40 {
            p.on_send(id, 40_000, t);
            p.on_ack(id, t + 5_000);
            t += 16_000;
            p.adjust(t);
        }
        assert!(p.target_bps > 20_000_000);
        let high = p.target_bps;
        // Queueing: round trips grow to 200 ms and stay there (a standing queue, two seconds).
        for id in 40..165 {
            p.on_send(id, 40_000, t);
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
        // The rate that got through is remembered: recovery approaches it carefully.
        let ceiling = p.ceiling.expect("a ceiling after congestion").0;
        for id in 165..285 {
            p.on_send(id, 40_000, t);
            p.on_ack(id, t + 5_000);
            t += 16_000;
            p.adjust(t);
        }
        assert!(
            p.target_bps <= ceiling * 11 / 10,
            "{} vs ceiling {ceiling}",
            p.target_bps
        );
    }

    #[test]
    fn a_relay_that_jitters_is_not_mistaken_for_a_queue() {
        // A public tunnel: 40 ms at best, most frames 50-70 ms, one in five up to 100 ms, and
        // content that uses what it gets. The target must climb, not collapse.
        let mut p = Pacer::new(2_000_000, 500_000, 50_000_000, 60);
        let mut t = 0;
        let mut x = 12345u32;
        for id in 0..600 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let jitter = if x.is_multiple_of(5) {
                30_000 + x as u64 % 30_000
            } else {
                x as u64 % 30_000
            };
            let bytes = (p.target_bps / 8 / 60) as usize;
            p.on_send(id, bytes, t);
            assert!(p.can_send(t + 1) || p.inflight() > 0);
            p.on_ack(id, t + 40_000 + jitter);
            t += 16_667;
            p.adjust(t);
        }
        assert!(p.target_bps > 8_000_000, "target fell to {}", p.target_bps);
    }

    #[test]
    fn a_tunnel_whose_latency_drifts_keeps_the_target_of_light_content() {
        // The round trip swings between 45 and 80 ms over four seconds, nothing queues, and the
        // stream uses 1.5 Mbit/s of a 16 Mbit/s target: the target must not collapse.
        let mut p = Pacer::new(16_000_000, 500_000, 50_000_000, 60);
        let mut t = 0;
        for id in 0..1200u32 {
            let phase = (t % 4_000_000) as f64 / 4_000_000.0 * std::f64::consts::TAU;
            let rtt = 45_000 + (17_500.0 * (1.0 - phase.cos())) as u64;
            p.on_send(id, 3_100, t);
            p.on_ack(id, t + rtt);
            t += 16_667;
            p.adjust(t);
        }
        assert_eq!(p.target_bps, 16_000_000);
    }

    #[test]
    fn a_light_stream_on_a_link_slower_than_it_still_backs_off() {
        // 3 Mbit/s of content into a 2 Mbit/s path: acks fall behind and round trips grow.
        let mut p = Pacer::new(16_000_000, 500_000, 50_000_000, 60);
        let mut t = 0u64;
        let mut link_free = 0u64;
        for id in 0..600u32 {
            let bytes = 6_250; // 3 Mbit/s at 60 fps
            p.on_send(id, bytes, t);
            // The bottleneck delivers 2 Mbit/s (250 bytes per ms) behind a 20 ms base trip.
            link_free = link_free.max(t) + bytes as u64 * 1000 / 250;
            p.on_ack(id, link_free + 20_000);
            t += 16_667;
            p.adjust(t);
        }
        assert!(
            p.target_bps < 4_000_000,
            "target {} should follow the path down",
            p.target_bps
        );
    }

    #[test]
    fn light_content_does_not_raise_the_target() {
        // A blinking cursor says nothing about what the path could carry.
        let mut p = Pacer::new(8_000_000, 1_000_000, 50_000_000, 60);
        let mut t = 0;
        for id in 0..300 {
            p.on_send(id, 2_000, t);
            p.on_ack(id, t + 5_000);
            t += 16_000;
            p.adjust(t);
        }
        assert_eq!(p.target_bps, 8_000_000);
    }

    #[test]
    fn window_holds_the_allowance_worth_of_frames() {
        let mut p = Pacer::new(10_000_000, 1_000_000, 50_000_000, 100);
        p.on_send(0, 1000, 0);
        p.on_ack(0, 2_000); // 2 ms round trip: (2 + 60) / 10 + 2 = 8 frames
        for id in 1..=8 {
            assert!(p.can_send(id as u64 * 1_000), "frame {id}");
            p.on_send(id, 1000, id as u64 * 1_000);
        }
        assert!(!p.can_send(9_000));
    }

    #[test]
    fn a_viewer_that_stops_acking_is_not_waited_for_forever() {
        let mut p = Pacer::new(10_000_000, 1_000_000, 50_000_000, 60);
        p.on_send(1, 1000, 0);
        assert!(!p.stalled(1_000_000));
        assert!(!p.can_send(1_000_000));
        assert!(p.stalled(STALL_US + 1));
        assert!(p.can_send(STALL_US + 2));
    }

    #[test]
    fn a_slow_path_that_still_acks_is_not_a_stall() {
        // A VPN exit behind a relay: each frame takes 3.5 s to come back, but acks keep arriving.
        let mut p = Pacer::new(2_000_000, 500_000, 50_000_000, 60);
        for id in 0..20u32 {
            p.on_send(id, 20_000, id as u64 * 500_000);
        }
        for id in 0..12u32 {
            let now = 3_500_000 + id as u64 * 500_000;
            p.on_ack(id, now);
            assert!(!p.stalled(now + 400_000), "frame {id}");
        }
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
