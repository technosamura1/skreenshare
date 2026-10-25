//! Receiver side of audio loss handling: unpacks the redundant copy of the previous chunk, so a single lost
//! packet costs nothing, and counts what could not be recovered (the player then fills the hole by timestamp:
//! see `AudioTimeline` / the concealment in `AudioPlayer.kt`).

pub struct Chunk {
    /// Capture time (sender clock, ms) of the first sample.
    pub ts_ms: f64,
    pub data: Vec<u8>,
}

pub struct AudioRx {
    rate: u32,
    channels: u32,
    last_seq: Option<u32>,
    pub received: u32,
    /// Chunks that were lost but rebuilt from the next packet's copy.
    pub recovered: u32,
    /// Chunks lost for good (the player conceals the hole).
    pub lost: u32,
}

impl AudioRx {
    pub fn new(rate: u32, channels: u32) -> Self {
        AudioRx { rate, channels: channels.max(1), last_seq: None, received: 0, recovered: 0, lost: 0 }
    }

    pub fn reset(&mut self) {
        self.last_seq = None;
    }

    fn chunk_ms(&self, bytes: usize) -> f64 {
        (bytes / (2 * self.channels as usize)) as f64 * 1000.0 / self.rate as f64
    }

    /// Fresh sequence number? (UDP may reorder or duplicate: an older chunk than one already queued is useless.)
    fn accept(&mut self, seq: u32) -> Option<u32> {
        let d = match self.last_seq {
            None => 1,
            Some(l) => seq.wrapping_sub(l) as i32,
        };
        if d <= 0 {
            return None;
        }
        self.last_seq = Some(seq);
        Some(d as u32)
    }

    /// A plain v0.3 AUDIO packet.
    pub fn on_plain(&mut self, seq: u32, ts_ms: u32, pcm: &[u8], out: &mut Vec<Chunk>) {
        let Some(d) = self.accept(seq) else { return };
        self.received += 1;
        self.lost += d - 1;
        out.push(Chunk { ts_ms: ts_ms as f64, data: pcm.to_vec() });
    }

    /// An AUDIO_RED packet: current chunk plus (maybe) a copy of the previous one.
    pub fn on_red(&mut self, seq: u32, ts_ms: u32, prev: &[u8], cur: &[u8], out: &mut Vec<Chunk>) {
        let Some(d) = self.accept(seq) else { return };
        self.received += 1;
        let mut missing = d - 1;
        if d >= 2 && !prev.is_empty() {
            // the chunk just before this one was lost, but its copy is here
            out.push(Chunk { ts_ms: ts_ms as f64 - self.chunk_ms(prev.len()), data: prev.to_vec() });
            self.recovered += 1;
            missing -= 1;
        }
        self.lost += missing;
        out.push(Chunk { ts_ms: ts_ms as f64, data: cur.to_vec() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CH: usize = 576; // 3 ms of 48 kHz stereo is 576 bytes

    fn ts(n: u32) -> u32 {
        n * 3
    }

    #[test]
    fn single_loss_is_rebuilt_from_the_next_packet() {
        let mut a = AudioRx::new(48000, 2);
        let mut out = Vec::new();
        a.on_red(0, ts(0), &[], &[0; CH], &mut out);
        // packet 1 lost
        a.on_red(2, ts(2), &[1; CH], &[2; CH], &mut out);
        assert_eq!(out.len(), 3);
        assert_eq!(out[1].data[0], 1);
        assert!((out[1].ts_ms - 3.0).abs() < 1e-9, "{}", out[1].ts_ms);
        assert_eq!((a.received, a.recovered, a.lost), (2, 1, 0));
    }

    #[test]
    fn double_loss_leaves_one_hole_for_the_player() {
        let mut a = AudioRx::new(48000, 2);
        let mut out = Vec::new();
        a.on_red(0, ts(0), &[], &[0; CH], &mut out);
        a.on_red(3, ts(3), &[2; CH], &[3; CH], &mut out); // 1 and 2 lost; copy of 2 is here
        assert_eq!((a.recovered, a.lost), (1, 1));
    }

    #[test]
    fn old_and_duplicate_packets_are_ignored() {
        let mut a = AudioRx::new(48000, 2);
        let mut out = Vec::new();
        a.on_red(5, ts(5), &[], &[5; CH], &mut out);
        a.on_red(5, ts(5), &[], &[5; CH], &mut out);
        a.on_red(4, ts(4), &[3; CH], &[4; CH], &mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn plain_packets_count_losses_without_recovery() {
        let mut a = AudioRx::new(48000, 2);
        let mut out = Vec::new();
        a.on_plain(0, 0, &[0; 960], &mut out);
        a.on_plain(3, 15, &[0; 960], &mut out);
        assert_eq!((a.received, a.recovered, a.lost), (2, 0, 2));
    }
}
