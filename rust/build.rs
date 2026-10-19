// The saved view counts only for the exact code that derives state, the same
// rule as Python's REDUCER: editing any of these files retires old snapshots.
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

fn main() {
    let files = ["src/json.rs", "src/pyfmt.rs", "src/state.rs", "src/view.rs", "src/board.rs", "src/snapshot.rs"];
    let mut hasher = Sha256::new();
    for file in files {
        println!("cargo:rerun-if-changed={file}");
        hasher.update(fs::read(file).unwrap_or_default());
    }
    let digest = hasher.finalize();
    let bytes: Vec<String> = digest.iter().map(|b| format!("{b}")).collect();
    let out = Path::new(&std::env::var("OUT_DIR").unwrap()).join("reducer.rs");
    fs::write(out, format!("pub const REDUCER: [u8; 32] = [{}];\n", bytes.join(", "))).unwrap();
}
