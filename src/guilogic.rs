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

/// How the ViGEmBus installer run ended, in terms the host page can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriverEnd {
    /// The driver answers, no restart needed.
    Ready,
    /// Installed (or half-installed): Windows wants a restart before the driver works.
    NeedReboot,
    /// The user said "No" to the UAC prompt or closed the installer.
    Cancelled,
    /// The installer failed with this exit code.
    Failed(u32),
}

/// Exit codes of the (Advanced Installer / MSI) setup: 0 ok, 3010 restart required, 1641 restart started,
/// 1602 and 1223 the user cancelled.
pub const EXIT_REBOOT: u32 = 3010;
pub const EXIT_REBOOT_STARTED: u32 = 1641;
pub const EXIT_USER_CANCEL: u32 = 1602;
pub const EXIT_UAC_CANCEL: u32 = 1223;

/// After a silent run that failed for some reason other than a cancel, the installer is started once more with its
/// normal window, so the user sees what is wrong.
pub fn retry_with_window(exit: u32) -> bool {
    !matches!(exit, 0 | EXIT_REBOOT | EXIT_REBOOT_STARTED | EXIT_USER_CANCEL | EXIT_UAC_CANCEL)
}

/// Final verdict from the installer's exit code and a fresh "does the driver answer" check.
pub fn classify_driver_install(exit: u32, driver_answers: bool) -> DriverEnd {
    match exit {
        EXIT_UAC_CANCEL | EXIT_USER_CANCEL => DriverEnd::Cancelled,
        EXIT_REBOOT | EXIT_REBOOT_STARTED => DriverEnd::NeedReboot,
        0 if driver_answers => DriverEnd::Ready,
        // Setup is happy but the device is not there yet: the usual sign of a pending restart.
        0 => DriverEnd::NeedReboot,
        // Some failures still leave a working driver (e.g. it was installed meanwhile).
        _ if driver_answers => DriverEnd::Ready,
        c => DriverEnd::Failed(c),
    }
}

pub fn driver_end_text(e: DriverEnd) -> String {
    match e {
        DriverEnd::Ready => "Драйвер геймпада установлен. Перезагрузка не нужна.".into(),
        DriverEnd::NeedReboot => "Драйвер установлен, но Windows просит перезагрузку. Перезагрузите компьютер, потом откройте программу снова.".into(),
        DriverEnd::Cancelled => "Установка отменена: окно Windows с запросом разрешения закрыто или отклонено.".into(),
        DriverEnd::Failed(c) => format!("Установщик драйвера завершился с ошибкой (код {c}). Попробуйте ещё раз или скачайте драйвер вручную."),
    }
}

/// What the host's control thread reports (`@CTRL on=1 pad=1 kbm=1 vigem=1 pads=2 active=0 revoked=0`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct CtrlStatus {
    pub on: bool,
    pub pad: bool,
    pub kbm: bool,
    pub vigem: bool,
    pub pads: u8,
    /// The viewer is controlling right now (input arrived within the last moments).
    pub active: bool,
    /// The owner took control away with the emergency hotkey.
    pub revoked: bool,
}

pub fn parse_ctrl(text: &str) -> Option<CtrlStatus> {
    let mut c = CtrlStatus::default();
    let mut seen = 0;
    for w in text.split_whitespace() {
        let Some((k, v)) = w.split_once('=') else { continue };
        let b = v == "1";
        match k {
            "on" => c.on = b,
            "pad" => c.pad = b,
            "kbm" => c.kbm = b,
            "vigem" => c.vigem = b,
            "pads" => c.pads = v.parse().ok()?,
            "active" => c.active = b,
            "revoked" => c.revoked = b,
            _ => continue,
        }
        seen += 1;
    }
    (seen >= 5).then_some(c)
}

/// The line the GUI sends to the host process when the owner changes the switches.
pub fn control_line(on: bool, pad: bool, kbm: bool) -> String {
    format!("CONTROL on={} pad={} kbm={}", on as u8, pad as u8, kbm as u8)
}

