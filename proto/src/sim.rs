//! Loopback simulation of the whole media path under packet loss, in virtual time (deterministic, runs in
//! milliseconds): sender (pacing, retransmit buffer, keyframe cooldown) -> lossy, jittery link -> receiver
//! (reassembly, NACK, keyframe policy, audio redundancy), and back (NACK / KEYREQ, also lossy).
//!
//! It runs the SAME logic twice:
//!  * `Mode::Old`: the v0.3.0 behaviour (kept in `legacy.rs`): any frame that cannot be completed right away is
//!    lost and the decoder asks for a whole keyframe, re-asking every 150 ms; audio has no redundancy.
//!  * `Mode::New`: v0.4.0: NACK of the missing packets only, keyframe requests with a cooldown, audio redundancy.
//!
//! and measures what the viewer would get: decodable frames per second, the longest freeze, keyframe requests,
//! and audio gaps. Run with `cargo test --release sim -- --nocapture` to see the table.

use crate::keyreq::KeyPolicy;
use crate::legacy::reassembly as old;
use crate::loss::Rng;
use crate::reassembly::{self as new, Config, Reassembler};
use crate::retx::{RetxBuffer, RetxLimiter};
use crate::*;
use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Old,
    New,
}

#[derive(Clone, Copy)]
enum LossModel {
    /// Independent losses.
    Random,
    /// Wi-Fi style: losses come in bursts (mean burst of ~3 packets), same average rate.
    Bursty,
}

#[derive(Clone)]
struct Params {
    mode: Mode,
    loss: f64,
    model: LossModel,
    fps: u32,
    secs: u32,
    /// Encoded sizes: ~15 Mbit/s at 30 fps with a keyframe every 2 s.
    p_bytes: usize,
    key_bytes: usize,
    one_way_ms: f64,
    jitter_ms: f64,
    seed: u64,
}

impl Params {
    fn new(mode: Mode, loss: f64, model: LossModel) -> Self {
        Params {
            mode,
            loss,
            model,
            fps: 30,
            secs: 30,
            p_bytes: 52_000,
            key_bytes: 240_000,
            one_way_ms: 8.0,
            jitter_ms: 4.0,
            seed: 7,
        }
    }
    fn target_bytes_per_s(&self) -> f64 {
        (self.p_bytes as f64 * (self.fps as f64 * 2.0 - 1.0) + self.key_bytes as f64) / 2.0
    }
}

enum Ev {
    FrameTick,
    AudioTick,
    TxSend(Vec<u8>),          // sender hands a video packet to the (lossy) link
    RxVideo(Vec<u8>),         // arrives at the receiver
    RxAudio(Vec<u8>),
    TxNack(Vec<(u32, u16)>),  // arrives at the sender
    TxKeyReq,
}

struct Event {
    t: u64, // microseconds
    n: u64,
    ev: Ev,
}
impl PartialEq for Event {
    fn eq(&self, o: &Self) -> bool {
        (self.t, self.n) == (o.t, o.n)
    }
}
impl Eq for Event {}
impl PartialOrd for Event {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Event {
    fn cmp(&self, o: &Self) -> Ordering {
        (self.t, self.n).cmp(&(o.t, o.n))
    }
}

#[derive(Default, Debug, Clone)]
struct Result {
    fps: f64,
    max_freeze_ms: u64,
    key_requests: u32,
    forced_keys: u32,
    frames_lost: u32,
    nacked_pkts: u32,
    retx_sent: u32,
    retx_throttled: u32,
    mbps: f64,
    audio_gap_events_per_s: f64,
    audio_gap_pct: f64,
    audio_max_ts_err_ms: f64,
    corrupt_frames: u32,
}

const SESSION: u32 = 0xABCD_0001;

fn frame_bytes(id: u32, len: usize) -> Vec<u8> {
    (0..len).map(|i| (id as usize).wrapping_mul(31).wrapping_add(i) as u8).collect()
}

struct Sim {
    p: Params,
    t_us: u64,
    n: u64,
    heap: BinaryHeap<Reverse<Event>>,
    rng: Rng,
    ge_bad: bool,
    // ---- sender
    frame_id: u32,
    first: bool,
    last_key_ms: u64,
    keyreq_pending: bool,
    tx_free_us: u64,
    retx: RetxBuffer,
    limiter: RetxLimiter,
    audio_seq: u32,
    audio_prev: Vec<u8>,
    // ---- receiver
    reasm_new: Reassembler,
    reasm_old: old::Reassembler,
    old_pending_since: Option<u64>,
    old_need_key: bool,
    old_last_req: u64,
    policy: KeyPolicy,
    awaiting_key: bool,
    arx: audiorx::AudioRx,
    chunks: Vec<(f64, f64)>, // (ts_ms, duration_ms) of audio handed to the player
    audio_ts_err: f64,
    // ---- measurements
    submitted: Vec<u64>, // ms of every frame handed to the decoder
    r: Result,
    bytes_sent: u64,
}

impl Sim {
    fn new(p: Params) -> Self {
        let mut s = Sim {
            rng: Rng::new(p.seed),
            ge_bad: false,
            t_us: 0,
            n: 0,
            heap: BinaryHeap::new(),
            frame_id: 0,
            first: true,
            last_key_ms: 0,
            keyreq_pending: false,
            tx_free_us: 0,
            retx: RetxBuffer::new(300),
            limiter: RetxLimiter::new(24.0 * 1300.0),
            audio_seq: 0,
            audio_prev: Vec::new(),
            reasm_new: Reassembler::with_config(Config::default()),
            reasm_old: old::Reassembler::new(),
            old_pending_since: None,
            old_need_key: true,
            old_last_req: 0,
            policy: KeyPolicy::standard(),
            awaiting_key: true,
            arx: audiorx::AudioRx::new(48000, 2),
            chunks: Vec::new(),
            audio_ts_err: 0.0,
            submitted: Vec::new(),
            r: Result::default(),
            bytes_sent: 0,
            p,
        };
        s.policy.need();
        s.push(0, Ev::FrameTick);
        s.push(0, Ev::AudioTick);
        s
    }

