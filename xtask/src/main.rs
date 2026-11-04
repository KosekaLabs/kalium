//! Developer tasks: `cargo xtask build|run|boot-test|wasm|test|clippy|fmt-check`.
//! See docs/prototype-plan.md.
//!
//! Only `std` is used here. Every subcommand is a function returning the
//! process exit code; `main` only dispatches.

use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const KERNEL_TARGET: &str = "aarch64-unknown-none";
const KERNEL_ELF: &str = "target/aarch64-unknown-none/release/kalium";
const QEMU: &str = "qemu-system-aarch64";
const QEMU_TIMEOUT: Duration = Duration::from_secs(60);
/// Exit code reported when QEMU is killed after the timeout (like coreutils).
const EXIT_TIMEOUT: i32 = 124;

/// The line the boot test expects on the console.
const BOOT_TEST_MARKER: &str = "kalium: 10 ticks";

fn main() {
    let args: Vec<String> = env::args().skip(1).filter(|a| a != "--").collect();
    let (cmd, rest) = match args.split_first() {
        Some((cmd, rest)) => (cmd.as_str(), rest),
        None => {
            usage();
            process::exit(2);
        }
    };
    let root = workspace_root();
    let code = match cmd {
        "build" => build(&root),
        "run" => run(&root, rest),
        "boot-test" => boot_test(&root),
        "wasm" => wasm(&root),
        "test" | "clippy" | "fmt-check" => not_implemented(cmd),
        "help" | "-h" | "--help" => {
            usage();
            0
        }
        other => {
            eprintln!("xtask: unknown subcommand `{other}`");
            usage();
            2
        }
    };
    process::exit(code);
}

fn usage() {
    eprintln!(
        "usage: cargo xtask <command>\n\
         \n\
         commands:\n\
         \x20 build                 build the kernel for {KERNEL_TARGET} (release)\n\
         \x20 run [-- --boot-only]  build, then boot the kernel in QEMU (60 s timeout)\n\
         \x20 boot-test             build, boot in QEMU, require `{BOOT_TEST_MARKER}` and exit 0\n\
         \x20 wasm                  build the SDK example modules into target/wasm/\n\
         \x20 test                  host tests, build, boot-test (not implemented)\n\
         \x20 clippy                lint the default members (not implemented)\n\
         \x20 fmt-check             check formatting (not implemented)"
    );
}

fn not_implemented(cmd: &str) -> i32 {
    eprintln!("xtask: `{cmd}` is not implemented");
    2
}

/// The workspace root: the parent of this crate's directory.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the workspace root")
        .to_path_buf()
}

fn cargo() -> Command {
    Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
}

fn status_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

/// The SDK example modules in boot-image order (`init=0, ping=1, pong=2,
/// rt=3, fair=4, faulty=5`); the kernel's `build.rs` embeds
/// `target/wasm/<name>.wasm` in this order.
const WASM_MODULES: [&str; 6] = ["init", "ping", "pong", "rt", "fair", "faulty"];
const WASM_TARGET: &str = "wasm32-unknown-unknown";
/// Where the SDK workspace builds (kept apart from the kernel's target dir
/// so the two workspaces never share a lock or a profile).
const WASM_BUILD_DIR: &str = "target/wasm-build";
/// Where the modules are copied for the kernel's `build.rs`.
const WASM_OUT_DIR: &str = "target/wasm";

/// `cargo build --release --target wasm32-unknown-unknown --manifest-path
/// sdk/Cargo.toml --target-dir target/wasm-build`, then copy each example's
/// `.wasm` to `target/wasm/<name>.wasm`.
fn wasm(root: &Path) -> i32 {
    eprintln!("xtask: building SDK examples for {WASM_TARGET}");
    let status = cargo()
        .current_dir(root)
        .args([
            "build",
            "--release",
            "--target",
            WASM_TARGET,
            "--manifest-path",
            "sdk/Cargo.toml",
            "--target-dir",
            WASM_BUILD_DIR,
        ])
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => {
            eprintln!("xtask: SDK build failed");
            return status_code(s);
        }
        Err(e) => {
            eprintln!("xtask: failed to run cargo: {e}");
            return 1;
        }
    }

    let out_dir = root.join(WASM_OUT_DIR);
    if let Err(e) = fs::create_dir_all(&out_dir) {
        eprintln!("xtask: cannot create {}: {e}", out_dir.display());
        return 1;
    }
    let built = root.join(WASM_BUILD_DIR).join(WASM_TARGET).join("release");
    for (index, name) in WASM_MODULES.iter().enumerate() {
        let src = built.join(format!("{name}.wasm"));
        let dst = out_dir.join(format!("{name}.wasm"));
        if let Err(e) = fs::copy(&src, &dst) {
            eprintln!(
                "xtask: cannot copy {} to {}: {e}",
                src.display(),
                dst.display()
            );
            return 1;
        }
        let size = fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
        println!("module {index}: {} ({size} bytes)", dst.display());
    }
    0
}

