// Remote control (v0.11.0): packet formats for input from the viewer to the host and the host's answers.
// Pure code (no Windows calls), unit-tested by tools/lossim.
//
// Viewer -> host, PT_INPUT (state snapshots, so a lost packet is repaired by the next one):
//   ver u8 (=1) | seq u16 | pad_mask u8 (bit i = virtual pad i is plugged in) | flags u8
//   flags bit 0: keyboard/mouse block follows, bit 1: pad blocks follow
//   pad blocks:  n u8 | n × [idx u8 | buttons u16 | lt u8 | rt u8 | lx i16 | ly i16 | rx i16 | ry i16]
//   kbm block:   mouse buttons u8 | total dx i32 | total dy i32 | total wheel v i32 | total wheel h i32 |
//                n u8 | n × held key u16 | m u8 | m × event [evseq u16 | kind u8 | code u16]
//   Mouse movement and wheel are running totals (the host applies the difference), so a lost packet only delays
//   motion. Held keys / buttons are snapshots; events (sent several times, numbered) catch taps shorter than a gap.
// Host -> viewer, PT_CTRL (about once a second and on change): ver u8 | flags u8 | pads u8
// Host -> viewer, PT_RUMBLE (on change, repeated; the viewer also stops by itself): ver u8 | seq u16 | idx u8 | large u8 | small u8
//
// Old peers: a viewer announces CAP_INPUT in PT_CAPS, the host answers PT_CTRL only to such viewers; unknown packet
// types are ignored by old programs.

pub const VERSION: u8 = 1;
pub const MAX_PADS: usize = 4;
pub const MAX_KEYS: usize = 24;
pub const MAX_EVENTS: usize = 24;

/// One Xbox 360 style controller state (the XInput layout).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PadState {
    pub buttons: u16,
    pub lt: u8,
    pub rt: u8,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
}

pub mod btn {
    pub const UP: u16 = 0x0001;
    pub const DOWN: u16 = 0x0002;
    pub const LEFT: u16 = 0x0004;
    pub const RIGHT: u16 = 0x0008;
    pub const START: u16 = 0x0010;
    pub const BACK: u16 = 0x0020;
    pub const LTHUMB: u16 = 0x0040;
    pub const RTHUMB: u16 = 0x0080;
    pub const LB: u16 = 0x0100;
    pub const RB: u16 = 0x0200;
    #[allow(dead_code)]
    pub const GUIDE: u16 = 0x0400;
    pub const A: u16 = 0x1000;
    pub const B: u16 = 0x2000;
    pub const X: u16 = 0x4000;
    pub const Y: u16 = 0x8000;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvKind {
    KeyUp,
    KeyDown,
    MouseUp,
    MouseDown,
}

impl EvKind {
    fn code(self) -> u8 {
        match self {
            EvKind::KeyUp => 0,
            EvKind::KeyDown => 1,
            EvKind::MouseUp => 2,
            EvKind::MouseDown => 3,
        }
    }
    fn from_code(c: u8) -> Option<EvKind> {
        Some(match c {
            0 => EvKind::KeyUp,
            1 => EvKind::KeyDown,
            2 => EvKind::MouseUp,
            3 => EvKind::MouseDown,
            _ => return None,
        })
    }
}

/// A numbered press or release: key = scan code (0xE000 | code for extended keys), mouse = button number 0..4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ev {
    pub seq: u16,
    pub kind: EvKind,
    pub code: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Kbm {
    /// Held mouse buttons: bit 0 left, 1 right, 2 middle, 3 back, 4 forward.
    pub buttons: u8,
    pub dx: i32,
    pub dy: i32,
    pub wheel_v: i32,
    pub wheel_h: i32,
    pub keys: Vec<u16>,
    pub events: Vec<Ev>,
    /// Where the viewer's pointer is on the picture, 0..=65535 both ways (since 0.11.7). With it the host puts
    /// its cursor on exactly that spot (the arrows match whatever the scaling); `dx`/`dy` are for games that
    /// hide the cursor and turn the camera with raw motion.
    pub abs: Option<(u16, u16)>,
}

/// Tag of the optional pointer-position tail after the kbm block (older hosts stop reading before it).
const ABS_TAG: u8 = 0xA1;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    pub seq: u16,
    pub pad_mask: u8,
    pub pads: Vec<(u8, PadState)>,
    pub kbm: Option<Kbm>,
}

