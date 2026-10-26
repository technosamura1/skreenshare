// Pure logic of the launcher window (no Windows calls, so tools/lossim unit-tests it on any OS):
// thumbnail sizes and refresh scheduling, which connection step to show, and plain-Russian error texts.

/// Thumbnails are made at most this big (the card shows them scaled to its width).
pub const THUMB_W: u32 = 256;
pub const THUMB_H: u32 = 144;
/// A thumbnail is re-taken at most this often (ms) and only a few per tick: that is the CPU limit.
pub const REFRESH_MS: u64 = 3000;
pub const MAX_PER_TICK: usize = 2;
pub const TICK_MS: u64 = 350;
/// The worker stops by itself when nobody asked for a picture for this long (ms).
pub const IDLE_STOP_MS: u64 = 2500;

/// Size of a `w`x`h` picture scaled down to fit the `bw`x`bh` box, keeping the aspect ratio (never enlarged).
pub fn fit(w: u32, h: u32, bw: u32, bh: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (1, 1);
    }
    let k = (bw as f64 / w as f64).min(bh as f64 / h as f64).min(1.0);
    (((w as f64 * k).round() as u32).max(1), ((h as f64 * k).round() as u32).max(1))
}

/// In place: BGRA -> RGBA, alpha forced to opaque (a GDI copy of a window has no meaningful alpha).
pub fn bgra_to_opaque_rgba(px: &mut [u8]) {
    for p in px.chunks_exact_mut(4) {
        p.swap(0, 2);
        p[3] = 255;
    }
}

/// True when the picture is entirely black (a GPU surface PrintWindow could not read): not worth showing.
pub fn all_black(px: &[u8]) -> bool {
    px.chunks_exact(4).all(|p| p[0] == 0 && p[1] == 0 && p[2] == 0)
}

/// Which entries to refresh now: never-taken first, then the stalest that are due (older than `interval`),
/// at most `max`. `last[i]` is the time (ms) the i-th thumbnail was taken, None = never.
pub fn pick_batch(last: &[Option<u64>], now: u64, interval: u64, max: usize) -> Vec<usize> {
    let mut due: Vec<(u64, usize)> = last
        .iter()
        .enumerate()
        .filter_map(|(i, l)| match l {
            None => Some((0, i)),
            Some(t) if now.saturating_sub(*t) >= interval => Some((*t + 1, i)),
            _ => None,
        })
        .collect();
    due.sort();
    due.into_iter().take(max).map(|(_, i)| i).collect()
}

/// The steps of a connection, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    /// Preparing and waiting for the peer's code / the short code's answer.
    Find = 0,
    /// Exchanging addresses and punching through NAT.
    Connect = 1,
    /// Connected, the first measurements of the channel are not in yet.
    Check = 2,
    /// Pictures are flowing.
    Picture = 3,
}

pub const STEPS: [Step; 4] = [Step::Find, Step::Connect, Step::Check, Step::Picture];

impl Step {
    pub fn label(self) -> &'static str {
        match self {
            Step::Find => "Подготовка",
            Step::Connect => "Соединяемся",
            Step::Check => "Проверяем канал",
            Step::Picture => "Идёт картинка",
        }
    }

    pub fn hint(self, host: bool, room: bool) -> &'static str {
        match (self, host, room) {
            (Step::Find, true, true) => "Ждём, пока зритель введёт короткий код",
            (Step::Find, true, false) => "Передайте код зрителю и вставьте его ответ",
            (Step::Find, false, true) => "Ищем трансляцию по короткому коду",
            (Step::Find, false, false) => "Проверяем код трансляции",
            (Step::Connect, ..) => "Пробиваем прямой путь между компьютерами",
            (Step::Check, ..) => "Связь есть, смотрим, как идёт сеть",
            (Step::Picture, ..) => "Всё работает",
        }
    }
}

/// Which step a session is at. Derived from what the child process already reports (its status lines,
/// `@CODE`, `@CONNECTED`, the first `@STATS`): nothing new is needed on the wire.
/// `host`: the side that shows the screen. `have_code`: a `@CODE` came (for the host it is its own code, for
/// the viewer it is the answer code, which it produces only after it has taken the host's code).
pub fn conn_step(host: bool, status: &str, have_code: bool, connected: bool, has_stats: bool) -> Step {
    if connected {
        return if has_stats { Step::Picture } else { Step::Check };
    }
    if status.starts_with("Соединяюсь") || status.starts_with("Жду, пока код ответа") {
        return Step::Connect;
    }
    if !host && have_code {
        return Step::Connect;
    }
    Step::Find
}