/// The viewer window wakes itself on a ~30 fps clock only while something on it animates: an overlay line or
/// panel, the bar fading, or the bar still to be wiped once more. Otherwise it sleeps until an event.
pub fn viewer_ticks(overlay: bool, bar_visible: bool, bar_to_wipe: bool) -> bool {
    overlay || bar_visible || bar_to_wipe
}

/// The two tabs that start a session of their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Host,
    Join,
}

/// What is running that stands in the way of starting something on a tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Occupied {
    Host,
    Join,
    Test,
}

impl Occupied {
    pub fn title(self) -> &'static str {
        match self {
            Occupied::Host => "Сейчас идёт показ вашего экрана",
            Occupied::Join => "Сейчас идёт просмотр",
            Occupied::Test => "Сейчас идёт проверка на этом ПК",
        }
    }
    /// "перейти к ..." / "остановить ..." button texts.
    pub fn open_text(self) -> &'static str {
        match self {
            Occupied::Host => "Перейти к показу",
            Occupied::Join => "Перейти к просмотру",
            Occupied::Test => "Перейти к проверке",
        }
    }
    pub fn stop_text(self) -> &'static str {
        match self {
            Occupied::Host => "Остановить показ",
            Occupied::Join => "Остановить просмотр",
            Occupied::Test => "Остановить проверку",
        }
    }
}

/// The sidebar items are always clickable and every session keeps running in the background. A tab whose action
/// cannot start right now (one session at a time: showing, watching or the self-test) still opens, and says what
/// is running instead of its form. None: the tab shows its own page.
pub fn tab_busy(tab: Tab, host_on: bool, join_on: bool, test_on: bool) -> Option<Occupied> {
    if test_on {
        return Some(Occupied::Test);
    }
    match tab {
        Tab::Host if join_on => Some(Occupied::Join),
        Tab::Join if host_on => Some(Occupied::Host),
        _ => None,
    }
}

/// One half of the self-test as the window sees it.
#[derive(Clone, Copy, Debug, Default)]
pub struct TestHalf {
    pub ended: bool,
    pub connected: bool,
    pub error: bool,
}

