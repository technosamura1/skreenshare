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
    /// Preparing and waiting for the friend's code / the short code's answer.
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
            Step::Find => "Ищем друга",
            Step::Connect => "Соединяемся",
            Step::Check => "Проверяем канал",
            Step::Picture => "Идёт картинка",
        }
    }

    pub fn hint(self, host: bool, room: bool) -> &'static str {
        match (self, host, room) {
            (Step::Find, true, true) => "Ждём, пока друг введёт короткий код",
            (Step::Find, true, false) => "Отправь другу код и вставь его ответ",
            (Step::Find, false, true) => "Ищем друга по короткому коду",
            (Step::Find, false, false) => "Проверяем код друга",
            (Step::Connect, ..) => "Пробиваем прямой путь между компьютерами",
            (Step::Check, ..) => "Связь есть, смотрим, как идёт сеть",
            (Step::Picture, ..) => "Всё работает",
        }
    }
}

/// Which step a session is at. Derived from what the child process already reports (its status lines,
/// `@CODE`, `@CONNECTED`, the first `@STATS`): nothing new is needed on the wire.
/// `host`: the side that shows the screen. `have_code`: a `@CODE` came (for the host it is its own code, for
/// the viewer it is the answer code, which it produces only after it has taken the friend's code).
pub fn conn_step(host: bool, status: &str, have_code: bool, connected: bool, has_stats: bool) -> Step {
    if connected {
        return if has_stats { Step::Picture } else { Step::Check };
    }
    if status.starts_with("Соединяюсь") || status.starts_with("Жду, пока друг вставит") {
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
            "Не получилось соединиться с другом",
            "Проверь, что друг не закрыл свою программу, коды свежие (из этого запуска) и файрвол не блокирует screenshare. Потом нажми «Повторить».",
        )
    } else if has("сервер подключения недоступен") || has("нет связи с сервером") || has("не удалось запустить curl") {
        (
            "Нет связи с сервером подключения",
            "Проверь интернет и нажми «Повторить». Если не поможет — открой «Дополнительно» и используй обычные длинные коды.",
        )
    } else if has("устарел") {
        ("Код устарел", "Короткий код живёт 15 минут. Нажми «Повторить» — будет новый.")
    } else if has("повреждён") || has("в коде нет адресов") {
        ("Код не подошёл", "Скопируй его целиком, без пробелов и лишних символов, и попробуй снова.")
    } else if has("другой версии") {
        ("У вас разные версии программы", "Скачай свежую версию и у себя, и у друга.")
    } else if has("ffmpeg") {
        ("Не удалось подготовить видеокомпоненты", "Закрой программу и запусти снова. Если повторится — проверь, что диск не заполнен.")
    } else if has("захват") || has("монитор") || has("duplicat") {
        ("Не получилось захватить экран", "Выбери другой монитор или окно. Игры в «настоящем» полноэкранном режиме иногда не захватываются.")
    } else if has("остановлено") || has("интерфейс закрыт") || has("ввод закрыт") {
        ("Подключение остановлено", "Нажми «Повторить», когда будешь готов.")
    } else {
        ("Что-то пошло не так", "Нажми «Повторить». Если повторится — открой «Подробности» и пришли текст ошибки разработчику.")
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
        assert_eq!(conn_step(true, "Короткий код опубликован — жду друга...", true, false, false), Step::Find);
        assert_eq!(conn_step(true, "Соединяюсь...", true, false, false), Step::Connect);
        assert_eq!(conn_step(true, "Подключено! Идёт трансляция.", true, true, false), Step::Check);
        assert_eq!(conn_step(true, "Подключено! Идёт трансляция.", true, true, true), Step::Picture);
        // viewer
        assert_eq!(conn_step(false, "Определяю адрес в интернете...", false, false, false), Step::Find);
        assert_eq!(conn_step(false, "Ищу друга по короткому коду...", false, false, false), Step::Find);
        assert_eq!(conn_step(false, "Жду, пока друг вставит код ответа...", true, false, false), Step::Connect);
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
        assert_eq!(t, "Не получилось соединиться с другом");
        assert_eq!(friendly_error("сервер подключения недоступен: curl: (6)").0, "Нет связи с сервером подключения");
        assert_eq!(friendly_error("короткий код устарел (15 минут) — создай новый").0, "Код устарел");
        assert_eq!(friendly_error("код от другой версии программы — обновитесь оба").0, "У вас разные версии программы");
        assert_eq!(friendly_error("не удалось подготовить FFmpeg: x").0, "Не удалось подготовить видеокомпоненты");
        assert_eq!(friendly_error("что-то совсем новое").0, "Что-то пошло не так");
        for raw in ["", "остановлено", "захват окна: x"] {
            let (t, h) = friendly_error(raw);
            assert!(!t.is_empty() && !h.is_empty());
        }
    }
}
