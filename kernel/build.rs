//! Embeds the boot image: the SDK example modules built by `cargo xtask wasm`
//! into `target/wasm/<name>.wasm`, in the fixed order the plan prescribes
//! (`init=0, ping=1, pong=2, rt=3, fair=4, faulty=5`; `init` hardcodes the
//! indices of the others).
//!
//! The generated `$OUT_DIR/boot_image.rs` defines
//! `BOOT_IMAGE: [BootModule; N]` with the module name and an
//! `include_bytes!` of each file. A missing file is a build error that tells
//! the user to run `cargo xtask wasm`. The directory can be overridden with
//! `KALIUM_WASM_DIR` (absolute path).
//!
//! With the `boot-only` feature nothing is embedded.

use std::env;
use std::fs;
use std::path::PathBuf;

/// Boot image order. Keep in sync with `xtask` and `sdk/examples/init`.
const MODULES: [&str; 6] = ["init", "ping", "pong", "rt", "fair", "faulty"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=link.ld");
    println!("cargo:rerun-if-env-changed=KALIUM_WASM_DIR");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let dest = out_dir.join("boot_image.rs");

    if env::var_os("CARGO_FEATURE_BOOT_ONLY").is_some() {
        fs::write(&dest, "pub const BOOT_IMAGE: [BootModule; 0] = [];\n")
            .expect("write boot_image.rs");
        return;
    }

    let wasm_dir = match env::var_os("KALIUM_WASM_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => {
            let manifest = PathBuf::from(
                env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo"),
            );
            manifest
                .parent()
                .expect("kernel/ lives one level below the workspace root")
                .join("target")
                .join("wasm")
        }
    };

    let mut entries = String::new();
    let mut missing = Vec::new();
    for name in MODULES {
        let path = wasm_dir.join(format!("{name}.wasm"));
        println!("cargo:rerun-if-changed={}", path.display());
        if !path.is_file() {
            missing.push(path.display().to_string());
            continue;
        }
        entries.push_str(&format!(
            "    BootModule {{ name: {name:?}, bytes: include_bytes!({:?}) }},\n",
            path.display().to_string()
        ));
    }
    if !missing.is_empty() {
        eprintln!();
        eprintln!("error: the boot image is incomplete; these modules are missing:");
        for m in &missing {
            eprintln!("    {m}");
        }
        eprintln!();
        eprintln!("Build the SDK examples first with `cargo xtask wasm` (or run");
        eprintln!("`cargo xtask build`, which does both), then rebuild the kernel.");
        eprintln!();
        panic!("boot image modules missing; run `cargo xtask wasm`");
    }

    let generated = format!(
        "pub const BOOT_IMAGE: [BootModule; {}] = [\n{}];\n",
        MODULES.len(),
        entries
    );
    fs::write(&dest, generated).expect("write boot_image.rs");
}
