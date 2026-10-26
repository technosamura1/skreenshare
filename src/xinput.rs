// XInput (gamepads on the viewer), loaded at run time so the program still starts where the DLL is missing
// (Windows N editions) and nothing new shows up in the import table. Windows only; NOT tested with a real
// gamepad by the author. `SS_FAKE_PAD=1` makes up one moving gamepad, to try the screens without hardware.
use crate::inputproto::{btn, PadState};
use windows::core::{s, w, PCSTR};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct XState {
    packet: u32,
    buttons: u16,
    lt: u8,
    rt: u8,
    lx: i16,
    ly: i16,
    rx: i16,
    ry: i16,
}

#[repr(C)]
struct XVibration {
    left: u16,
    right: u16,
}

type GetState = unsafe extern "system" fn(u32, *mut XState) -> u32;
type SetState = unsafe extern "system" fn(u32, *mut XVibration) -> u32;

pub struct XInput {
    get: GetState,
    set: SetState,
    fake: bool,
    t0: std::time::Instant,
}

impl XInput {
    pub fn load() -> Option<XInput> {
        let fake = std::env::var("SS_FAKE_PAD").is_ok_and(|v| v == "1");
        unsafe {
            let m = LoadLibraryW(w!("xinput1_4.dll")).or_else(|_| LoadLibraryW(w!("xinput1_3.dll"))).or_else(|_| LoadLibraryW(w!("xinput9_1_0.dll"))).ok()?;
            // Ordinal 100 is XInputGetStateEx: the same as XInputGetState, plus the Guide button.
            let get = GetProcAddress(m, PCSTR(100usize as *const u8)).or_else(|| GetProcAddress(m, s!("XInputGetState")))?;
            let set = GetProcAddress(m, s!("XInputSetState"))?;
            Some(XInput { get: std::mem::transmute::<_, GetState>(get), set: std::mem::transmute::<_, SetState>(set), fake, t0: std::time::Instant::now() })
        }
    }

    /// The state of controller `slot` (0..3), None when nothing is connected there.
    pub fn state(&self, slot: usize) -> Option<PadState> {
        if self.fake {
            return (slot == 0).then(|| fake_state(self.t0.elapsed().as_secs_f32()));
        }
        let mut st = XState::default();
        let r = unsafe { (self.get)(slot as u32, &mut st) };
        (r == 0).then_some(PadState { buttons: st.buttons, lt: st.lt, rt: st.rt, lx: st.lx, ly: st.ly, rx: st.rx, ry: st.ry })
    }

    /// Motor speeds 0..=255 each (large = low frequency, small = high frequency).
    pub fn vibrate(&self, slot: usize, large: u8, small: u8) {
        if self.fake {
            return;
        }
        let mut v = XVibration { left: large as u16 * 257, right: small as u16 * 257 };
        unsafe {
            (self.set)(slot as u32, &mut v);
        }
    }
}

fn fake_state(t: f32) -> PadState {
    let a = t * 1.5;
    let pressed = ((t * 1.2) as u32) % 8;
    let buttons = [btn::A, btn::B, btn::X, btn::Y, btn::LB, btn::RB, btn::UP, btn::START][pressed as usize];
    PadState {
        buttons,
        lt: ((a.sin() * 0.5 + 0.5) * 255.0) as u8,
        rt: ((a.cos() * 0.5 + 0.5) * 255.0) as u8,
        lx: (a.cos() * 30000.0) as i16,
        ly: (a.sin() * 30000.0) as i16,
        rx: ((a * 0.5).sin() * 20000.0) as i16,
        ry: ((a * 0.7).cos() * 20000.0) as i16,
    }
}
