// Encoder choice helpers (pure std, tested in tools/lossim): what codec a stream carries, how big a software
// (x264) stream may be on this CPU, and a readable Russian explanation when NVENC refuses to start.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Hevc,
    H264,
}

/// Looks at the first NAL unit of an Annex-B access unit. Keyframes of both encoders start with parameter
/// sets (HEVC: VPS/SPS/PPS = 0x40/0x42/0x44 + 0x01, H.264: SPS 0x67 + profile), so a keyframe tells the codec.
pub fn detect_codec(data: &[u8]) -> Option<Codec> {
    let n = data.len().min(64);
    let d = &data[..n];
    let mut i = 0;
    while i + 3 < d.len() {
        if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            let b0 = d[i + 3];
            let b1 = *d.get(i + 4)?;
            if b0 & 0x80 != 0 {
                return None;
            }
            let hevc_type = (b0 >> 1) & 0x3f;
            // HEVC: forbidden bit 0, layer id 0 (top bit of the next byte pair), temporal id + 1 = 1.
            if b0 & 0x81 == 0 && b1 == 0x01 && hevc_type <= 40 {
                return Some(Codec::Hevc);
            }
            let h264_type = b0 & 0x1f;
            if (1..=12).contains(&h264_type) {
                return Some(Codec::H264);
            }
            return None;
        }
        i += 1;
    }
    None
}

/// What a software encode may be on this CPU: output size, frame rate and the top bitrate worth using.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SoftPlan {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub max_mbps: f64,
}

fn even(v: u32) -> u32 {
    (v.max(2)) & !1
}

/// x264 "ultrafast" costs roughly this many pixels per second per core including the colour conversion.
const PX_PER_CORE: f64 = 12.0e6;

/// Picks the biggest picture the CPU can encode in real time: 1080p30 / 720p60 on ~6 cores, 1080p60 on 12+,
/// 720p30 on 4, smaller below. The source is never enlarged. Frame rate never goes below 30 unless the CPU
/// can't even do 480p30.
pub fn soft_plan(cores: usize, src_w: u32, src_h: u32, want_fps: u32) -> SoftPlan {
    let budget = cores.max(1) as f64 * PX_PER_CORE;
    let top = want_fps.clamp(1, 60);
    let mut fps_opts = vec![top];
    if top > 30 {
        fps_opts.push(30);
    }
    let src_h = src_h.max(2);
    let src_w = src_w.max(2);
    let mut heights: Vec<u32> = [1080u32, 900, 720, 576, 480, 360, 288].into_iter().filter(|&h| h < src_h).collect();
    heights.insert(0, src_h.min(1080).max(2)); // no scaling at all when the source is small enough
    heights.dedup();
    let mut pick = None;
    'outer: for &h in &heights {
        let w = if h == src_h { src_w } else { even((src_w as f64 * h as f64 / src_h as f64).round() as u32) };
        let (w, h) = if src_h > 1080 && h == 1080 { (even((src_w as f64 * 1080.0 / src_h as f64).round() as u32), 1080) } else { (w, h) };
        for &f in &fps_opts {
            if (w as f64) * (h as f64) * f as f64 <= budget {
                pick = Some((w, h, f));
                break 'outer;
            }
        }
    }
    let (w, h, f) = pick.unwrap_or_else(|| {
        let h = 288u32.min(src_h);
        (even((src_w as f64 * h as f64 / src_h as f64).round() as u32), h, 30.min(top))
    });
    let (w, h) = (even(w), even(h));
    let rate = w as f64 * h as f64 * f as f64;
    SoftPlan { width: w, height: h, fps: f, max_mbps: (rate / 1e6 * 0.18).clamp(2.0, 12.0) }
}

/// Facts pulled out of FFmpeg's log while NVENC was being opened.
#[derive(Debug, Default, PartialEq)]
pub struct NvLog {
    /// "Required: 11.1 Found: 9.1" (driver too old for this program's NVENC API).
    pub api: Option<(String, String)>,
    /// "The minimum required Nvidia driver for nvenc is 471.41 or newer".
    pub min_driver: Option<String>,
    /// "[ GPU #0 - < NVIDIA GeForce RTX 3060 > has Compute SM 8.6 ]".
    pub gpus: Vec<String>,
    pub no_cuda: bool,
    pub no_dll: bool,
    /// Last error-ish lines, for the log file.
    pub tail: Vec<String>,
}

pub fn parse_nv_log(log: &str) -> NvLog {
    let mut r = NvLog::default();
    for line in log.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        if let Some(p) = l.find("Required: ") {
            let rest = &l[p + 10..];
            if let Some((req, found)) = rest.split_once(" Found: ") {
                r.api = Some((req.trim().to_string(), found.trim().to_string()));
            }
        }
        if let Some(p) = l.find("minimum required Nvidia driver for nvenc is ") {
            let rest = &l[p + 43..];
            r.min_driver = rest.split_whitespace().next().map(|s| s.to_string());
        }
        if l.starts_with("[ GPU #") {
            if let (Some(a), Some(b)) = (l.find("< "), l.find(" >")) {
                if a + 2 <= b {
                    r.gpus.push(l[a + 2..b].to_string());
                }
            }
        }
        if l.contains("No CUDA capable devices") || l.contains("Cannot load nvcuda") || l.contains("cuInit") {
            r.no_cuda = true;
        }
        if l.contains("Cannot load nvEncodeAPI") || l.contains("nvEncodeAPI64.dll") && l.contains("annot") {
            r.no_dll = true;
        }
        r.tail.push(l.to_string());
    }
    if r.tail.len() > 12 {
        let n = r.tail.len() - 12;
        r.tail.drain(..n);
    }
    r
}

