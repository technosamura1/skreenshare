// Network quality lamp shared by the host and the viewer: green / yellow / red with a short Russian text.
// Pure logic (no clock of its own: every sample says how much time it covers), so tools/lossim tests it.
//
// A sample is judged on its own first (`judge`), then the lamp only moves after the judgement has stayed different
// for a while: getting worse needs ~1.5 s (red: ~1 s), getting better needs ~6 s and goes one step at a time.
// That stops it from flickering on a single bad report.

#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum Level {
    Good = 0,
    Weak = 1,
    Bad = 2,
}

/// Which end of the link shows the lamp (the wording differs: only the sender can say it lowered the quality).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Host,
    Viewer,
}

/// What one measuring interval looked like.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sample {
    /// Milliseconds this sample covers.
    pub dt_ms: u32,
    /// Share of video fragments lost on the way (0..1), before repairs.
    pub loss: f64,
    /// Share still missing after retransmissions (0..1).
    pub residual: f64,
    /// Round-trip time in ms; None = not known yet.
    pub rtt_ms: Option<f64>,
    /// Share of sent packets that were retransmissions (0..1); host only, 0 elsewhere.
    pub retx: f64,
    /// The sender recently cut the quality because of the network (host only).
    pub cut: bool,
    /// Current / maximum bitrate in Mbit/s (host only; 0 = unknown), for the tooltip.
    pub bitrate: f64,
    pub max_bitrate: f64,
}

/// Verdict for one sample, without memory.
pub fn judge(s: &Sample) -> Level {
    let rtt = s.rtt_ms.unwrap_or(0.0);
    if s.residual > 0.02 || s.loss > 0.10 || s.retx > 0.15 || rtt > 350.0 {
        Level::Bad
    } else if s.residual > 0.004 || s.loss > 0.03 || s.retx > 0.05 || rtt > 120.0 || s.cut {
        Level::Weak
    } else {
        Level::Good
    }
}

const WORSE_MS: u32 = 1500;
const WORSE_BAD_MS: u32 = 1000;
const BETTER_MS: u32 = 6000;

pub struct NetQuality {
    pub level: Level,
    side: Side,
    /// How long the judgement has pointed away from `level`, and in which direction.
    pending_ms: u32,
    pending: Level,
    last: Sample,
}

impl NetQuality {
    pub fn new(side: Side) -> Self {
        NetQuality { level: Level::Good, side, pending_ms: 0, pending: Level::Good, last: Sample::default() }
    }

    /// Feed one sample; returns the new level when the lamp changed.
    pub fn update(&mut self, s: Sample) -> Option<Level> {
        self.last = s;
        let j = judge(&s);
        if j == self.level {
            self.pending_ms = 0;
            self.pending = self.level;
            return None;
        }
        if j != self.pending {
            // The judgement changed direction or size: start counting again.
            self.pending = j;
            self.pending_ms = 0;
        }
        self.pending_ms += s.dt_ms;
        let before = self.level;
        if j > self.level {
            let need = if j == Level::Bad { WORSE_BAD_MS } else { WORSE_MS };
            if self.pending_ms >= need {
                self.level = j;
            }
        } else if self.pending_ms >= BETTER_MS {
            // One step up at a time: red -> yellow -> green.
            self.level = match self.level {
                Level::Bad => Level::Weak,
                _ => Level::Good,
            };
        }
        if self.level != before {
            self.pending_ms = 0;
            self.pending = self.level;
            Some(self.level)
        } else {
            None
        }
    }

    pub fn text(&self) -> &'static str {
        match (self.level, self.side) {
            (Level::Good, _) => "Сеть хорошая",
            (Level::Weak, Side::Host) => "Сеть слабая: снизил качество",
            (Level::Weak, Side::Viewer) => "Сеть слабая",
            (Level::Bad, _) => "Сеть плохая",
        }
    }

    /// Tooltip: the numbers behind the lamp.
    pub fn details(&self) -> String {
        let s = &self.last;
        let mut v = vec![format!("потери {:.1}%", s.loss * 100.0), format!("не восстановлено {:.1}%", s.residual * 100.0)];
        v.push(match s.rtt_ms {
            Some(r) => format!("пинг {r:.0} мс"),
            None => "пинг —".into(),
        });
        if self.side == Side::Host {
            v.push(format!("повторные пакеты {:.1}%", s.retx * 100.0));
            if s.max_bitrate > 0.0 {
                v.push(format!("битрейт {:.0} из {:.0} Мбит/с", s.bitrate, s.max_bitrate));
            }
        }
        v.join(" · ")
    }
}

/// Wire form for the GUI: `@NET <0|1|2>|<text>|<details>`.
pub fn encode(q: &NetQuality) -> String {
    format!("{}|{}|{}", q.level as u8, q.text(), q.details())
}

