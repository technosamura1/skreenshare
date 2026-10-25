// System audio capture that leaves out one process tree (Windows "process loopback", Win10 2004+/Win11).
// We exclude the voice chat app (Discord): otherwise the friend's own voice, played on the host,
// would be captured and streamed back to them as an echo.
use anyhow::{bail, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use windows::core::{implement, Interface, Ref, HRESULT, IUnknown};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, BLOB, COINIT_MULTITHREADED};
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use std::sync::atomic::Ordering as AtomicOrdering;
use windows::Win32::System::Variant::VT_BLOB;

pub const RATE: u32 = 48_000;

/// Voice chat apps whose sound must never go into the stream.
const VOICE_APPS: &[(&str, &str)] = &[
    ("discord.exe", "Discord"),
    ("discordptb.exe", "Discord PTB"),
    ("discordcanary.exe", "Discord Canary"),
    ("vesktop.exe", "Vesktop"),
];

/// Root process of a running voice app (its children play the actual audio), with a display name.
pub fn find_voice_app() -> Option<(u32, &'static str)> {
    let procs = list_processes();
    for (exe, label) in VOICE_APPS {
        let pids: Vec<(u32, u32)> =
            procs.iter().filter(|p| p.2.eq_ignore_ascii_case(exe)).map(|p| (p.0, p.1)).collect();
        if let Some(root) = pids.iter().find(|(_, parent)| !pids.iter().any(|(pid, _)| pid == parent)) {
            return Some((root.0, label));
        }
    }
    None
}

fn list_processes() -> Vec<(u32, u32, String)> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let end = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            out.push((e.th32ProcessID, e.th32ParentProcessID, String::from_utf16_lossy(&e.szExeFile[..end])));
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

// windows-rs makes #[implement] objects agile (free-threaded) by itself, as the audio API requires.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(HANDLE);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _op: Ref<IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        unsafe { SetEvent(self.0) }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    /// Everything except this process tree.
    Exclude,
    /// Only this process tree (used by the self-check).
    Include,
}

pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Starts capturing 48 kHz stereo i16. `on_audio(samples, rate)` runs on the capture thread.
pub fn start(pid: u32, mode: Mode, mut on_audio: impl FnMut(&[i16], u32) + Send + 'static) -> Result<Capture> {
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
    let stop2 = stop.clone();
    let thread = std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        crate::audio::boost_audio_thread();
        let r = run(pid, mode, &stop2, &ready_tx, &mut on_audio);
        if let Err(e) = r {
            let _ = ready_tx.send(Err(e));
        }
        CoUninitialize();
    });
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Capture { stop, thread: Some(thread) }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => bail!("поток захвата звука завершился"),
    }
}

unsafe fn activate(pid: u32, mode: Mode) -> Result<IAudioClient> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: match mode {
                    Mode::Exclude => PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
                    Mode::Include => PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        },
    };
    // windows-rs gives PROPVARIANT a Drop that calls PropVariantClear, which would CoTaskMemFree our
    // stack-allocated blob and corrupt the heap. The blob isn't ours to free, so never drop it.
    let mut pv = std::mem::ManuallyDrop::new(PROPVARIANT::default());
    unsafe {
        let inner = &mut *pv.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob = BLOB {
            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: &params as *const _ as *mut u8,
        };
        let done = CreateEventW(None, false, false, None)?;
        let handler: IActivateAudioInterfaceCompletionHandler = Activated(done).into();
        let op = ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*pv), &handler)
            .context("ActivateAudioInterfaceAsync (нужна Windows 10 2004+ или Windows 11)")?;
        WaitForSingleObject(done, 5000);
        let _ = CloseHandle(done);
        let mut hr = HRESULT(0);
        let mut unk: Option<IUnknown> = None;
        op.GetActivateResult(&mut hr, &mut unk)?;
        hr.ok().context("захват звука по процессам недоступен")?;
        Ok(unk.context("пустой результат активации")?.cast()?)
    }
}