/// The text shown when NVENC can't start (the stream then goes on with the processor if possible).
pub fn explain_nvenc(err: &str, log: &str, min_driver_built: &str) -> String {
    let nv = parse_nv_log(log);
    let gpu = if nv.gpus.is_empty() { String::new() } else { format!(" Видеокарта: {}.", nv.gpus.join(", ")) };
    let min = nv.min_driver.clone().unwrap_or_else(|| min_driver_built.to_string());
    if let Some((req, found)) = &nv.api {
        return format!(
            "Драйвер NVIDIA слишком старый для аппаратного кодирования (нужен NVENC API {req}, в драйвере {found}; \
             минимальная версия драйвера — {min}).{gpu} Обнови драйвер на nvidia.com/drivers (или в приложении NVIDIA)."
        );
    }
    if nv.no_dll {
        return format!(
            "Не найден nvEncodeAPI64.dll — на этом ПК нет драйвера NVIDIA или он поставлен без кодека NVENC.{gpu} \
             Установи свежий драйвер с nvidia.com/drivers (минимум {min})."
        );
    }
    if nv.no_cuda {
        return format!("Драйвер NVIDIA не отвечает (CUDA недоступна) или видеокарты NVIDIA нет.{gpu} Установи драйвер {min} или новее.");
    }
    format!("NVENC не запустился: {err}.{gpu} Чаще всего помогает обновление драйвера NVIDIA (минимум {min}).")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_hevc_and_h264_keyframes() {
        assert_eq!(detect_codec(&[0, 0, 0, 1, 0x40, 0x01, 0x0c, 0x01]), Some(Codec::Hevc)); // VPS
        assert_eq!(detect_codec(&[0, 0, 1, 0x42, 0x01, 0x01, 0x01]), Some(Codec::Hevc)); // SPS
        assert_eq!(detect_codec(&[0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1f]), Some(Codec::H264)); // SPS high
        assert_eq!(detect_codec(&[0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f]), Some(Codec::H264)); // SPS baseline
        assert_eq!(detect_codec(&[0, 0, 0, 1, 0x65, 0x88, 0x84]), Some(Codec::H264)); // IDR slice
        assert_eq!(detect_codec(&[1, 2, 3]), None);
        assert_eq!(detect_codec(&[0, 0, 0, 1]), None);
    }

    #[test]
    fn soft_plan_follows_cpu() {
        let p = soft_plan(6, 2560, 1440, 60);
        assert_eq!((p.width, p.height, p.fps), (1920, 1080, 30)); // 1080p30
        let p = soft_plan(12, 2560, 1440, 60);
        assert_eq!((p.width, p.height, p.fps), (1920, 1080, 60));
        let p = soft_plan(4, 1920, 1080, 60);
        assert!(p.fps == 30 && (720..=900).contains(&p.height), "{p:?}"); // 4 cores: 900p30 at most
        let p = soft_plan(8, 3840, 2160, 60);
        assert!(p.width * p.height * p.fps <= 96_000_000 && p.fps >= 30, "{p:?}");
        assert_eq!(p.width % 2 + p.height % 2, 0);
        let p = soft_plan(2, 1920, 1080, 60);
        assert!(p.height <= 576 && p.fps >= 30, "{p:?}");
        let p = soft_plan(16, 1280, 720, 60);
        assert_eq!((p.width, p.height, p.fps), (1280, 720, 60)); // never enlarged
        let p = soft_plan(16, 1280, 720, 24);
        assert_eq!(p.fps, 24);
        assert!(soft_plan(1, 3840, 2160, 60).max_mbps >= 2.0);
        assert!(soft_plan(32, 3840, 2160, 60).max_mbps <= 12.0);
    }

    #[test]
    fn soft_plan_keeps_aspect_ratio() {
        let p = soft_plan(6, 3440, 1440, 60);
        let ar = p.width as f64 / p.height as f64;
        assert!((ar - 3440.0 / 1440.0).abs() < 0.02, "{p:?}");
    }

    #[test]
    fn parses_old_driver_log() {
        let log = "[hevc_nvenc @ 000] Driver does not support the required nvenc API version. Required: 11.1 Found: 9.1\n\
                   [hevc_nvenc @ 000] The minimum required Nvidia driver for nvenc is 471.41 or newer\n";
        let n = parse_nv_log(log);
        assert_eq!(n.api, Some(("11.1".into(), "9.1".into())));
        assert_eq!(n.min_driver.as_deref(), Some("471.41"));
        let t = explain_nvenc("Function not implemented", log, "471.41");
        assert!(t.contains("слишком старый") && t.contains("471.41") && t.contains("9.1"), "{t}");
    }

    #[test]
    fn parses_gpu_name_and_generic_failure() {
        let log = "[ GPU #0 - < NVIDIA GeForce RTX 3060 > has Compute SM 8.6 ]\nsomething odd\n";
        let n = parse_nv_log(log);
        assert_eq!(n.gpus, vec!["NVIDIA GeForce RTX 3060".to_string()]);
        let t = explain_nvenc("Function not implemented", log, "471.41");
        assert!(t.contains("RTX 3060") && t.contains("471.41") && t.contains("Function not implemented"), "{t}");
        let t = explain_nvenc("x", "Cannot load nvEncodeAPI64.dll\n", "471.41");
        assert!(t.contains("nvEncodeAPI64.dll"), "{t}");
    }
}