/// A clear Russian message for a failure: (what happened, what to do). The raw text stays in "Подробности".
pub fn friendly_error(raw: &str) -> (&'static str, &'static str) {
    let r = raw.to_lowercase();
    let has = |s: &str| r.contains(s);
    if has("не удалось соединиться") {
        (
            "Не получилось соединиться",
            "Проверьте, что на другой стороне программа не закрыта, коды свежие (из этого запуска) и файрвол не блокирует screenshare. Потом нажмите «Повторить».",
        )
    } else if has("сервер подключения недоступен") || has("нет связи с сервером") || has("не удалось запустить curl") {
        (
            "Нет связи с сервером подключения",
            "Проверьте интернет и нажмите «Повторить». Если не поможет — откройте «Дополнительно» и используйте обычные длинные коды.",
        )
    } else if has("устарел") {
        ("Код устарел", "Короткий код живёт 15 минут. Нажмите «Повторить» — будет новый.")
    } else if has("повреждён") || has("в коде нет адресов") {
        ("Код не подошёл", "Скопируйте его целиком, без пробелов и лишних символов, и попробуйте снова.")
    } else if has("другой версии") {
        ("У вас разные версии программы", "Установите свежую версию на обоих компьютерах.")
    } else if has("ffmpeg") {
        ("Не удалось подготовить видеокомпоненты", "Закройте программу и запустите снова. Если повторится — проверьте, что диск не заполнен.")
    } else if has("захват") || has("монитор") || has("duplicat") {
        ("Не получилось захватить экран", "Выберите другой монитор или окно. Игры в «настоящем» полноэкранном режиме иногда не захватываются.")
    } else if has("остановлено") || has("интерфейс закрыт") || has("ввод закрыт") {
        ("Подключение остановлено", "Нажмите «Повторить», когда будете готовы.")
    } else {
        ("Что-то пошло не так", "Нажмите «Повторить». Если повторится — откройте «Подробности» и пришлите текст ошибки разработчику.")
    }
}

// ---------------------------------------------------------------- look: theme, accent, settings (0.9.0)

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Accent {
    Blue,
    Purple,
    Green,
}

impl Accent {
    pub const ALL: [Accent; 3] = [Accent::Blue, Accent::Purple, Accent::Green];
    pub fn key(self) -> &'static str {
        match self {
            Accent::Blue => "blue",
            Accent::Purple => "purple",
            Accent::Green => "green",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Accent::Blue => "Синий",
            Accent::Purple => "Фиолетовый",
            Accent::Green => "Зелёный",
        }
    }
    pub fn parse(s: Option<&str>) -> Accent {
        match s.map(str::trim) {
            Some("purple") => Accent::Purple,
            Some("green") => Accent::Green,
            _ => Accent::Blue,
        }
    }
}

/// Everything about the look that is kept in gui.ini.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Look {
    pub light: bool,
    pub accent: Accent,
    /// Animations on (off = every change is instant and nothing repaints by itself).
    pub anim: bool,
    /// Short sound cues on connect / disconnect / error.
    pub sounds: bool,
    /// Windows 11 glass (Mica) backdrop; takes effect at the next start.
    pub glass: bool,
}

impl Default for Look {
    fn default() -> Look {
        Look { light: false, accent: Accent::Blue, anim: true, sounds: true, glass: false }
    }
}

impl Look {
    /// Reads the keys `theme`, `accent`, `anim`, `sounds`, `glass`; anything unknown means the default.
    pub fn from_ini(text: &str) -> Look {
        let get = |t: &str, k: &str| crate::volume::ini_get(t, k).map(str::to_string);
        let d = Look::default();
        let flag = |k: &str, def: bool| match get(text, k).as_deref().map(str::trim) {
            Some("1") => true,
            Some("0") => false,
            _ => def,
        };
        Look {
            light: get(text, "theme").as_deref().map(str::trim) == Some("light"),
            accent: Accent::parse(get(text, "accent").as_deref()),
            anim: flag("anim", d.anim),
            sounds: flag("sounds", d.sounds),
            glass: flag("glass", d.glass),
        }
    }
}

/// 0..1 -> 0..1, fast start and a soft stop (page and card transitions).
pub fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t)
}

/// Progress of a transition that started `elapsed_ms` ago and lasts `dur_ms` (a zero duration is "done").
pub fn progress(elapsed_ms: u128, dur_ms: u128) -> f32 {
    if dur_ms == 0 {
        1.0
    } else {
        (elapsed_ms as f32 / dur_ms as f32).clamp(0.0, 1.0)
    }
}

// ---------------------------------------------------------------- the 60-second quality graph

/// How many one-second samples the graph keeps.
pub const GRAPH_LEN: usize = 60;

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct GraphPoint {
    pub mbps: f32,
    /// None = unknown (no ping answer yet).
    pub rtt_ms: Option<f32>,
    pub loss_pct: f32,
}