    fn push(&mut self, t: u64, ev: Ev) {
        self.n += 1;
        self.heap.push(Reverse(Event { t, n: self.n, ev }));
    }

    fn lost_down(&mut self) -> bool {
        match self.p.model {
            LossModel::Random => self.rng.f64() < self.p.loss,
            LossModel::Bursty => {
                let leave = 1.0 / 3.0;
                let enter = self.p.loss * leave / (1.0 - self.p.loss);
                self.ge_bad = if self.ge_bad { self.rng.f64() >= leave } else { self.rng.f64() < enter };
                self.ge_bad
            }
        }
    }

    fn delay_us(&mut self) -> u64 {
        ((self.p.one_way_ms + self.rng.f64() * self.p.jitter_ms) * 1000.0) as u64
    }

    fn run(mut self) -> Result {
        let end_us = self.p.secs as u64 * 1_000_000;
        let mut next_ms_tick = 0u64;
        while self.t_us < end_us {
            // events up to the next whole millisecond, then the receiver's 1 ms housekeeping
            let limit = (next_ms_tick + 1) * 1000;
            while let Some(Reverse(e)) = self.heap.peek() {
                if e.t >= limit {
                    break;
                }
                let Reverse(e) = self.heap.pop().unwrap();
                self.t_us = e.t;
                self.handle(e.ev);
            }
            self.t_us = limit;
            self.rx_tick(next_ms_tick + 1);
            next_ms_tick += 1;
        }
        self.finish()
    }

