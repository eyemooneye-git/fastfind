//! Embed the application icon and version info into the Windows exe.
//!
//! The resource is compiled with the Windows SDK's `rc.exe` (already required
//! for MSVC builds) and handed to the linker as an extra input.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=assets/fastfind.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let Some(rc) = find_rc() else {
        println!("cargo:warning=rc.exe not found; building without the exe icon");
        return;
    };

    // Name the .res after its content so changing the icon relinks the exe.
    let ico = std::fs::read("assets/fastfind.ico").unwrap_or_default();
    let script = std::fs::read("app.rc").unwrap_or_default();
    let manifest = std::fs::read("app.manifest").unwrap_or_default();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in ico.iter().chain(script.iter()).chain(manifest.iter()) {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let res = out_dir.join(format!("fastfind-{hash:016x}.res"));
    if !res.exists() {
        let status = Command::new(&rc)
            .arg("/nologo")
            .arg("/fo")
            .arg(&res)
            .arg("app.rc")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .expect("failed to run rc.exe");
        if !status.success() {
            // Never leave a partial .res behind that a later build would trust.
            let _ = std::fs::remove_file(&res);
            println!("cargo:warning=rc.exe failed; building without the exe icon");
            return;
        }
    }
    println!("cargo:rustc-link-arg-bins={}", res.display());
}

/// Newest `...\Windows Kits\10\bin\<version>\x64\rc.exe`.
fn find_rc() -> Option<PathBuf> {
    let kits = std::env::var_os("ProgramFiles(x86)")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files (x86)"));
    let bin = kits.join("Windows Kits").join("10").join("bin");
    let mut best: Option<(Vec<u32>, PathBuf)> = None;
    for entry in std::fs::read_dir(bin).ok()?.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let version: Vec<u32> = entry
            .file_name()
            .to_string_lossy()
            .split('.')
            .filter_map(|s| s.parse().ok())
            .collect();
        let exe = dir.join("x64").join("rc.exe");
        if exe.is_file() && best.as_ref().map_or(true, |(v, _)| version > *v) {
            best = Some((version, exe));
        }
    }
    best.map(|(_, p)| p)
}
