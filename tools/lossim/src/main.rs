// Packet-loss simulator: before (v0.5.0 logic) vs after (v0.5.1) on identical loss traces.
//   cargo run --release -- table          full comparison table (what the README quotes)
//   cargo run --release -- one 3 [burst]  one scenario, per-second fps timeline
#![allow(dead_code)]
#[path = "../../../src/audiopkt.rs"]
mod audiopkt;
mod legacy;
#[path = "../../../src/net.rs"]
mod net;
#[path = "../../../src/ratectl.rs"]
mod ratectl;
#[path = "../../../src/reassembly.rs"]
mod reassembly;
#[path = "../../../src/retx.rs"]
mod retx;
mod sim;
#[path = "../../../src/source.rs"]
#[allow(dead_code)]
mod source;
use sim::{Cfg, Loss, Out};

fn row(label: &str, tag: &str, o: &Out) {
    println!(
        "| {label} | {tag} | {:.1} | {:.0} | {:.2} | {} / {} | {:.0} / {:.0} | {} |",
        o.fps, o.fps_min, o.freeze_s, o.keyreq, o.keys_sent, o.lat_p50, o.lat_p95, o.corrupt
    );
}

fn arow(label: &str, tag: &str, o: &Out) {
    let snr = if o.a_snr_db.is_nan() { "—".to_string() } else { format!("{:.0} дБ", o.a_snr_db) };
    println!(
        "| {label} | {tag} | {} | {} | {} | {} | {:+.0} мс | {snr} | {:.2} |",
        o.a_sent, o.a_lost, o.a_rebuilt, o.a_glitches, -o.a_shift_ms, o.a_mbps
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("one") => {
            let p: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(3.0) / 100.0;
            let burst = args.get(2).is_some_and(|s| s == "burst");
            let loss = if burst { Loss::Burst { p, len: 4.0 } } else { Loss::Iid(p) };
            for new in [false, true] {
                let o = sim::run(&Cfg::base("one", new, loss));
                println!("{} {}: fps {:.1}, min {:.0}, keyreq {}", if new { "после" } else { "до" }, loss.label(), o.fps, o.fps_min, o.keyreq);
                println!("  переотправлено пакетов {}, восстановлено {}, сдались (ключ) {}, видео {:.1} Мбит/с, битрейт итог/средний {:.1}/{:.1}", o.resent, o.recovered, o.gave_up, o.video_mbps, o.bitrate_end, o.bitrate_avg);
                println!("  fps по секундам: {:?}", o.timeline.iter().map(|&x| x as u32).collect::<Vec<_>>());
            }
        }
        _ => table(),
    }
}

fn table() {
    let losses = [
        Loss::None,
        Loss::Iid(0.01),
        Loss::Iid(0.03),
        Loss::Iid(0.05),
        Loss::Burst { p: 0.03, len: 4.0 },
        Loss::Burst { p: 0.05, len: 6.0 },
    ];
    println!("## A. Видео: 40 Мбит/с, 60 кадров/с, 60 с, RTT 40 мс, канал 1 Гбит/с, потери в обе стороны\n");
    println!("| потери | версия | кадров/с (среднее) | худшая секунда | заморозка, с | запросов ключа / ключей отправлено | задержка p50 / p95, мс | битых кадров показано |");
    println!("|---|---|---|---|---|---|---|---|");
    let mut audio = Vec::new();
    for l in losses {
        for new in [false, true] {
            // average over 3 seeds
            let outs: Vec<Out> = (1..=3).map(|s| sim::run(&Cfg { seed: s, ..Cfg::base("a", new, l) })).collect();
            let o = avg(&outs);
            row(&l.label(), if new { "**0.5.1**" } else { "0.5.0" }, &o);
            audio.push((l, new, o));
        }
    }
    println!("\n## B. Звук (тот же прогон): пакеты, потери, склейка таймлайна\n");
    println!("| потери | версия | отправлено пакетов | потеряно | восстановлено копией | слышимых дыр | звук к концу укорочен на (−) / в пути (+) | SNR | Мбит/с |");
    println!("|---|---|---|---|---|---|---|---|---|");
    for (l, new, o) in &audio {
        arow(&l.label(), if *new { "**0.5.1**" } else { "0.5.0" }, o);
    }
    println!("\n## C. Узкий канал: хост просит 40 Мбит/с, канал 15 Мбит/с, очередь 60 мс, 0.5% случайных потерь, 120 с\n");
    println!("| версия | кадров/с | худшая секунда | заморозка, с | запросов ключа | потеряно в очереди роутера | итоговый битрейт, Мбит/с | средний битрейт |");
    println!("|---|---|---|---|---|---|---|---|");
    for new in [false, true] {
        let o = sim::run(&Cfg { cap_mbps: 15.0, secs: 120.0, ..Cfg::base("c", new, Loss::Iid(0.005)) });
        println!(
            "| {} | {:.1} | {:.0} | {:.2} | {} | {} | {:.1} | {:.1} |",
            if new { "**0.5.1**" } else { "0.5.0" },
            o.fps, o.fps_min, o.freeze_s, o.keyreq, o.link_dropped_queue, o.bitrate_end, o.bitrate_avg
        );
    }
    println!("\n## D. Перестановка пакетов (джиттер 8 мс), 1% потерь: не должно быть лишних запросов\n");
    println!("| версия | кадров/с | худшая секунда | запросов ключа | переотправлено пакетов |");
    println!("|---|---|---|---|---|");
    for new in [false, true] {
        let o = sim::run(&Cfg { jitter_ms: 8.0, ..Cfg::base("d", new, Loss::Iid(0.01)) });
        println!("| {} | {:.1} | {:.0} | {} | {} |", if new { "**0.5.1**" } else { "0.5.0" }, o.fps, o.fps_min, o.keyreq, o.resent);
    }
}