    // ------------------------------------------------------------------ sender
    fn handle(&mut self, ev: Ev) {
        let now_ms = self.t_us / 1000;
        match ev {
            Ev::FrameTick => {
                let period_us = 1_000_000 / self.p.fps as u64;
                self.push(self.t_us + period_us, Ev::FrameTick);
                // keyframe: first frame, GOP (2 s), or the receiver asked (rate-limited like capture_loop)
                let cooldown = if self.p.mode == Mode::New { 500 } else { 250 };
                let req = self.keyreq_pending && now_ms.saturating_sub(self.last_key_ms) > cooldown;
                let gop = now_ms.saturating_sub(self.last_key_ms) >= 2000;
                let key = self.first || gop || req;
                if req {
                    self.keyreq_pending = false;
                }
                if key {
                    self.last_key_ms = now_ms;
                    self.r.forced_keys += 1;
                }
                self.first = false;
                let len = if key { self.p.key_bytes } else { self.p.p_bytes };
                let data = frame_bytes(self.frame_id, len);
                let count = fragment_count(len);
                // paced at 1.5x the target rate, like video_sender
                let rate = self.p.target_bytes_per_s() * 1.5;
                for (i, chunk) in data.chunks(MAX_PAYLOAD).enumerate() {
                    let frag = VideoFrag {
                        ts_ms: now_ms as u32,
                        frame_id: self.frame_id,
                        idx: i as u16,
                        count: count as u16,
                        key,
                    };
                    let mut pkt = Vec::new();
                    write_video_header(SESSION, &frag, &mut pkt);
                    pkt.extend_from_slice(chunk);
                    let at = self.tx_free_us.max(self.t_us);
                    self.tx_free_us = at + (pkt.len() as f64 / rate * 1e6) as u64;
                    self.push(at, Ev::TxSend(pkt));
                }
                self.frame_id = self.frame_id.wrapping_add(1);
            }
            Ev::TxSend(pkt) => {
                if self.p.mode == Mode::New {
                    let (_, _, body) = parse_header(&pkt).unwrap();
                    let (f, _) = parse_video(body).unwrap();
                    self.retx.store(now_ms, f.frame_id, f.idx, f.count, &pkt);
                }
                self.send_down(pkt, true);
            }
            Ev::AudioTick => {
                let new_mode = self.p.mode == Mode::New;
                let chunk_us = if new_mode { 3000 } else { 5000 };
                self.push(self.t_us + chunk_us, Ev::AudioTick);
                let bytes = chunk_us as usize / 1000 * 48 * 4;
                let cur = vec![self.audio_seq as u8; bytes];
                let ts = now_ms as u32;
                let mut pkt = Vec::new();
                if new_mode {
                    write_audio_red(SESSION, self.audio_seq, ts, &self.audio_prev, &cur, &mut pkt);
                } else {
                    write_audio_header(SESSION, self.audio_seq, ts, &mut pkt);
                    pkt.extend_from_slice(&cur);
                }
                self.audio_prev = cur;
                self.audio_seq += 1;
                self.send_down(pkt, false);
            }
            Ev::TxNack(items) => {
                let rate = (self.p.target_bytes_per_s() * 0.3).max(125_000.0);
                let mut v = Vec::new();
                for item in items {
                    self.retx.collect(now_ms, item, 25, &mut v);
                }
                for pkt in v {
                    if !self.limiter.allow(now_ms, pkt.len(), rate) {
                        self.r.retx_throttled += 1;
                        continue;
                    }
                    self.r.retx_sent += 1;
                    self.send_down(pkt, true);
                }
            }
            Ev::TxKeyReq => self.keyreq_pending = true,
            Ev::RxVideo(pkt) => self.rx_video(now_ms, &pkt),
            Ev::RxAudio(pkt) => self.rx_audio(&pkt),
        }
    }

    fn send_down(&mut self, pkt: Vec<u8>, video: bool) {
        self.bytes_sent += pkt.len() as u64;
        if self.lost_down() {
            return;
        }
        let at = self.t_us + self.delay_us();
        self.push(at, if video { Ev::RxVideo(pkt) } else { Ev::RxAudio(pkt) });
    }

    fn send_up(&mut self, ev: Ev) {
        if self.rng.f64() < self.p.loss {
            return; // the way back is lossy too
        }
        let at = self.t_us + self.delay_us();
        self.push(at, ev);
    }

    // ---------------------------------------------------------------- receiver
    fn rx_video(&mut self, now_ms: u64, pkt: &[u8]) {
        let (_, _, body) = parse_header(pkt).unwrap();
        let (f, payload) = parse_video(body).unwrap();
        let mut out_new = Vec::new();
        let mut out_old = Vec::new();
        match self.p.mode {
            Mode::New => self.reasm_new.push(now_ms, &f, payload, &mut out_new),
            Mode::Old => {
                if self.old_pending_since.is_none() {
                    self.old_pending_since = Some(now_ms);
                }
                self.reasm_old.push(&f, payload, &mut out_old)
            }
        }
        self.handle_new_out(now_ms, out_new);
        self.handle_old_out(now_ms, out_old);
    }

    fn handle_new_out(&mut self, now_ms: u64, outs: Vec<new::Out>) {
        for o in outs {
            match o {
                new::Out::Frame(fr) => self.deliver(now_ms, fr.frame_id, fr.key, &fr.data),
                new::Out::Lost => {
                    self.awaiting_key = true;
                    self.policy.need();
                }
            }
        }
    }

    fn handle_old_out(&mut self, now_ms: u64, outs: Vec<old::Out>) {
        for o in outs {
            match o {
                old::Out::Frame(fr) => self.deliver(now_ms, fr.frame_id, fr.key, &fr.data),
                old::Out::Lost => {
                    self.awaiting_key = true;
                    self.old_need_key = true;
                }
            }
        }
    }

