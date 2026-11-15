// Stream resolution (v0.15.0). The picture used to be encoded at the screen's own size, so a 4K screen meant a 4K
// encode even when the viewer's window was 1440p: the streamer's video card choked next to a game. Now both sides
// have a say: the streamer sets a ceiling, the viewer picks what to watch (like the quality menu of a video site),
// and "auto" follows the viewer's window. The host shrinks the picture before encoding; changing the choice only
// restarts the encoder with a keyframe, the stream itself goes on.

/// "Как на экране": no limit.
pub const NATIVE: u16 = u16::MAX;
/// "Авто".
pub const AUTO: u16 = 0;
/// The heights "auto" chooses between.
const STEPS: [u16; 3] = [720, 1080, 1440];

/// The choices of both menus, in order: value and label.
pub const CHOICES: [(u16, &str); 5] = [(AUTO, "Авто"), (720, "720p"), (1080, "1080p"), (1440, "1440p"), (NATIVE, "Исходное")];

pub fn label(v: u16) -> &'static str {
    CHOICES.iter().find(|c| c.0 == v).map_or("Авто", |c| c.1)
}

/// A value read from a setting or a packet, snapped to one of the choices (anything unknown is "auto").
pub fn sanitize(v: u32) -> u16 {
    CHOICES.iter().map(|c| c.0).find(|&c| c as u32 == v).unwrap_or(AUTO)
}

/// What "auto" means for a viewer whose window is `win_h` pixels high (0 = not known yet): the smallest step that
/// still fills the window, 1440p at most.
pub fn auto_height(win_h: u16) -> u16 {
    if win_h == 0 {
        return 1440;
    }
    STEPS.into_iter().find(|&s| s >= win_h).unwrap_or(1440)
}

/// The height limit the host encodes with (`NATIVE` = none): the viewer's pick (or "auto" for its window), never
/// above the streamer's own ceiling.
pub fn effective(host: u16, viewer: u16, win_h: u16) -> u16 {
    let ceiling = if host == AUTO { NATIVE } else { host };
    let wish = if viewer == AUTO { auto_height(win_h) } else { viewer };
    ceiling.min(wish)
}

/// Size of a `w`x`h` picture under the height `limit`: unchanged when it fits, otherwise shrunk keeping the aspect
/// ratio. Both sides stay even (the encoders need that).
pub fn fit(w: u32, h: u32, limit: u16) -> (u32, u32) {
    let limit = limit as u32;
    if limit == NATIVE as u32 || h <= limit || w == 0 || h == 0 {
        return (w, h);
    }
    let nh = limit & !1;
    let nw = ((w as u64 * nh as u64 + h as u64 / 2) / h as u64) as u32 & !1;
    (nw.max(2), nh.max(2))
}

/// The most "Авто" sends for a picture `h` pixels high, Mbit/s. HEVC needs far less than the old flat 60 for a
/// clean picture, and tens of megabits of UDP to one address for minutes on end is what home routers and
/// providers tend to throttle or drop.
pub fn auto_mbps(h: u32) -> f64 {
    match h {
        0..=720 => 8.0,
        721..=1080 => 14.0,
        1081..=1440 => 22.0,
        _ => 35.0,
    }
}

/// Viewer -> host: ver u8 = 1 | wanted height u16 LE | window height u16 LE.
pub fn encode(viewer: u16, win_h: u16, out: &mut Vec<u8>) {
    out.push(1);
    out.extend_from_slice(&viewer.to_le_bytes());
    out.extend_from_slice(&win_h.to_le_bytes());
}

pub fn decode(body: &[u8]) -> Option<(u16, u16)> {
    if body.len() < 5 || body[0] != 1 {
        return None;
    }
    let v = u16::from_le_bytes([body[1], body[2]]);
    Some((sanitize(v as u32), u16::from_le_bytes([body[3], body[4]])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_follows_the_window_and_stops_at_1440() {
        assert_eq!(auto_height(0), 1440);
        assert_eq!(auto_height(600), 720);
        assert_eq!(auto_height(720), 720);
        assert_eq!(auto_height(900), 1080);
        assert_eq!(auto_height(1351), 1440);
        assert_eq!(auto_height(2160), 1440);
    }

    #[test]
    fn the_streamer_sets_the_ceiling_and_the_viewer_picks_under_it() {
        // both on auto: the viewer's window decides
        assert_eq!(effective(AUTO, AUTO, 1440), 1440);
        assert_eq!(effective(AUTO, AUTO, 1000), 1080);
        // the viewer asks for the original and the streamer does not mind
        assert_eq!(effective(AUTO, NATIVE, 1000), NATIVE);
        // the streamer's ceiling wins over a bigger wish, a smaller wish is honoured
        assert_eq!(effective(1080, NATIVE, 1440), 1080);
        assert_eq!(effective(1080, 720, 1440), 720);
        assert_eq!(effective(NATIVE, AUTO, 800), 1080);
        assert_eq!(effective(720, AUTO, 2000), 720);
    }

    #[test]
    fn fit_keeps_the_aspect_and_even_sides_and_never_enlarges() {
        assert_eq!(fit(3840, 2160, 1440), (2560, 1440));
        assert_eq!(fit(3840, 2160, 1080), (1920, 1080));
        assert_eq!(fit(3840, 2160, 720), (1280, 720));
        assert_eq!(fit(3440, 1440, 1080), (2580, 1080));
        assert_eq!(fit(2560, 1440, 1440), (2560, 1440));
        assert_eq!(fit(1920, 1080, 1440), (1920, 1080));
        assert_eq!(fit(3840, 2160, NATIVE), (3840, 2160));
        let (w, h) = fit(1366, 768, 720);
        assert!(w % 2 == 0 && h % 2 == 0 && h == 720, "{w}x{h}");
    }

    #[test]
    fn auto_bitrate_grows_with_the_picture() {
        assert_eq!(auto_mbps(720), 8.0);
        assert_eq!(auto_mbps(1080), 14.0);
        assert_eq!(auto_mbps(1440), 22.0);
        assert_eq!(auto_mbps(2160), 35.0);
        assert!(auto_mbps(600) <= auto_mbps(900) && auto_mbps(900) <= auto_mbps(1200) && auto_mbps(1200) <= auto_mbps(1600));
    }

    #[test]
    fn the_packet_roundtrips_and_junk_is_refused() {
        let mut b = Vec::new();
        encode(1080, 1351, &mut b);
        assert_eq!(decode(&b), Some((1080, 1351)));
        b.clear();
        encode(NATIVE, 0, &mut b);
        assert_eq!(decode(&b), Some((NATIVE, 0)));
        assert_eq!(decode(&[1, 2]), None);
        assert_eq!(decode(&[2, 0, 0, 0, 0]), None);
        // an unknown height reads as "auto"
        assert_eq!(decode(&[1, 0x39, 0x05, 0, 0]), Some((AUTO, 0)));
        assert_eq!(sanitize(1080), 1080);
        assert_eq!(sanitize(999), AUTO);
        assert_eq!(label(NATIVE), "Исходное");
    }
}