fn avg(v: &[Out]) -> Out {
    let n = v.len() as f64;
    let mut o = v[0].clone();
    o.fps = v.iter().map(|x| x.fps).sum::<f64>() / n;
    o.fps_min = v.iter().map(|x| x.fps_min).fold(f64::MAX, f64::min);
    o.freeze_s = v.iter().map(|x| x.freeze_s).sum::<f64>() / n;
    o.lat_p50 = v.iter().map(|x| x.lat_p50).sum::<f64>() / n;
    o.lat_p95 = v.iter().map(|x| x.lat_p95).sum::<f64>() / n;
    o.keyreq = (v.iter().map(|x| x.keyreq).sum::<u64>() as f64 / n).round() as u64;
    o.keys_sent = (v.iter().map(|x| x.keys_sent).sum::<u64>() as f64 / n).round() as u64;
    o.corrupt = (v.iter().map(|x| x.corrupt).sum::<u64>() as f64 / n).round() as u64;
    o.a_sent = (v.iter().map(|x| x.a_sent).sum::<u64>() as f64 / n).round() as u64;
    o.a_lost = (v.iter().map(|x| x.a_lost).sum::<u64>() as f64 / n).round() as u64;
    o.a_rebuilt = (v.iter().map(|x| x.a_rebuilt).sum::<u64>() as f64 / n).round() as u64;
    o.a_glitches = (v.iter().map(|x| x.a_glitches).sum::<u64>() as f64 / n).round() as u64;
    o.a_shift_ms = v.iter().map(|x| x.a_shift_ms).sum::<f64>() / n;
    o.a_snr_db = v.iter().map(|x| x.a_snr_db).sum::<f64>() / n;
    o
}

#[cfg(test)]
mod simtests {
    use super::*;
    #[test]
    fn audio_clean_link_is_aligned_and_clean() {
        let o = sim::run(&Cfg { jitter_ms: 0.0, secs: 8.0, ..Cfg::base("t", true, Loss::None) });
        assert!(o.a_snr_db > 30.0, "snr {}", o.a_snr_db);
        // jitter reorders datagrams a little; that must not be heard as loss
        let o = sim::run(&Cfg { jitter_ms: 2.0, secs: 8.0, ..Cfg::base("t", true, Loss::None) });
        assert!(o.a_snr_db > 30.0, "snr with jitter {}", o.a_snr_db);
    }

    #[test]
    fn new_beats_old_under_loss() {
        for p in [0.01, 0.05] {
            let old = sim::run(&Cfg { secs: 12.0, ..Cfg::base("o", false, Loss::Iid(p)) });
            let new = sim::run(&Cfg { secs: 12.0, ..Cfg::base("n", true, Loss::Iid(p)) });
            assert!(old.fps < 5.0, "old {}", old.fps);
            assert!(new.fps > 55.0, "new {}", new.fps);
            assert!(new.keyreq <= 2);
            assert!(new.a_glitches * 5 < old.a_glitches, "{} vs {}", new.a_glitches, old.a_glitches);
        }
    }
}