unsafe fn run(
    pid: u32,
    mode: Mode,
    stop: &AtomicBool,
    ready: &mpsc::Sender<Result<()>>,
    on_audio: &mut impl FnMut(&[i16], u32),
) -> Result<()> {
    unsafe {
        let client = activate(pid, mode)?;
        // Process loopback has no mix format of its own; we ask for exactly what we send over the wire.
        let fmt = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: 2,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * 4,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            1_000_000, // 100 ms of slack: the capture thread may be late by a few periods without losing samples
            0,
            &fmt,
            None,
        )?;
        let cap: IAudioCaptureClient = client.GetService()?;
        let ev = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(ev)?;
        client.Start()?;
        let _ = ready.send(Ok(()));
        let mut silence: Vec<i16> = Vec::new();
        let mut fixed: Vec<i16> = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            WaitForSingleObject(ev, 100);
            loop {
                let n = cap.GetNextPacketSize()?;
                if n == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    silence.clear();
                    silence.resize(frames as usize * 2, 0);
                    on_audio(&silence, RATE);
                } else if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                    // Samples were lost before this buffer: ramp it in so the splice does not click.
                    crate::audio::CAPTURE_GLITCHES.fetch_add(1, AtomicOrdering::Relaxed);
                    fixed.clear();
                    fixed.extend_from_slice(std::slice::from_raw_parts(data as *const i16, frames as usize * 2));
                    let ramp = (RATE as usize / 1000).min(fixed.len() / 2);
                    for i in 0..ramp {
                        let g = i as f32 / ramp as f32;
                        fixed[i * 2] = (fixed[i * 2] as f32 * g) as i16;
                        fixed[i * 2 + 1] = (fixed[i * 2 + 1] as f32 * g) as i16;
                    }
                    on_audio(&fixed, RATE);
                } else {
                    on_audio(std::slice::from_raw_parts(data as *const i16, frames as usize * 2), RATE);
                }
                cap.ReleaseBuffer(frames)?;
            }
        }
        let _ = client.Stop();
        let _ = CloseHandle(ev);
    }
    Ok(())
}

/// Self-check: plays a quiet tone from this process and verifies that Exclude mode really drops it
/// (and Include mode really hears it). Returns (tone level when excluded, tone level when included).
pub fn self_check() -> Result<(f64, f64)> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let dev = cpal::default_host().default_output_device().context("нет устройства вывода звука")?;
    let cfg = dev.default_output_config()?;
    let ch = cfg.channels() as usize;
    let sr = cfg.sample_rate() as f64;
    let mut phase = 0.0f64;
    let stream = dev.build_output_stream::<f32, _, _>(
        cfg.config(),
        move |out: &mut [f32], _| {
            for f in out.chunks_mut(ch) {
                let v = (phase * std::f64::consts::TAU).sin() as f32 * 0.03;
                phase = (phase + 440.0 / sr).fract();
                f.fill(v);
            }
        },
        |e| eprintln!("тон: {e}"),
        None,
    )?;
    stream.play()?;
    std::thread::sleep(std::time::Duration::from_millis(300));
    let me = std::process::id();
    let measure = |mode| -> Result<f64> {
        let buf = Arc::new(std::sync::Mutex::new(Vec::<i16>::new()));
        let b2 = buf.clone();
        let c = start(me, mode, move |s, _| b2.lock().unwrap().extend(s.iter().step_by(2)))?;
        std::thread::sleep(std::time::Duration::from_millis(1200));
        drop(c);
        let left = buf.lock().unwrap();
        Ok(goertzel(&left, 440.0, RATE as f64))
    };
    let excluded = measure(Mode::Exclude)?;
    let included = measure(Mode::Include)?;
    Ok((excluded, included))
}

/// Normalized amplitude of one frequency in the signal (0..1).
fn goertzel(x: &[i16], freq: f64, rate: f64) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    let w = std::f64::consts::TAU * freq / rate;
    let c = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &v in x {
        let s0 = v as f64 / 32768.0 + c * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - c * s1 * s2;
    2.0 * power.sqrt() / x.len() as f64
}