/// Parses the payload of `@GRAPH <Мбит/с>|<пинг мс или ->|<потери %>`.
pub fn parse_graph(payload: &str) -> Option<GraphPoint> {
    let mut it = payload.trim().split('|');
    let mbps: f32 = it.next()?.trim().parse().ok()?;
    let rtt = it.next()?.trim();
    let loss: f32 = it.next()?.trim().parse().ok()?;
    let rtt_ms = if rtt == "-" { None } else { Some(rtt.parse::<f32>().ok()?) };
    if !mbps.is_finite() || !loss.is_finite() || mbps < 0.0 || loss < 0.0 || rtt_ms.is_some_and(|r| !r.is_finite() || r < 0.0) {
        return None;
    }
    Some(GraphPoint { mbps, rtt_ms, loss_pct: loss.min(100.0) })
}

/// The last `GRAPH_LEN` samples, oldest first.
#[derive(Default, Debug)]
pub struct Graph {
    pub pts: std::collections::VecDeque<GraphPoint>,
}

impl Graph {
    pub fn push(&mut self, p: GraphPoint) {
        if self.pts.len() >= GRAPH_LEN {
            self.pts.pop_front();
        }
        self.pts.push_back(p);
    }
}

/// A round upper bound for a graph axis: at least `floor`, then 1/2/5 x 10^n above the biggest value.
pub fn nice_max(v: f32, floor: f32) -> f32 {
    let v = if v.is_finite() { v.max(floor) } else { floor };
    let mag = 10f32.powf(v.log10().floor());
    for k in [1.0, 2.0, 5.0, 10.0] {
        if v <= k * mag * 1.0001 {
            return k * mag;
        }
    }
    10.0 * mag
}

// ---------------------------------------------------------------- how the player bar fades

/// Bar opacity 0..1 `idle_ms` after the last mouse move: fully shown for `SHOW_MS`, then fades out over `FADE_MS`.
pub const BAR_SHOW_MS: u128 = 2600;
pub const BAR_FADE_MS: u128 = 400;

