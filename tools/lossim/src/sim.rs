// Discrete-time simulation of host -> link -> viewer, driving the REAL protocol modules
// (retx / reassembly / ratectl / audiopkt, included from ../../src) or the v0.5.0 receiver logic.
// Virtual clock: nothing here sleeps, a 60-second scenario runs in about a second.
use super::legacy;
use super::net;
use super::{audiopkt, ratectl, reassembly, retx};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, VecDeque};
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

const STEP_NS: u64 = 500_000; // 0.5 ms
const FPS: u64 = 60;
const TOKEN: u32 = 0xC0FFEE;
const MAX_SEND_MBPS: u32 = 80;
const WARMUP_S: f64 = 2.0;

pub struct Rng(u64);
impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    pub fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    /// ~N(0,1)
    fn gauss(&mut self) -> f64 {
        (0..12).map(|_| self.f()).sum::<f64>() - 6.0
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Loss {
    None,
    Iid(f64),
    /// Gilbert-Elliott: average loss `p`, mean burst length `len` packets (in a burst everything is lost).
    Burst { p: f64, len: f64 },
}

impl Loss {
    pub fn label(&self) -> String {
        match self {
            Loss::None => "0%".into(),
            Loss::Iid(p) => format!("{:.0}% случайные", p * 100.0),
            Loss::Burst { p, len } => format!("{:.0}% пачками (~{len:.0} пак.)", p * 100.0),
        }
    }
}

struct Link {
    cap_bytes_s: f64, // 0 = unlimited
    queue_bytes: f64,
    prop_ns: u64,
    jitter_ns: u64,
    loss: Loss,
    bad: bool,
    busy_until: u64,
    q: BinaryHeap<Reverse<(u64, u64, Vec<u8>)>>,
    n: u64,
    pub dropped_loss: u64,
    pub dropped_queue: u64,
    pub bytes: u64,
}

impl Link {
    fn new(cap_mbps: f64, queue_ms: f64, one_way_ms: f64, jitter_ms: f64, loss: Loss) -> Self {
        let cap = cap_mbps * 125_000.0;
        Link {
            cap_bytes_s: cap,
            queue_bytes: if cap > 0.0 { cap * queue_ms / 1000.0 } else { 0.0 },
            prop_ns: (one_way_ms * 1e6) as u64,
            jitter_ns: (jitter_ms * 1e6) as u64,
            loss,
            bad: false,
            busy_until: 0,
            q: BinaryHeap::new(),
            n: 0,
            dropped_loss: 0,
            dropped_queue: 0,
            bytes: 0,
        }
    }

    fn lost(&mut self, rng: &mut Rng) -> bool {
        match self.loss {
            Loss::None => false,
            Loss::Iid(p) => rng.f() < p,
            Loss::Burst { p, len } => {
                let p_bg = 1.0 / len;
                let p_gb = p * p_bg / (1.0 - p);
                self.bad = if self.bad { rng.f() >= p_bg } else { rng.f() < p_gb };
                self.bad
            }
        }
    }

    fn send(&mut self, t: u64, pkt: &[u8], rng: &mut Rng) {
        self.bytes += pkt.len() as u64;
        if self.lost(rng) {
            self.dropped_loss += 1;
            return;
        }
        let mut depart = t;
        if self.cap_bytes_s > 0.0 {
            let backlog_ns = self.busy_until.saturating_sub(t);
            if backlog_ns as f64 * 1e-9 * self.cap_bytes_s > self.queue_bytes {
                self.dropped_queue += 1;
                return;
            }
            let ser = ((pkt.len() + 28) as f64 / self.cap_bytes_s * 1e9) as u64;
            self.busy_until = self.busy_until.max(t) + ser;
            depart = self.busy_until;
        }
        let jit = if self.jitter_ns > 0 { (rng.f() * self.jitter_ns as f64) as u64 } else { 0 };
        self.n += 1;
        self.q.push(Reverse((depart + self.prop_ns + jit, self.n, pkt.to_vec())));
    }

    fn recv(&mut self, t: u64) -> Option<Vec<u8>> {
        if self.q.peek().is_some_and(|Reverse((at, _, _))| *at <= t) {
            return self.q.pop().map(|Reverse((_, _, p))| p);
        }
        None
    }
}

#[derive(Clone, Debug)]
pub struct Cfg {
    pub name: String,
    /// true = new code (v0.5.1), false = v0.5.0 behaviour
    pub new: bool,
    pub loss: Loss,
    /// the viewer -> host direction (NACKs, requests, reports) is lossy too
    pub rev_loss: bool,
    pub cap_mbps: f64,
    pub queue_ms: f64,
    pub rtt_ms: f64,
    pub jitter_ms: f64,
    pub bitrate_mbps: f64,
    pub secs: f64,
    pub seed: u64,
    pub audio: bool,
}

impl Cfg {
    pub fn base(name: &str, new: bool, loss: Loss) -> Self {
        Cfg {
            name: name.into(),
            new,
            loss,
            rev_loss: true,
            cap_mbps: 0.0,
            queue_ms: 60.0,
            rtt_ms: 40.0,
            jitter_ms: 2.0,
            bitrate_mbps: 40.0,
            secs: 60.0,
            seed: 1,
            audio: true,
        }
    }
}

#[derive(Default, Clone, Debug)]
pub struct Out {
    pub fps: f64,
    pub fps_min: f64,
    pub frames: u64,
    pub freeze_s: f64,
    pub max_gap_ms: f64,
    pub keyreq: u64,
    pub keys_sent: u64,
    pub lat_p50: f64,
    pub lat_p95: f64,
    pub corrupt: u64,
    pub resent: u64,
    pub recovered: u64,
    pub gave_up: u64,
    pub video_mbps: f64,
    pub bitrate_avg: f64,
    pub bitrate_end: f64,
    pub link_dropped_queue: u64,
    // audio
    pub a_sent: u64,
    pub a_lost: u64,
    pub a_rebuilt: u64,
    pub a_glitches: u64,
    pub a_shift_ms: f64,
    pub a_snr_db: f64,
    pub a_mbps: f64,
    pub timeline: Vec<f64>, // fps per second
}

fn src_sample(i: u64) -> i16 {
    let t = i as f64 / 2.0 / 48000.0; // frame index -> seconds (stereo interleaved: both channels equal)
    ((t * 2.0 * std::f64::consts::PI * (220.0 + 40.0 * (t * 0.7).sin())).sin() * 9000.0
        + (t * 2.0 * std::f64::consts::PI * 1370.0).sin() * 2500.0) as i16
}

struct Frame {
    enc_ns: u64,
    key: bool,
    corrupt: bool,
    data: Vec<u8>,
}

pub fn run(cfg: &Cfg) -> Out {
    let base = Instant::now();
    let at = |t: u64| base + Duration::from_nanos(t);
    let mut rng = Rng::new(cfg.seed);
    let mut rng_enc = Rng::new(cfg.seed ^ 0xABCDEF);
    let half = cfg.rtt_ms / 2.0;
    let mut down = Link::new(cfg.cap_mbps, cfg.queue_ms, half, cfg.jitter_ms, cfg.loss); // host -> viewer
    let mut up = Link::new(0.0, 0.0, half, cfg.jitter_ms / 2.0, if cfg.rev_loss { cfg.loss } else { Loss::None });
    let end_ns = (cfg.secs * 1e9) as u64;

    // ---- host
    let mut sender = retx::Sender::new(TOKEN, at(0));
    let mut gate = retx::KeyGate::new(at(0));
    let legacy_cooldown = Duration::from_millis(250);
    let mut legacy_last_key: Option<u64> = None;
    let mut legacy_pending = true;
    let mut rc = ratectl::RateControl::new(cfg.bitrate_mbps, at(0));
    let mut tracker = ratectl::ReportTracker::default();
    let mut bitrate = cfg.bitrate_mbps;
    let mut pace = retx::pacer_rate(bitrate, MAX_SEND_MBPS) as f64;
    let mut chan: VecDeque<Frame> = VecDeque::new();
    let mut chain_broken = false;
    let mut next_enc_ns = 0u64;
    let mut fid_next = 0u32;
    let mut meta: BTreeMap<u32, (u64, bool)> = BTreeMap::new(); // fid -> (encode time, corrupt)
    let mut keys_sent = 0u64;
    let mut bitrate_acc = 0.0f64;
    let mut bitrate_n = 0u64;
    let a_seq = Arc::new(AtomicU32::new(0));
    let mut atx = audiopkt::AudioTx::new(TOKEN, a_seq.clone());
    let mut legacy_aseq = 0u32;
    let mut a_sent = 0u64;
    let mut a_bytes = 0u64;
    let mut src_audio: Vec<i16> = Vec::new();
    let mut next_audio_ns = 0u64;

    // ---- viewer
    let mut re = reassembly::Reassembler::new();
    let mut lre = legacy::Reassembler::new();
    let mut kr = reassembly::KeyRequester::new();
    let mut arx = audiopkt::AudioRx::new();
    let mut out_audio: Vec<i16> = Vec::new();
    let mut a_arrived: Vec<bool> = Vec::new();
    let mut first_aseq: Option<usize> = None;
    let mut rtt = Duration::from_millis(0);
    let mut delivered: Vec<(u64, f64, bool)> = Vec::new(); // (time, latency ms, corrupt)
    let mut keyreq = 0u64;
    let mut frames_out: Vec<(Vec<u8>, bool)> = Vec::new();
    let mut vid_bytes = 0u64;
    let mut next_nack_ns = 0u64;
    let mut next_key_ns = 0u64;
    let mut next_rep_ns = 500_000_000u64;
    let mut legacy_key_tick = 0u64;

    let mut t = 0u64;
    while t < end_ns {
        let now = at(t);

        // ---- host receives control packets
        while let Some(p) = up.recv(t) {
            let Some((ty, body)) = net::parse_header(&p, TOKEN) else { continue };
            match ty {
                net::PT_KEYREQ => {
                    if cfg.new {
                        gate.request()
                    } else {
                        legacy_pending = true
                    }
                }
                net::PT_NACK => {
                    if let Some((fid, idxs)) = retx::parse_nack(body) {
                        sender.on_nack(now, fid, &idxs);
                    }
                }
                net::PT_REPORT if cfg.new => {
                    if let Some(d) = tracker.on_report(body) {
                        if let Some(b) = rc.update(now, d) {
                            bitrate = b;
                            pace = retx::pacer_rate(b, MAX_SEND_MBPS) as f64;
                        }
                    }
                }
                net::PT_PING => {
                    let mut pong = Vec::new();
                    net::write_header(&mut pong, net::PT_PONG, TOKEN);
                    pong.extend_from_slice(body);
                    down.send(t, &pong, &mut rng);
                }
                _ => {}
            }
        }

        // ---- host audio capture (10 ms of 48 kHz stereo)
        if cfg.audio && t >= next_audio_ns {
            next_audio_ns += 10_000_000;
            let start = src_audio.len() as u64;
            let chunk: Vec<i16> = (0..960).map(|i| src_sample(start + i)).collect();
            src_audio.extend_from_slice(&chunk);
            if cfg.new {
                let mut pk = Vec::new();
                atx.push(&chunk, 48000, |p| pk.push(p.to_vec()));
                for p in pk {
                    a_sent += 1;
                    a_bytes += p.len() as u64;
                    down.send(t, &p, &mut rng);
                }
            } else {
                for c in chunk.chunks(480) {
                    let mut p = Vec::new();
                    net::write_header(&mut p, net::PT_AUDIO, TOKEN);
                    p.extend_from_slice(&legacy_aseq.to_le_bytes());
                    p.extend_from_slice(&48000u32.to_le_bytes());
                    for s in c {
                        p.extend_from_slice(&s.to_le_bytes());
                    }
                    legacy_aseq += 1;
                    a_sent += 1;
                    a_bytes += p.len() as u64;
                    down.send(t, &p, &mut rng);
                }
            }
        }

        // ---- host: capture/encode tick
        if t >= next_enc_ns {
            next_enc_ns += 1_000_000_000 / FPS;
            let want_key = if cfg.new {
                gate.take(now)
            } else if legacy_pending && legacy_last_key.map_or(true, |l| Duration::from_nanos(t - l) > legacy_cooldown) {
                legacy_pending = false;
                legacy_last_key = Some(t);
                true
            } else {
                false
            };
            let mean = bitrate * 125_000.0 / FPS as f64;
            let size = if want_key { mean * 6.0 } else { (mean * (rng_enc.gauss() * 0.35).exp()).max(300.0) };
            if want_key {
                chain_broken = false;
            }
            let mut data = vec![0u8; size as usize];
            data.truncate(size as usize);
            let f = Frame { enc_ns: t, key: want_key, corrupt: chain_broken, data };
            if chan.len() >= 6 {
                // channel to the sender thread is full: this frame is lost and the chain is broken
                chain_broken = true;
                if cfg.new {
                    gate.request()
                } else {
                    legacy_pending = true
                }
            } else {
                chan.push_back(f);
            }
            bitrate_acc += bitrate;
            bitrate_n += 1;
        }

        // ---- host sender thread
        if !sender.busy() {
            if let Some(f) = chan.pop_front() {
                let fid = fid_next;
                fid_next += 1;
                let mut d = f.data;
                if d.len() < 4 {
                    d.resize(4, 0);
                }
                d[..4].copy_from_slice(&fid.to_le_bytes());
                meta.insert(fid, (f.enc_ns, f.corrupt));
                if f.key {
                    keys_sent += 1;
                }
                sender.push_frame(fid, f.key, &d);
            }
        }
        loop {
            match sender.poll(now, pace) {
                retx::Poll::Send(p) => down.send(t, &p, &mut rng),
                _ => break,
            }
        }

        // ---- viewer receives
        while let Some(p) = down.recv(t) {
            let Some((ty, body)) = net::parse_header(&p, TOKEN) else { continue };
            match ty {
                net::PT_VIDEO if body.len() >= 9 => {
                    vid_bytes += p.len() as u64;
                    let fid = u32::from_le_bytes(body[0..4].try_into().unwrap());
                    let idx = u16::from_le_bytes([body[4], body[5]]);
                    let count = u16::from_le_bytes([body[6], body[7]]);
                    let key = body[8] != 0;
                    frames_out.clear();
                    if cfg.new {
                        re.on_fragment(now, fid, idx, count, key, &body[9..], &mut frames_out);
                    } else {
                        lre.on_fragment(now, fid, idx, count, key, &body[9..], &mut frames_out);
                    }
                    for (d, _k) in frames_out.drain(..) {
                        let f = u32::from_le_bytes(d[..4].try_into().unwrap());
                        let (enc, corrupt) = meta[&f];
                        delivered.push((t, (t - enc) as f64 / 1e6, corrupt));
                    }
                }
                net::PT_AUDIO if body.len() >= 8 => {
                    let seq = u32::from_le_bytes(body[0..4].try_into().unwrap()) as usize;
                    if a_arrived.len() <= seq {
                        a_arrived.resize(seq + 1, false);
                    }
                    a_arrived[seq] = true;
                    out_audio.extend(body[8..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])));
                }
                net::PT_AUDIO2 => {
                    if body.len() >= 4 {
                        let seq = u32::from_le_bytes(body[0..4].try_into().unwrap()) as usize;
                        if a_arrived.len() <= seq {
                            a_arrived.resize(seq + 1, false);
                        }
                        a_arrived[seq] = true;
                        first_aseq.get_or_insert(seq);
                    }
                    arx.on_packet(body, |s, _| out_audio.extend_from_slice(s));
                }
                net::PT_PONG if body.len() >= 8 => {
                    let sent = u64::from_le_bytes(body[0..8].try_into().unwrap());
                    rtt = Duration::from_nanos(t.saturating_sub(sent));
                }
                _ => {}
            }
        }

        // ---- viewer timers
        if t >= next_nack_ns {
            next_nack_ns += 1_000_000;
            frames_out.clear();
            if cfg.new {
                for (fid, idxs) in re.nacks(now, rtt) {
                    for p in retx::nack_packets(TOKEN, fid, &idxs) {
                        up.send(t, &p, &mut rng);
                    }
                }
                re.check_timeout(now, rtt, &mut frames_out);
            } else {
                lre.check_timeout(now, &mut frames_out);
            }
            for (d, _k) in frames_out.drain(..) {
                let f = u32::from_le_bytes(d[..4].try_into().unwrap());
                let (enc, corrupt) = meta[&f];
                delivered.push((t, (t - enc) as f64 / 1e6, corrupt));
            }
        }
        if t >= next_key_ns {
            next_key_ns += 100_000_000;
            legacy_key_tick += 1;
            let send = if cfg.new {
                kr.poll(now, re.waiting_key)
            } else {
                lre.waiting_key && legacy_key_tick % 3 == 0
            };
            if send {
                keyreq += 1;
                let mut p = Vec::new();
                net::write_header(&mut p, net::PT_KEYREQ, TOKEN);
                up.send(t, &p, &mut rng);
            }
        }
        if t >= next_rep_ns {
            next_rep_ns += 500_000_000;
            let mut p = Vec::new();
            net::write_header(&mut p, net::PT_PING, TOKEN);
            p.extend_from_slice(&t.to_le_bytes());
            up.send(t, &p, &mut rng);
            if cfg.new {
                let mut p = Vec::new();
                net::write_header(&mut p, net::PT_REPORT, TOKEN);
                ratectl::encode_report(&re.stats, &mut p);
                up.send(t, &p, &mut rng);
            }
        }

        t += STEP_NS;
    }

    // ---- metrics
    let mut o = Out::default();
    let w0 = (WARMUP_S * 1e9) as u64;
    let span = cfg.secs - WARMUP_S;
    let mut buckets = vec![0u32; cfg.secs as usize];
    let mut lat = Vec::new();
    let mut prev = w0;
    let mut gaps_over = 0.0;
    let mut max_gap = 0.0f64;
    for &(tt, l, corrupt) in &delivered {
        if tt < w0 {
            continue;
        }
        o.frames += 1;
        if corrupt {
            o.corrupt += 1;
        }
        let bi = ((tt / 1_000_000_000) as usize).min(buckets.len() - 1);
        buckets[bi] += 1;
        lat.push(l);
        let gap = (tt - prev) as f64 / 1e6;
        max_gap = max_gap.max(gap);
        if gap > 100.0 {
            gaps_over += gap;
        }
        prev = tt;
    }
    let tail = (end_ns - prev) as f64 / 1e6;
    max_gap = max_gap.max(tail);
    if tail > 100.0 {
        gaps_over += tail;
    }
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| if lat.is_empty() { f64::NAN } else { lat[((lat.len() - 1) as f64 * p) as usize] };
    o.fps = o.frames as f64 / span;
    o.fps_min = buckets[WARMUP_S as usize..].iter().copied().min().unwrap_or(0) as f64;
    o.timeline = buckets.iter().map(|&b| b as f64).collect();
    o.freeze_s = gaps_over / 1000.0;
    o.max_gap_ms = max_gap;
    o.keyreq = keyreq;
    o.keys_sent = keys_sent;
    o.lat_p50 = pct(0.5);
    o.lat_p95 = pct(0.95);
    o.resent = sender.resent;
    o.recovered = re.stats.recovered;
    o.gave_up = re.stats.gave_up;
    o.video_mbps = vid_bytes as f64 * 8.0 / cfg.secs / 1e6;
    o.bitrate_avg = bitrate_acc / bitrate_n.max(1) as f64;
    o.bitrate_end = bitrate;
    o.link_dropped_queue = down.dropped_queue;

    if cfg.new {
        arx.flush(|s, _| out_audio.extend_from_slice(s));
    }
    if cfg.audio {
        // Only look at packets that could have arrived before the end.
        let sent_ok = a_sent.saturating_sub((cfg.rtt_ms / 5.0) as u64 + 8) as usize;
        a_arrived.resize(a_arrived.len().max(sent_ok), false);
        let upto = sent_ok.min(a_arrived.len());
        o.a_sent = upto as u64;
        o.a_lost = a_arrived[..upto].iter().filter(|&&a| !a).count() as u64;
        // Rebuildable = new code only: lost packet whose successor arrived.
        let mut rebuilt = 0;
        let mut glitches = 0;
        let mut in_run = false;
        for i in 0..upto {
            let lost = !a_arrived[i];
            let fixed = cfg.new && lost && a_arrived.get(i + 1).copied().unwrap_or(false);
            if lost && fixed {
                rebuilt += 1;
            }
            let hole = lost && !fixed;
            if hole && !in_run {
                glitches += 1;
            }
            in_run = hole;
        }
        o.a_rebuilt = rebuilt;
        o.a_glitches = glitches;
        let chunk = if cfg.new { audiopkt::AUDIO_CHUNK } else { 480 } as f64;
        let expected = (upto as f64) * chunk;
        o.a_shift_ms = (expected - out_audio.len() as f64) / 2.0 / 48.0;
        if cfg.new {
            // the viewer's timeline starts at the first datagram that reached it
            let off = first_aseq.unwrap_or(0) * audiopkt::AUDIO_CHUNK;
            let n = out_audio.len().min(src_audio.len().saturating_sub(off));
            let (mut sig, mut err) = (0.0f64, 0.0f64);
            for i in 0..n {
                let s = src_audio[off + i] as f64;
                sig += s * s;
                err += (s - out_audio[i] as f64).powi(2);
            }
            o.a_snr_db = 10.0 * (sig / err.max(1.0)).log10();
        } else {
            o.a_snr_db = f64::NAN;
        }
        o.a_mbps = a_bytes as f64 * 8.0 / cfg.secs / 1e6;
    }
    o
}
