// Copies the FFmpeg DLLs next to the built exe so `cargo run` works without touching PATH.
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    let Ok(ffmpeg_dir) = std::env::var("FFMPEG_DIR") else { return };
    let bin = PathBuf::from(ffmpeg_dir).join("bin");
    // OUT_DIR = target/<profile>/build/<pkg>-<hash>/out
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let Some(profile_dir) = out.ancestors().nth(3) else { return };
    let Ok(entries) = std::fs::read_dir(&bin) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("dll")) {
            let dst = profile_dir.join(p.file_name().unwrap());
            if !dst.exists() {
                let _ = std::fs::copy(&p, &dst);
            }
        }
    }
}