pub fn bar_opacity(idle_ms: u128) -> f32 {
    if idle_ms <= BAR_SHOW_MS {
        1.0
    } else if idle_ms >= BAR_SHOW_MS + BAR_FADE_MS {
        0.0
    } else {
        1.0 - (idle_ms - BAR_SHOW_MS) as f32 / BAR_FADE_MS as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_aspect_and_never_enlarges() {
        assert_eq!(fit(1920, 1080, 256, 144), (256, 144));
        assert_eq!(fit(1000, 1000, 256, 144), (144, 144));
        assert_eq!(fit(100, 50, 256, 144), (100, 50));
        assert_eq!(fit(0, 10, 256, 144), (1, 1));
        let (w, h) = fit(3840, 600, 256, 144);
        assert!(w <= 256 && h >= 1);
    }

    #[test]
    fn pixels_are_swapped_and_opaque() {
        let mut p = [1u8, 2, 3, 0, 10, 20, 30, 7];
        bgra_to_opaque_rgba(&mut p);
        assert_eq!(p, [3, 2, 1, 255, 30, 20, 10, 255]);
        assert!(all_black(&[0, 0, 0, 0, 0, 0, 0, 255]));
        assert!(!all_black(&[0, 0, 0, 0, 0, 1, 0, 255]));
    }

    #[test]
    fn batch_takes_new_first_then_stalest_and_respects_limits() {
        let last = [Some(1000), None, Some(100), Some(3900), None];
        assert_eq!(pick_batch(&last, 4000, 3000, 2), vec![1, 4]);
        assert_eq!(pick_batch(&last, 4000, 3000, 10), vec![1, 4, 2, 0]);
        assert_eq!(pick_batch(&last[3..4], 4000, 3000, 2), Vec::<usize>::new());
        assert_eq!(pick_batch(&[], 0, 3000, 2), Vec::<usize>::new());
    }

    #[test]
    fn steps_follow_the_real_status_lines() {
        // host
        assert_eq!(conn_step(true, "Запускаю...", false, false, false), Step::Find);
        assert_eq!(conn_step(true, "Короткий код опубликован — жду зрителя...", true, false, false), Step::Find);
        assert_eq!(conn_step(true, "Соединяюсь...", true, false, false), Step::Connect);
        assert_eq!(conn_step(true, "Подключено! Идёт трансляция.", true, true, false), Step::Check);
        assert_eq!(conn_step(true, "Подключено! Идёт трансляция.", true, true, true), Step::Picture);
        // viewer
        assert_eq!(conn_step(false, "Определяю адрес в интернете...", false, false, false), Step::Find);
        assert_eq!(conn_step(false, "Ищу трансляцию по короткому коду...", false, false, false), Step::Find);
        assert_eq!(conn_step(false, "Жду, пока код ответа введут на стороне показа...", true, false, false), Step::Connect);
        assert_eq!(conn_step(false, "Подключено! Открываю окно просмотра.", true, true, false), Step::Check);
        assert!(Step::Find < Step::Connect && Step::Check < Step::Picture);
        assert_eq!(STEPS.len(), 4);
        for s in STEPS {
            assert!(!s.label().is_empty() && !s.hint(true, true).is_empty() && !s.hint(false, false).is_empty());
        }
    }

    #[test]
    fn real_errors_get_clear_texts() {
        let (t, _) = friendly_error("не удалось соединиться за 30 с. Проверьте, что коды свежие");
        assert_eq!(t, "Не получилось соединиться");
        assert_eq!(friendly_error("сервер подключения недоступен: curl: (6)").0, "Нет связи с сервером подключения");
        assert_eq!(friendly_error("короткий код устарел (15 минут) — создайте новый").0, "Код устарел");
        assert_eq!(friendly_error("код от другой версии программы — обновитесь оба").0, "У вас разные версии программы");
        assert_eq!(friendly_error("не удалось подготовить FFmpeg: x").0, "Не удалось подготовить видеокомпоненты");
        assert_eq!(friendly_error("что-то совсем новое").0, "Что-то пошло не так");
        for raw in ["", "остановлено", "захват окна: x"] {
            let (t, h) = friendly_error(raw);
            assert!(!t.is_empty() && !h.is_empty());
        }
    }

    #[test]
    fn look_defaults_and_parsing() {
        assert_eq!(Look::from_ini(""), Look::default());
        let l = Look::from_ini("theme=light\naccent=purple\nanim=0\nsounds=0\nglass=1\n");
        assert_eq!(l, Look { light: true, accent: Accent::Purple, anim: false, sounds: false, glass: true });
        let l = Look::from_ini("theme=weird\naccent=pink\nanim=maybe\n");
        assert_eq!(l, Look::default());
        for a in Accent::ALL {
            assert_eq!(Accent::parse(Some(a.key())), a);
        }
    }

    #[test]
    fn easing_is_monotone_and_bounded() {
        assert_eq!(ease_out(0.0), 0.0);
        assert_eq!(ease_out(1.0), 1.0);
        assert_eq!(ease_out(7.0), 1.0);
        assert_eq!(ease_out(-1.0), 0.0);
        let mut prev = 0.0;
        for i in 0..=20 {
            let v = ease_out(i as f32 / 20.0);
            assert!(v >= prev);
            prev = v;
        }
        assert_eq!(progress(10, 0), 1.0);
        assert_eq!(progress(50, 100), 0.5);
        assert_eq!(progress(500, 100), 1.0);
    }

    #[test]
    fn graph_parse_and_ring() {
        assert_eq!(parse_graph("12.5|34|0.5"), Some(GraphPoint { mbps: 12.5, rtt_ms: Some(34.0), loss_pct: 0.5 }));
        assert_eq!(parse_graph("3|-|0"), Some(GraphPoint { mbps: 3.0, rtt_ms: None, loss_pct: 0.0 }));
        assert_eq!(parse_graph("x|1|2"), None);
        assert_eq!(parse_graph("1|2"), None);
        assert_eq!(parse_graph("-1|2|3"), None);
        assert_eq!(parse_graph("NaN|2|3"), None);
        assert_eq!(parse_graph("1|2|500").unwrap().loss_pct, 100.0);
        let mut g = Graph::default();
        for i in 0..100 {
            g.push(GraphPoint { mbps: i as f32, rtt_ms: None, loss_pct: 0.0 });
        }
        assert_eq!(g.pts.len(), GRAPH_LEN);
        assert_eq!(g.pts.front().unwrap().mbps, 40.0);
        assert_eq!(g.pts.back().unwrap().mbps, 99.0);
    }

    #[test]
    fn nice_axis_values() {
        assert_eq!(nice_max(0.0, 10.0), 10.0);
        assert_eq!(nice_max(23.0, 10.0), 50.0);
        assert_eq!(nice_max(61.0, 10.0), 100.0);
        assert_eq!(nice_max(7.0, 5.0), 10.0);
        assert_eq!(nice_max(0.3, 1.0), 1.0);
        assert_eq!(nice_max(f32::NAN, 5.0), 5.0);
        assert_eq!(nice_max(100.0, 10.0), 100.0);
    }

    #[test]
    fn bar_fades_after_idle() {
        assert_eq!(bar_opacity(0), 1.0);
        assert_eq!(bar_opacity(BAR_SHOW_MS), 1.0);
        let mid = bar_opacity(BAR_SHOW_MS + BAR_FADE_MS / 2);
        assert!((mid - 0.5).abs() < 0.01);
        assert_eq!(bar_opacity(BAR_SHOW_MS + BAR_FADE_MS), 0.0);
        assert_eq!(bar_opacity(60_000), 0.0);
    }
}