    /// What Engine.onFrame does: until a keyframe has come through, nothing can be decoded.
    fn deliver(&mut self, now_ms: u64, id: u32, key: bool, data: &[u8]) {
        let len = if key { self.p.key_bytes } else { self.p.p_bytes };
        if data != frame_bytes(id, len).as_slice() {
            self.r.corrupt_frames += 1;
        }
        if self.awaiting_key {
            if !key {
                return;
            }
            self.awaiting_key = false;
            self.old_need_key = false;
            self.policy.on_key();
        }
        self.submitted.push(now_ms);
    }

    fn rx_tick(&mut self, now_ms: u64) {
        match self.p.mode {
            Mode::New => {
                let mut nacks = Vec::new();
                let mut out = Vec::new();
                self.reasm_new.poll(now_ms, &mut nacks, &mut out);
                self.handle_new_out(now_ms, out);
                for c in nacks.chunks(MAX_NACK_ITEMS) {
                    let mut pkt = Vec::new();
                    write_nack(SESSION, c, &mut pkt);
                    // as the sender's control thread would see it
                    let (_, _, body) = parse_header(&pkt).unwrap();
                    let items = parse_nack(body);
                    self.send_up(Ev::TxNack(items));
                }
                let in_flight = self.reasm_new.key_pending();
                if self.policy.poll(now_ms, in_flight) {
                    self.r.key_requests += 1;
                    self.send_up(Ev::TxKeyReq);
                }
            }
            Mode::Old => {
                // Engine.housekeeping of v0.3: a frame waiting > 100 ms is given up; re-ask for a key every 150 ms.
                if self.reasm_old.has_pending() {
                    let since = *self.old_pending_since.get_or_insert(now_ms);
                    if now_ms.saturating_sub(since) > 100 {
                        let mut out = Vec::new();
                        self.reasm_old.give_up_oldest(&mut out);
                        self.old_pending_since = Some(now_ms);
                        self.handle_old_out(now_ms, out);
                    }
                } else {
                    self.old_pending_since = None;
                }
                if self.old_need_key && now_ms.saturating_sub(self.old_last_req) > 150 {
                    self.old_last_req = now_ms;
                    self.r.key_requests += 1;
                    self.send_up(Ev::TxKeyReq);
                }
            }
        }
    }

    fn rx_audio(&mut self, pkt: &[u8]) {
        let (ty, _, body) = parse_header(pkt).unwrap();
        let mut out = Vec::new();
        if ty == T_AUDIO_RED {
            let (seq, ts, prev, cur) = parse_audio_red(body).unwrap();
            self.arx.on_red(seq, ts, prev, cur, &mut out);
        } else {
            let (seq, ts, pcm) = parse_audio(body).unwrap();
            self.arx.on_plain(seq, ts, pcm, &mut out);
        }
        for c in out {
            let dur = (c.data.len() / 4) as f64 / 48.0;
            // every chunk (also those recovered from a redundant copy) must sit exactly on the capture grid
            let grid = dur;
            let off = c.ts_ms % grid;
            self.audio_ts_err = self.audio_ts_err.max(off.min(grid - off));
            self.chunks.push((c.ts_ms, dur));
        }
    }

