// The FFmpeg DLLs travel inside the exe (see build.rs). Before the first FFmpeg call we make sure they are
// unpacked to %LOCALAPPDATA%\screenshare\ffmpeg-<id> and point the DLL search there; the imports are
// delay-loaded, so Windows only looks for them at that first call.
use anyhow::{Context, Result};
use std::path::PathBuf;
use windows::core::PCWSTR;
use windows::Win32::System::LibraryLoader::SetDllDirectoryW;

static BUNDLE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ffmpeg_bundle.bin"));

struct Entry<'a> {
    name: &'a str,
    size: u64,
    packed: &'a [u8],
}

fn entries() -> Vec<Entry<'static>> {
    let mut v = Vec::new();
    let mut b = BUNDLE;
    while b.len() >= 2 {
        let n = u16::from_le_bytes([b[0], b[1]]) as usize;
        let name = std::str::from_utf8(&b[2..2 + n]).unwrap();
        let size = u64::from_le_bytes(b[2 + n..10 + n].try_into().unwrap());
        let plen = u64::from_le_bytes(b[10 + n..18 + n].try_into().unwrap()) as usize;
        v.push(Entry { name, size, packed: &b[18 + n..18 + n + plen] });
        b = &b[18 + n + plen..];
    }
    v
}

pub fn ensure_ffmpeg() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let dir = base.join("screenshare").join(concat!("ffmpeg-", env!("SS_BUNDLE_ID")));
    std::fs::create_dir_all(&dir).with_context(|| format!("не создать {}", dir.display()))?;
    for e in entries() {
        let path = dir.join(e.name);
        if std::fs::metadata(&path).is_ok_and(|m| m.len() == e.size) {
            continue;
        }
        let data = zstd::bulk::decompress(e.packed, e.size as usize).context("распаковка FFmpeg")?;
        // Unique temp name + rename: the GUI may start two copies of us at the same moment.
        let tmp = dir.join(format!("{}.{}.tmp", e.name, std::process::id()));
        std::fs::write(&tmp, &data).with_context(|| format!("не записать {}", tmp.display()))?;
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp); // another copy won the race; its file is identical
        }
    }
    let wide: Vec<u16> = dir.as_os_str().encode_wide_nul();
    unsafe { SetDllDirectoryW(PCWSTR(wide.as_ptr()))? };
    Ok(dir)
}

trait EncodeWideNul {
    fn encode_wide_nul(&self) -> Vec<u16>;
}

impl EncodeWideNul for std::ffi::OsStr {
    fn encode_wide_nul(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.encode_wide().chain(Some(0)).collect()
    }
}