/// The self-test is over for good and the page goes back to "Запустить проверку" by itself: one half was stopped
/// (the red button on the viewer's bar, its window closed, the sharing side ended) after the two had been
/// connected, and neither half reports an error (an error stays on screen to be read). Without this the main
/// button kept saying "Остановить" over two dead sessions and needed a second click.
pub fn test_finished(host: TestHalf, client: TestHalf) -> bool {
    if host.error || client.error {
        return false;
    }
    (host.ended && host.connected) || (client.ended && client.connected)
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

    #[test]
    fn ctrl_status_parses_and_rejects_junk() {
        let c = parse_ctrl("on=1 pad=1 kbm=0 vigem=1 pads=2 active=1 revoked=0").unwrap();
        assert!(c.on && c.pad && !c.kbm && c.vigem && c.active && !c.revoked);
        assert_eq!(c.pads, 2);
        assert!(parse_ctrl("hello").is_none());
        assert!(parse_ctrl("on=1 pad=1").is_none());
        assert!(parse_ctrl("on=1 pad=1 kbm=1 vigem=0 pads=x").is_none());
        assert!(parse_ctrl("on=0 pad=1 kbm=1 vigem=0 pads=0 active=0 revoked=1 future=7").unwrap().revoked);
    }

    #[test]
    fn control_line_matches_what_the_host_reads() {
        assert_eq!(control_line(true, true, false), "CONTROL on=1 pad=1 kbm=0");
        assert_eq!(control_line(false, false, false), "CONTROL on=0 pad=0 kbm=0");
    }

    #[test]
    fn driver_install_verdicts() {
        assert_eq!(classify_driver_install(0, true), DriverEnd::Ready);
        assert_eq!(classify_driver_install(0, false), DriverEnd::NeedReboot);
        assert_eq!(classify_driver_install(3010, false), DriverEnd::NeedReboot);
        assert_eq!(classify_driver_install(3010, true), DriverEnd::NeedReboot);
        assert_eq!(classify_driver_install(1641, false), DriverEnd::NeedReboot);
        assert_eq!(classify_driver_install(1223, false), DriverEnd::Cancelled);
        assert_eq!(classify_driver_install(1602, true), DriverEnd::Cancelled);
        assert_eq!(classify_driver_install(1603, false), DriverEnd::Failed(1603));
        assert_eq!(classify_driver_install(1603, true), DriverEnd::Ready);
    }

    #[test]
    fn driver_retry_only_on_real_failure() {
        for ok in [0, 3010, 1641, 1602, 1223] {
            assert!(!retry_with_window(ok), "{ok}");
        }
        for bad in [1, 2, 87, 1603, 1618, 1625] {
            assert!(retry_with_window(bad), "{bad}");
        }
        assert!(driver_end_text(DriverEnd::NeedReboot).contains("перезагруз"));
        assert!(!driver_end_text(DriverEnd::Failed(5)).contains("друг"));
    }

    #[test]
    fn self_test_resets_by_itself_when_either_half_is_stopped() {
        let up = TestHalf { ended: false, connected: true, error: false };
        let gone = TestHalf { ended: true, connected: true, error: false };
        // running: stays
        assert!(!test_finished(up, up));
        // the viewer's stop button / closed window, or the sharing side ending: back to the start at once
        assert!(test_finished(up, gone));
        assert!(test_finished(gone, up));
        assert!(test_finished(gone, gone));
        // still connecting: nothing to reset
        let start = TestHalf::default();
        assert!(!test_finished(start, start));
        // a failure stays on screen so it can be read
        let bad = TestHalf { ended: true, connected: true, error: true };
        assert!(!test_finished(up, bad));
        assert!(!test_finished(bad, gone));
        // never connected and ended without a word: left visible (not silently swallowed)
        let dead = TestHalf { ended: true, connected: false, error: false };
        assert!(!test_finished(dead, start));
    }

    #[test]
    fn viewer_sleeps_when_nothing_animates() {
        assert!(!viewer_ticks(false, false, false));
        assert!(viewer_ticks(true, false, false));
        assert!(viewer_ticks(false, true, false));
        assert!(viewer_ticks(false, false, true));
    }

    #[test]
    fn every_tab_opens_in_every_state_and_says_what_is_running() {
        for bits in 0..8u8 {
            let (h, j, t) = (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0);
            for tab in [Tab::Host, Tab::Join] {
                let b = tab_busy(tab, h, j, t);
                // A tab never reports its own session as the obstacle.
                match (tab, b) {
                    (Tab::Host, Some(Occupied::Host)) | (Tab::Join, Some(Occupied::Join)) => panic!("{tab:?} {h} {j} {t}"),
                    _ => {}
                }
                // Nothing running: the plain page.
                if !(h || j || t) {
                    assert_eq!(b, None);
                }
                // Whatever is named is really running.
                match b {
                    Some(Occupied::Host) => assert!(h),
                    Some(Occupied::Join) => assert!(j),
                    Some(Occupied::Test) => assert!(t),
                    None => {}
                }
            }
        }
        // One's own session shows its own page, the other tab explains.
        assert_eq!(tab_busy(Tab::Host, true, false, false), None);
        assert_eq!(tab_busy(Tab::Join, true, false, false), Some(Occupied::Host));
        assert_eq!(tab_busy(Tab::Host, false, true, false), Some(Occupied::Join));
        assert_eq!(tab_busy(Tab::Join, false, true, false), None);
        assert_eq!(tab_busy(Tab::Host, false, false, true), Some(Occupied::Test));
        assert_eq!(tab_busy(Tab::Join, false, false, true), Some(Occupied::Test));
        for o in [Occupied::Host, Occupied::Join, Occupied::Test] {
            assert!(!o.title().is_empty() && !o.open_text().is_empty() && !o.stop_text().is_empty());
            assert!(!o.title().contains("друг"));
        }
    }
}