/// True when `a` is newer than `b` in wrapping 16-bit sequence numbers.
pub fn newer(a: u16, b: u16) -> bool {
    let d = a.wrapping_sub(b);
    d != 0 && d < 0x8000
}

pub fn encode(f: &Frame, out: &mut Vec<u8>) {
    out.push(VERSION);
    out.extend_from_slice(&f.seq.to_le_bytes());
    out.push(f.pad_mask & 0x0F);
    let pads: Vec<&(u8, PadState)> = f.pads.iter().filter(|(i, _)| (*i as usize) < MAX_PADS).collect();
    out.push(f.kbm.is_some() as u8 | ((!pads.is_empty()) as u8) << 1);
    if !pads.is_empty() {
        out.push(pads.len() as u8);
        for (i, p) in pads {
            out.push(*i);
            out.extend_from_slice(&p.buttons.to_le_bytes());
            out.push(p.lt);
            out.push(p.rt);
            for v in [p.lx, p.ly, p.rx, p.ry] {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    if let Some(k) = &f.kbm {
        out.push(k.buttons & 0x1F);
        for v in [k.dx, k.dy, k.wheel_v, k.wheel_h] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        let keys = &k.keys[..k.keys.len().min(MAX_KEYS)];
        out.push(keys.len() as u8);
        for c in keys {
            out.extend_from_slice(&c.to_le_bytes());
        }
        let evs = &k.events[k.events.len().saturating_sub(MAX_EVENTS)..];
        out.push(evs.len() as u8);
        for e in evs {
            out.extend_from_slice(&e.seq.to_le_bytes());
            out.push(e.kind.code());
            out.extend_from_slice(&e.code.to_le_bytes());
        }
        if let Some((x, y)) = k.abs {
            out.push(ABS_TAG);
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
        }
    }
}

struct Rd<'a>(&'a [u8]);

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }
    fn i16(&mut self) -> Option<i16> {
        self.u16().map(|v| v as i16)
    }
    fn i32(&mut self) -> Option<i32> {
        self.take(4).map(|b| i32::from_le_bytes(b.try_into().unwrap()))
    }
}

/// None for a malformed packet or another protocol version (the host then just ignores it).
pub fn decode(body: &[u8]) -> Option<Frame> {
    let mut r = Rd(body);
    if r.u8()? != VERSION {
        return None;
    }
    let seq = r.u16()?;
    let pad_mask = r.u8()? & 0x0F;
    let flags = r.u8()?;
    let mut f = Frame { seq, pad_mask, pads: Vec::new(), kbm: None };
    if flags & 2 != 0 {
        let n = r.u8()? as usize;
        if n > MAX_PADS {
            return None;
        }
        for _ in 0..n {
            let idx = r.u8()?;
            let p = PadState { buttons: r.u16()?, lt: r.u8()?, rt: r.u8()?, lx: r.i16()?, ly: r.i16()?, rx: r.i16()?, ry: r.i16()? };
            if (idx as usize) >= MAX_PADS {
                return None;
            }
            f.pads.push((idx, p));
        }
    }
    if flags & 1 != 0 {
        let buttons = r.u8()? & 0x1F;
        let (dx, dy, wheel_v, wheel_h) = (r.i32()?, r.i32()?, r.i32()?, r.i32()?);
        let nk = r.u8()? as usize;
        if nk > MAX_KEYS {
            return None;
        }
        let mut keys = Vec::with_capacity(nk);
        for _ in 0..nk {
            keys.push(r.u16()?);
        }
        let ne = r.u8()? as usize;
        if ne > MAX_EVENTS {
            return None;
        }
        let mut events = Vec::with_capacity(ne);
        for _ in 0..ne {
            let seq = r.u16()?;
            let kind = EvKind::from_code(r.u8()?)?;
            events.push(Ev { seq, kind, code: r.u16()? });
        }
        let abs = match r.u8() {
            Some(ABS_TAG) => Some((r.u16()?, r.u16()?)),
            _ => None,
        };
        f.kbm = Some(Kbm { buttons, dx, dy, wheel_v, wheel_h, keys, events, abs });
    }
    Some(f)
}

