// Packs the FFmpeg DLLs into the exe (zstd-compressed) and marks them delay-loaded, so the program ships
// as a single file: on first start it unpacks them to %LOCALAPPDATA% (see src/bundle.rs).
use std::io::Write;
use std::path::PathBuf;

// avformat is linked by ffmpeg-next but none of its functions end up in the exe, so it isn't shipped.
const NEEDED: &[&str] = &["avcodec-", "avutil-", "swscale-", "swresample-"];
/// Loaded by our own code (directly imported); swresample is only pulled in by avcodec.
const DIRECT: &[&str] = &["avcodec-", "avutil-", "swscale-"];

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    println!("cargo:rerun-if-changed=build.rs");
    let ffmpeg_dir = std::env::var("FFMPEG_DIR").expect("FFMPEG_DIR не задан (см. .cargo/config.toml)");
    let bin = PathBuf::from(ffmpeg_dir).join("bin");
    let mut dlls: Vec<PathBuf> = std::fs::read_dir(&bin)
        .expect("нет FFMPEG_DIR/bin")
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().unwrap().to_string_lossy().to_lowercase();
            n.ends_with(".dll") && NEEDED.iter().any(|pre| n.starts_with(pre))
        })
        .collect();
    dlls.sort();
    // The exe must link against MSVC-format import libs: GNU ones (libav*.dll.a) make the linker ignore
    // /DELAYLOAD, the exe then imports the DLLs statically and does not even start (0.7.1/0.7.2 bug).
    let lib = PathBuf::from(std::env::var("FFMPEG_DIR").unwrap()).join("lib");
    for l in ["avcodec", "avutil", "swscale", "avformat"] {
        assert!(lib.join(format!("{l}.lib")).exists(), "нет {l}.lib в {}: запусти tools/make-import-libs.sh", lib.display());
        assert!(!lib.join(format!("lib{l}.dll.a")).exists(), "lib{l}.dll.a мешает /DELAYLOAD: запусти tools/make-import-libs.sh");
    }
    // ffmpeg-next links avformat although nothing calls it; it is not shipped, so it must never load at startup.
    println!("cargo:rustc-link-arg=/DELAYLOAD:avformat-63.dll");
    assert_eq!(dlls.len(), NEEDED.len(), "не все DLL FFmpeg найдены в {}", bin.display());

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let mut bundle = Vec::new();
    let mut id: u64 = 0xcbf29ce484222325;
    for p in &dlls {
        println!("cargo:rerun-if-changed={}", p.display());
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let data = std::fs::read(p).unwrap();
        let packed = zstd::bulk::compress(&data, 12).unwrap();
        bundle.extend_from_slice(&(name.len() as u16).to_le_bytes());
        bundle.extend_from_slice(name.as_bytes());
        bundle.extend_from_slice(&(data.len() as u64).to_le_bytes());
        bundle.extend_from_slice(&(packed.len() as u64).to_le_bytes());
        bundle.extend_from_slice(&packed);
        // FNV-1a over names and sizes: a new FFmpeg build gets a new cache folder.
        for b in name.bytes().chain(data.len().to_le_bytes()) {
            id = (id ^ b as u64).wrapping_mul(0x100000001b3);
        }
        let lower = name.to_lowercase();
        if DIRECT.iter().any(|pre| lower.starts_with(pre)) {
            println!("cargo:rustc-link-arg=/DELAYLOAD:{name}");
        }
    }
    std::fs::File::create(out.join("ffmpeg_bundle.bin")).unwrap().write_all(&bundle).unwrap();
    println!("cargo:rustc-env=SS_BUNDLE_ID={id:016x}");
    println!("cargo:rustc-link-lib=delayimp");
}
