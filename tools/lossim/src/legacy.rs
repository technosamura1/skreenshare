// The v0.5.0 receiver logic ("before"), copied from src/client.rs with only the clock made injectable,
// so the simulator can show the old behaviour on exactly the same loss traces as the new code.
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub const MAX_PAYLOAD: usize = 1200;

struct Partial {
    count: u16,
    got: u16,
    key: bool,
    frags: Vec<Option<Vec<u8>>>,
    first: Instant,
}

pub struct Reassembler {
    partial: BTreeMap<u32, Partial>,
    done: BTreeMap<u32, (Vec<u8>, bool)>,
    next: Option<u32>,
    pub waiting_key: bool,
    pub losses: u64,
}

impl Reassembler {
    pub fn new() -> Self {
        Reassembler { partial: BTreeMap::new(), done: BTreeMap::new(), next: None, waiting_key: true, losses: 0 }
    }

    pub fn on_fragment(&mut self, now: Instant, fid: u32, idx: u16, count: u16, key: bool, data: &[u8], out: &mut Vec<(Vec<u8>, bool)>) {
        if self.next.is_some_and(|n| fid < n) || self.done.contains_key(&fid) || count == 0 || idx >= count {
            return;
        }
        let p = self.partial.entry(fid).or_insert_with(|| Partial { count, got: 0, key, frags: vec![None; count as usize], first: now });
        if p.count != count || p.frags[idx as usize].is_some() {
            return;
        }
        p.frags[idx as usize] = Some(data.to_vec());
        p.got += 1;
        if p.got == p.count {
            let p = self.partial.remove(&fid).unwrap();
            let mut buf = Vec::with_capacity(p.count as usize * MAX_PAYLOAD);
            for f in p.frags {
                buf.extend_from_slice(&f.unwrap());
            }
            self.done.insert(fid, (buf, p.key));
        }
        while self.partial.len() > 120 {
            self.partial.pop_first();
        }
        self.drain(out);
    }

    pub fn check_timeout(&mut self, now: Instant, out: &mut Vec<(Vec<u8>, bool)>) {
        if self.waiting_key {
            return;
        }
        if let Some(n) = self.next {
            if self.partial.get(&n).is_some_and(|p| now.duration_since(p.first) > Duration::from_millis(150)) {
                self.lose();
                self.drain(out);
            }
        }
    }

    fn lose(&mut self) {
        self.waiting_key = true;
        self.losses += 1;
    }

    fn drain(&mut self, out: &mut Vec<(Vec<u8>, bool)>) {
        loop {
            if self.waiting_key {
                let Some(k) = self.done.iter().find(|(_, (_, key))| *key).map(|(f, _)| *f) else {
                    match self.partial.iter().find(|(_, p)| p.key).map(|(f, _)| *f) {
                        Some(kp) => self.done.retain(|&f, _| f > kp),
                        None => self.done.clear(),
                    }
                    return;
                };
                self.done = self.done.split_off(&k);
                self.partial = self.partial.split_off(&k);
                self.next = Some(k);
                self.waiting_key = false;
            }
            let n = self.next.unwrap();
            if let Some(f) = self.done.remove(&n) {
                out.push(f);
                self.next = Some(n + 1);
                self.partial = self.partial.split_off(&(n + 1));
                continue;
            }
            if self.done.last_key_value().is_some_and(|(&last, _)| last >= n + 3) {
                self.lose();
                continue;
            }
            return;
        }
    }
}