pub fn decode(s: &str) -> Option<(Level, String, String)> {
    let mut it = s.splitn(3, '|');
    let level = match it.next()?.trim() {
        "0" => Level::Good,
        "1" => Level::Weak,
        "2" => Level::Bad,
        _ => return None,
    };
    Some((level, it.next()?.to_string(), it.next().unwrap_or("").to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn smp(loss: f64, residual: f64, rtt: f64) -> Sample {
        Sample { dt_ms: 500, loss, residual, rtt_ms: Some(rtt), ..Default::default() }
    }

    fn run(q: &mut NetQuality, n: u32, s: Sample) -> Vec<Level> {
        (0..n).filter_map(|_| q.update(s)).collect()
    }

    #[test]
    fn clean_link_is_green() {
        let mut q = NetQuality::new(Side::Host);
        assert!(run(&mut q, 100, smp(0.0, 0.0, 30.0)).is_empty());
        assert_eq!((q.level, q.text()), (Level::Good, "Сеть хорошая"));
    }

    #[test]
    fn judging_thresholds() {
        assert_eq!(judge(&smp(0.01, 0.0, 40.0)), Level::Good);
        assert_eq!(judge(&smp(0.05, 0.0, 40.0)), Level::Weak);
        assert_eq!(judge(&smp(0.0, 0.0, 200.0)), Level::Weak);
        assert_eq!(judge(&smp(0.0, 0.0, 500.0)), Level::Bad);
        assert_eq!(judge(&smp(0.2, 0.0, 40.0)), Level::Bad);
        assert_eq!(judge(&smp(0.05, 0.03, 40.0)), Level::Bad);
        assert_eq!(judge(&Sample { retx: 0.08, ..smp(0.0, 0.0, 40.0) }), Level::Weak);
        assert_eq!(judge(&Sample { cut: true, ..smp(0.0, 0.0, 40.0) }), Level::Weak);
        assert_eq!(judge(&Sample { rtt_ms: None, ..smp(0.0, 0.0, 0.0) }), Level::Good);
    }

    #[test]
    fn one_bad_report_does_not_flicker_the_lamp() {
        let mut q = NetQuality::new(Side::Host);
        for i in 0..200 {
            // a bad half second every 3 s
            let s = if i % 6 == 0 { smp(0.15, 0.03, 300.0) } else { smp(0.0, 0.0, 30.0) };
            q.update(s);
        }
        assert_eq!(q.level, Level::Good);
    }

    #[test]
    fn sustained_trouble_turns_yellow_then_red_and_recovery_is_slow_and_stepwise() {
        let mut q = NetQuality::new(Side::Host);
        run(&mut q, 10, smp(0.0, 0.0, 30.0));
        assert_eq!(run(&mut q, 4, smp(0.05, 0.0, 40.0)), vec![Level::Weak]);
        assert_eq!(q.text(), "Сеть слабая: снизил качество");
        assert_eq!(run(&mut q, 4, smp(0.2, 0.05, 40.0)), vec![Level::Bad]);
        assert_eq!(q.text(), "Сеть плохая");
        // clean again: needs ~6 s per step, and goes red -> yellow -> green
        assert!(run(&mut q, 11, smp(0.0, 0.0, 30.0)).is_empty());
        assert_eq!(run(&mut q, 2, smp(0.0, 0.0, 30.0)), vec![Level::Weak]);
        assert_eq!(run(&mut q, 12, smp(0.0, 0.0, 30.0)), vec![Level::Good]);
    }

    #[test]
    fn short_clean_spell_inside_trouble_does_not_clear_it() {
        let mut q = NetQuality::new(Side::Viewer);
        run(&mut q, 6, smp(0.05, 0.0, 40.0));
        assert_eq!(q.level, Level::Weak);
        for _ in 0..5 {
            run(&mut q, 8, smp(0.0, 0.0, 30.0));
            run(&mut q, 2, smp(0.05, 0.0, 40.0));
        }
        assert_eq!(q.level, Level::Weak);
        assert_eq!(q.text(), "Сеть слабая");
    }

    #[test]
    fn wire_form_round_trips_and_details_have_the_numbers() {
        let mut q = NetQuality::new(Side::Host);
        q.update(Sample { bitrate: 28.0, max_bitrate: 60.0, retx: 0.03, ..smp(0.012, 0.0, 41.0) });
        let (l, t, d) = decode(&encode(&q)).unwrap();
        assert_eq!((l, t.as_str()), (Level::Good, "Сеть хорошая"));
        assert!(d.contains("потери 1.2%") && d.contains("пинг 41 мс") && d.contains("28 из 60"), "{d}");
        assert!(decode("9|x|y").is_none());
    }
}