    // ----------------------------------------------------------------- results
    fn finish(mut self) -> Result {
        let secs = self.p.secs as f64;
        self.r.fps = self.submitted.len() as f64 / secs;
        let mut prev = 0u64;
        for &t in &self.submitted {
            self.r.max_freeze_ms = self.r.max_freeze_ms.max(t - prev);
            prev = t;
        }
        self.r.max_freeze_ms = self.r.max_freeze_ms.max(self.p.secs as u64 * 1000 - prev);
        self.r.frames_lost = self.reasm_new.lost.max(self.reasm_old.lost);
        self.r.nacked_pkts = self.reasm_new.nacked_pkts;
        self.r.mbps = self.bytes_sent as f64 * 8.0 / secs / 1e6;
        // audio: chunks sorted by time; a hole between two chunks is what the player would have to conceal
        self.chunks.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let (mut events, mut gap_ms) = (0u32, 0.0);
        let mut end = self.chunks.first().map_or(0.0, |c| c.0);
        for &(ts, dur) in &self.chunks {
            if ts - end > 0.5 {
                events += 1;
                gap_ms += ts - end;
            }
            end = end.max(ts + dur);
        }
        // trailing hole up to the end of the run is not counted (the stream just stops there)
        let covered = end - self.chunks.first().map_or(0.0, |c| c.0);
        self.r.audio_gap_events_per_s = events as f64 / secs;
        self.r.audio_gap_pct = gap_ms / covered * 100.0;
        self.r.audio_max_ts_err_ms = self.audio_ts_err;
        self.r
    }
}

fn run(mode: Mode, loss: f64, model: LossModel) -> Result {
    Sim::new(Params::new(mode, loss, model)).run()
}

fn show(name: &str, r: &Result) {
    println!(
        "{name:<34} {:>5.1} к/с  макс.пауза {:>5} мс  KEYREQ {:>3}  ключ.кадров {:>3}  потеряно кадров {:>4}  NACK-пакетов {:>5}  \
         повторов {:>5} ({} отклонено)  {:>5.1} Мбит/с  звук: дыр {:>5.2}/с = {:>5.2}%",
        r.fps,
        r.max_freeze_ms,
        r.key_requests,
        r.forced_keys,
        r.frames_lost,
        r.nacked_pkts,
        r.retx_sent,
        r.retx_throttled,
        r.mbps,
        r.audio_gap_events_per_s,
        r.audio_gap_pct,
    );
}

#[test]
fn no_loss_nothing_extra_happens() {
    let r = run(Mode::New, 0.0, LossModel::Random);
    show("новая логика, 0% потерь", &r);
    assert!(r.fps > 29.0, "{r:?}");
    assert_eq!((r.nacked_pkts, r.retx_sent, r.frames_lost), (0, 0, 0));
    assert_eq!(r.key_requests, 1, "only the very first one");
    assert!(r.audio_gap_pct < 0.01 && r.corrupt_frames == 0, "{r:?}");
}

#[test]
fn old_logic_falls_into_the_keyframe_loop() {
    for loss in [0.03, 0.05] {
        let r = run(Mode::Old, loss, LossModel::Random);
        show(&format!("СТАРАЯ логика, {}% потерь", loss * 100.0), &r);
        // a 52 kB frame is ~44 packets: with 3% loss only ~26% of the frames arrive whole, and every hole means
        // "give me a 240 kB keyframe" (200 packets, of which ~99.8% are hit by at least one loss)
        assert!(r.fps < 15.0, "{loss}: {r:?}");
        assert!(r.max_freeze_ms > 1000, "{loss}: {r:?}");
        assert!(r.key_requests > 30, "{loss}: {r:?}");
        assert!(r.audio_gap_pct > 2.0, "{loss}: {r:?}");
    }
}

#[test]
fn new_logic_keeps_fps_and_audio_at_3_and_5_percent_loss() {
    for model in [LossModel::Random, LossModel::Bursty] {
        for loss in [0.03, 0.05] {
            let r = run(Mode::New, loss, model);
            let label = format!("новая логика, {}% ({})", loss * 100.0, if matches!(model, LossModel::Random) { "случайные" } else { "пачками" });
            show(&label, &r);
            let o = run(Mode::Old, loss, model);
            show(&format!("  для сравнения: старая {}%", loss * 100.0), &o);
            assert_eq!(r.corrupt_frames, 0);
            // video: practically every frame is shown, no long freezes, and almost no keyframe requests
            assert!(r.fps >= 28.0, "{label}: {r:?}");
            assert!(r.max_freeze_ms <= 500, "{label}: {r:?}");
            assert!(r.key_requests <= 4, "{label}: {r:?}");
            assert!(r.fps > o.fps * 2.0, "{label}");
            // audio: far fewer holes than before; whatever is left is concealed by the player
            // (bursts of 3+ packets in a row beat a single redundant copy, those holes are left to concealment)
            assert!(r.audio_gap_pct < 2.0, "{label}: {r:?}");
            assert!(r.audio_gap_pct * 2.5 < o.audio_gap_pct, "{label}: {r:?} vs {o:?}");
            assert!(r.audio_max_ts_err_ms < 0.01, "audio timestamps drifted: {r:?}");
            // retransmissions are a modest share of the traffic (limited by the bucket)
            assert!(r.mbps < o.mbps * 1.35 + 1.0, "{label}: {r:?} vs {o:?}");
        }
    }
}

#[test]
fn heavy_loss_degrades_gracefully_without_a_keyframe_storm() {
    let r = run(Mode::New, 0.15, LossModel::Bursty);
    show("новая логика, 15% пачками", &r);
    // not pretty, but it keeps going and does not hammer the sender with keyframe requests
    assert!(r.fps > 8.0, "{r:?}");
    assert!(r.key_requests < 40, "{r:?}");
    assert_eq!(r.corrupt_frames, 0);
}