/// `cargo build -p kalium-kernel --target aarch64-unknown-none --release`.
fn build(root: &Path) -> i32 {
    eprintln!("xtask: building kernel for {KERNEL_TARGET}");
    let status = cargo()
        .current_dir(root)
        .args([
            "build",
            "-p",
            "kalium-kernel",
            "--target",
            KERNEL_TARGET,
            "--release",
        ])
        .status();
    match status {
        Ok(s) if s.success() => 0,
        Ok(s) => {
            eprintln!("xtask: kernel build failed");
            status_code(s)
        }
        Err(e) => {
            eprintln!("xtask: failed to run cargo: {e}");
            1
        }
    }
}

/// The QEMU command line from the plan, plus `extra` arguments.
fn qemu_command(root: &Path, extra: &[&str]) -> Command {
    let mut cmd = Command::new(QEMU);
    cmd.current_dir(root).args([
        "-M",
        "virt,gic-version=2",
        "-cpu",
        "cortex-a72",
        "-m",
        "256M",
        "-nographic",
        "-semihosting-config",
        "enable=on,target=native",
        "-no-reboot",
        "-kernel",
        KERNEL_ELF,
    ]);
    cmd.args(extra);
    cmd
}

/// Wait for `child` with a timeout, killing it if it runs too long.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> io::Result<Option<ExitStatus>> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Build, then run QEMU interactively. `--boot-only` is passed to the kernel
/// as `-append boot-only`.
fn run(root: &Path, args: &[String]) -> i32 {
    let mut extra: Vec<&str> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--boot-only" => extra.extend(["-append", "boot-only"]),
            other => {
                eprintln!("xtask: unknown argument to `run`: `{other}`");
                return 2;
            }
        }
    }

    let code = build(root);
    if code != 0 {
        return code;
    }

    eprintln!(
        "xtask: starting QEMU (timeout {} s)",
        QEMU_TIMEOUT.as_secs()
    );
    let mut child = match qemu_command(root, &extra).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("xtask: failed to start {QEMU}: {e}");
            return 1;
        }
    };
    match wait_with_timeout(&mut child, QEMU_TIMEOUT) {
        Ok(Some(status)) => status_code(status),
        Ok(None) => {
            eprintln!(
                "xtask: QEMU timed out after {} s, killed",
                QEMU_TIMEOUT.as_secs()
            );
            EXIT_TIMEOUT
        }
        Err(e) => {
            eprintln!("xtask: waiting for QEMU failed: {e}");
            1
        }
    }
}

/// Read a pipe to the end on a helper thread.
fn drain<R: Read + Send + 'static>(reader: Option<R>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = reader {
            let _ = r.read_to_end(&mut buf);
        }
        buf
    })
}

/// Build, boot in QEMU with captured output, and check the boot-test marker
/// and exit code.
fn boot_test(root: &Path) -> i32 {
    let code = build(root);
    if code != 0 {
        return code;
    }

    eprintln!(
        "xtask: boot-test: starting QEMU (timeout {} s)",
        QEMU_TIMEOUT.as_secs()
    );
    let mut child = match qemu_command(root, &["-append", "boot-only"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("xtask: failed to start {QEMU}: {e}");
            return 1;
        }
    };
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let outcome = wait_with_timeout(&mut child, QEMU_TIMEOUT);
    let out = String::from_utf8_lossy(&stdout.join().unwrap_or_default()).into_owned();
    let err = String::from_utf8_lossy(&stderr.join().unwrap_or_default()).into_owned();

    println!("---- kernel console ----");
    print!("{out}");
    if !out.ends_with('\n') {
        println!();
    }
    println!("------------------------");
    if !err.trim().is_empty() {
        eprintln!("---- qemu stderr ----");
        eprint!("{err}");
        if !err.ends_with('\n') {
            eprintln!();
        }
        eprintln!("---------------------");
    }

    let status = match outcome {
        Ok(Some(status)) => status,
        Ok(None) => {
            eprintln!(
                "boot-test: FAIL: QEMU timed out after {} s",
                QEMU_TIMEOUT.as_secs()
            );
            return EXIT_TIMEOUT;
        }
        Err(e) => {
            eprintln!("boot-test: FAIL: waiting for QEMU failed: {e}");
            return 1;
        }
    };

    let mut failures = Vec::new();
    if !out.contains(BOOT_TEST_MARKER) {
        failures.push(format!(
            "console output does not contain `{BOOT_TEST_MARKER}`"
        ));
    }
    if status.code() != Some(0) {
        failures.push(format!("QEMU exited with {status}, expected code 0"));
    }
    if failures.is_empty() {
        println!("boot-test: PASS");
        0
    } else {
        for f in &failures {
            eprintln!("boot-test: FAIL: {f}");
        }
        1
    }
}
