// Compiles the vendored libopus (../../vendor/opus) so the Opus audio logic can be tested on any OS.
use std::path::PathBuf;

fn main() {
    let root = PathBuf::from("../../vendor/opus");
    let mut b = cc::Build::new();
    for dir in ["celt", "silk", "silk/float", "src"] {
        for e in std::fs::read_dir(root.join(dir)).unwrap().flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "c") {
                b.file(&p);
            }
        }
    }
    for inc in [".", "include", "celt", "silk", "silk/float", "src"] {
        b.include(root.join(inc));
    }
    b.define("HAVE_CONFIG_H", None).warnings(false).opt_level(3).compile("opus");
    println!("cargo:rerun-if-changed=../../vendor/opus");
}