/// What the host tells the viewer about control (PT_CTRL).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CtrlInfo {
    /// The host allows control in this session at all.
    pub allowed: bool,
    pub pad: bool,
    pub kbm: bool,
    /// The virtual gamepad driver (ViGEmBus) is installed on the host.
    pub vigem: bool,
    /// Virtual pads currently plugged in on the host.
    pub pads: u8,
    /// The host's cursor is hidden (a game took the mouse): the viewer's own pointer should hide too, and only raw
    /// motion counts. Otherwise the host follows the pointer's position on the picture (since 0.11.7).
    pub relative: bool,
    /// The owner turned down the viewer's request for control (a while; v0.12.8).
    pub denied: bool,
}

pub fn encode_ctrl(c: &CtrlInfo, out: &mut Vec<u8>) {
    out.push(VERSION);
    out.push(c.allowed as u8 | (c.pad as u8) << 1 | (c.kbm as u8) << 2 | (c.vigem as u8) << 3 | (c.relative as u8) << 4 | (c.denied as u8) << 5);
    out.push(c.pads);
}

pub fn decode_ctrl(body: &[u8]) -> Option<CtrlInfo> {
    if body.len() < 3 || body[0] != VERSION {
        return None;
    }
    let f = body[1];
    Some(CtrlInfo { allowed: f & 1 != 0, pad: f & 2 != 0, kbm: f & 4 != 0, vigem: f & 8 != 0, pads: body[2], relative: f & 16 != 0, denied: f & 32 != 0 })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rumble {
    pub seq: u16,
    pub idx: u8,
    pub large: u8,
    pub small: u8,
}

pub fn encode_rumble(r: &Rumble, out: &mut Vec<u8>) {
    out.push(VERSION);
    out.extend_from_slice(&r.seq.to_le_bytes());
    out.extend_from_slice(&[r.idx, r.large, r.small]);
}

pub fn decode_rumble(body: &[u8]) -> Option<Rumble> {
    if body.len() < 6 || body[0] != VERSION || body[3] as usize >= MAX_PADS {
        return None;
    }
    Some(Rumble { seq: u16::from_le_bytes([body[1], body[2]]), idx: body[3], large: body[4], small: body[5] })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad(n: i16) -> PadState {
        PadState { buttons: btn::A | btn::LB, lt: 7, rt: 255, lx: n, ly: -n, rx: i16::MAX, ry: i16::MIN }
    }

    #[test]
    fn frame_round_trips() {
        let f = Frame {
            seq: 65535,
            pad_mask: 0b0101,
            pads: vec![(0, pad(100)), (2, pad(-3))],
            kbm: Some(Kbm {
                buttons: 0b10101,
                dx: -123456,
                dy: i32::MAX,
                wheel_v: -240,
                wheel_h: 120,
                keys: vec![0x1E, 0xE04B],
                events: vec![Ev { seq: 9, kind: EvKind::KeyDown, code: 0x1E }, Ev { seq: 10, kind: EvKind::MouseUp, code: 4 }],
                abs: Some((0, 65535)),
            }),
        };
        let mut b = Vec::new();
        encode(&f, &mut b);
        assert_eq!(decode(&b), Some(f));
    }

    #[test]
    fn empty_frame_is_tiny_and_round_trips() {
        let f = Frame { seq: 1, ..Default::default() };
        let mut b = Vec::new();
        encode(&f, &mut b);
        assert_eq!(b.len(), 5);
        assert_eq!(decode(&b), Some(f));
    }

    #[test]
    fn packets_stay_small() {
        let k = Kbm {
            keys: (0..MAX_KEYS as u16).collect(),
            events: (0..MAX_EVENTS as u16).map(|i| Ev { seq: i, kind: EvKind::KeyDown, code: i }).collect(),
            ..Default::default()
        };
        let f = Frame { seq: 1, pad_mask: 15, pads: (0..4).map(|i| (i, pad(1))).collect(), kbm: Some(k) };
        let mut b = Vec::new();
        encode(&f, &mut b);
        assert!(b.len() < 400, "{}", b.len());
        assert_eq!(decode(&b), Some(f));
    }

    #[test]
    fn garbage_and_foreign_versions_are_ignored() {
        let f = Frame { seq: 5, pad_mask: 1, pads: vec![(0, pad(1))], kbm: Some(Kbm::default()) };
        let mut b = Vec::new();
        encode(&f, &mut b);
        for cut in 0..b.len() {
            assert_eq!(decode(&b[..cut]), None, "cut at {cut}");
        }
        let mut other = b.clone();
        other[0] = 2;
        assert_eq!(decode(&other), None);
        assert_eq!(decode(&[]), None);
        // random bytes never panic
        let mut x = 12345u32;
        for _ in 0..2000 {
            let v: Vec<u8> = (0..40).map(|_| { x = x.wrapping_mul(1664525).wrapping_add(1013904223); (x >> 24) as u8 }).collect();
            let _ = decode(&v);
        }
    }

    #[test]
    fn out_of_range_pad_or_counts_are_rejected() {
        // pad index 9
        let mut b = vec![VERSION, 1, 0, 0, 2, 1, 9];
        b.extend_from_slice(&[0; 12]);
        assert_eq!(decode(&b), None);
        // 200 keys
        let mut b = vec![VERSION, 1, 0, 0, 1, 0];
        b.extend_from_slice(&[0; 16]);
        b.push(200);
        assert_eq!(decode(&b), None);
    }

    #[test]
    fn pointer_position_rides_along_and_old_packets_still_work() {
        let k = Kbm { buttons: 1, dx: 5, abs: Some((12345, 65535)), ..Default::default() };
        let f = Frame { seq: 9, kbm: Some(k), ..Default::default() };
        let mut b = Vec::new();
        encode(&f, &mut b);
        assert_eq!(decode(&b), Some(f.clone()));
        // A 0.11.6 host reads up to the events and stops: it sees the same packet minus the position.
        let mut old = f.clone();
        old.kbm.as_mut().unwrap().abs = None;
        let mut ob = Vec::new();
        encode(&old, &mut ob);
        assert_eq!(&b[..ob.len()], &ob[..], "the position is only a tail");
        // A 0.11.6 viewer's packet (no tail) decodes with no position.
        assert_eq!(decode(&ob), Some(old));
    }

    #[test]
    fn sequence_compare_wraps() {
        assert!(newer(1, 0));
        assert!(newer(0, 65535));
        assert!(!newer(65535, 0));
        assert!(!newer(5, 5));
        assert!(newer(100, 65000));
    }

    #[test]
    fn ctrl_and_rumble_round_trip() {
        let c = CtrlInfo { allowed: true, pad: false, kbm: true, vigem: true, pads: 2, relative: true, denied: true };
        let mut b = Vec::new();
        encode_ctrl(&c, &mut b);
        assert_eq!(decode_ctrl(&b), Some(c));
        assert_eq!(decode_ctrl(&b[..2]), None);
        b.clear();
        let r = Rumble { seq: 77, idx: 3, large: 255, small: 1 };
        encode_rumble(&r, &mut b);
        assert_eq!(decode_rumble(&b), Some(r));
        b[3] = 4;
        assert_eq!(decode_rumble(&b), None);
    }
}
