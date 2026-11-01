// The FFmpeg DLLs travel inside the exe (see build.rs). Before the first FFmpeg call we make sure they are
// unpacked to %LOCALAPPDATA%\screenshare\ffmpeg-<id> and point the DLL search there; the imports are
// delay-loaded, so Windows only looks for them at that first call.
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::PathBuf;
use windows::core::PCWSTR;
use windows::Win32::System::LibraryLoader::{
    AddDllDirectory, LoadLibraryExW, SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
    LOAD_LIBRARY_SEARCH_SYSTEM32, LOAD_LIBRARY_SEARCH_USER_DIRS,
};

static BUNDLE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ffmpeg_bundle.bin"));

struct Entry<'a> {
    name: &'a str,
    size: u64,
    packed: &'a [u8],
    hash: &'a [u8],
}

fn entries() -> Vec<Entry<'static>> {
    let mut v = Vec::new();
    let mut b = BUNDLE;
    while b.len() >= 2 {
        let n = u16::from_le_bytes([b[0], b[1]]) as usize;
        let name = std::str::from_utf8(&b[2..2 + n]).unwrap();
        let size = u64::from_le_bytes(b[2 + n..10 + n].try_into().unwrap());
        let plen = u64::from_le_bytes(b[10 + n..18 + n].try_into().unwrap()) as usize;
        v.push(Entry {
            name,
            size,
            hash: &b[18 + n..50 + n],
            packed: &b[50 + n..50 + n + plen],
        });
        b = &b[50 + n + plen..];
    }
    v
}

/// Exclude the current directory and PATH from dynamic DLL loading.
pub fn secure_search() -> Result<()> {
    unsafe {
        SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32 | LOAD_LIBRARY_SEARCH_USER_DIRS)?;
    }
    Ok(())
}

fn valid_file(path: &std::path::Path, size: u64, hash: &[u8]) -> bool {
    let Ok(data) = std::fs::read(path) else {
        return false;
    };
    data.len() as u64 == size && Sha256::digest(&data).as_slice() == hash
}

pub fn ensure_ffmpeg() -> Result<PathBuf> {
    secure_search()?;
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base
        .join("screenshare")
        .join(concat!("ffmpeg-", env!("SS_BUNDLE_ID")));
    std::fs::create_dir_all(&dir).with_context(|| format!("не создать {}", dir.display()))?;
    for e in entries() {
        let path = dir.join(e.name);
        if valid_file(&path, e.size, e.hash) {
            continue;
        }
        let data =
            zstd::bulk::decompress(e.packed, e.size as usize).context("распаковка FFmpeg")?;
        if data.len() as u64 != e.size || Sha256::digest(&data).as_slice() != e.hash {
            bail!("нарушена целостность встроенной библиотеки {}", e.name);
        }
        let mut random = [0u8; 16];
        getrandom::fill(&mut random)
            .map_err(|e| anyhow::anyhow!("создание временного файла FFmpeg: {e}"))?;
        let tmp = dir.join(format!(
            "{}.{}.tmp",
            e.name,
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(&data)?;
        drop(f);
        let renamed = std::fs::rename(&tmp, &path);
        if renamed.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        // A concurrent process may have won the rename. Verify its file, fail closed otherwise.
        if !valid_file(&path, e.size, e.hash) {
            bail!("библиотека {} повреждена или заблокирована; закрывайте программу перед обновлением", path.display());
        }
    }
    let wide = dir.as_os_str().encode_wide_nul();
    if unsafe { AddDllDirectory(PCWSTR(wide.as_ptr())) }.is_null() {
        return Err(std::io::Error::last_os_error()).context("безопасная загрузка FFmpeg");
    }
    // Pin our verified modules by absolute path before delay imports can search for them.
    let es = entries();
    for prefix in ["avutil-", "swresample-", "swscale-", "avcodec-"] {
        let e = es
            .iter()
            .find(|e| e.name.starts_with(prefix))
            .context("неполный комплект FFmpeg")?;
        let path = dir.join(e.name);
        let wide = path.as_os_str().encode_wide_nul();
        unsafe {
            LoadLibraryExW(
                PCWSTR(wide.as_ptr()),
                None,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )?;
        }
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_same_size_replacement_does_not_pass_integrity_check() {
        let path =
            std::env::temp_dir().join(format!("screenshare-hash-{}.bin", std::process::id()));
        let hash = Sha256::digest(b"good");
        std::fs::write(&path, b"good").unwrap();
        assert!(valid_file(&path, 4, &hash));
        std::fs::write(&path, b"evil").unwrap();
        assert!(!valid_file(&path, 4, &hash));
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn every_embedded_dll_has_a_matching_content_hash() {
        let es = entries();
        assert_eq!(es.len(), 4);
        for e in es {
            let data = zstd::bulk::decompress(e.packed, e.size as usize).unwrap();
            assert_eq!(data.len() as u64, e.size);
            assert_eq!(Sha256::digest(data).as_slice(), e.hash);
        }
    }
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
