//! Developer tasks: `cargo xtask build|run|boot-test|selftest|wasm|demos|test|clippy|fmt-check`.
//! See docs/prototype-plan.md and docs/running.md.
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
/// Default QEMU wall-clock limit; `KALIUM_QEMU_TIMEOUT=<seconds>` overrides.
const QEMU_TIMEOUT_SECS: u64 = 120;
/// Exit code reported when QEMU is killed after the timeout (like coreutils).
const EXIT_TIMEOUT: i32 = 124;
/// The kernel's exit code for an unhandled exception in kernel code
/// (`kalium_hal::exceptions::KERNEL_FAULT_EXIT_CODE`); panics and scenario
/// failures exit with 1.
const EXIT_KERNEL_FAULT: i32 = 2;

/// The line the HAL boot test (`--features boot-only`) prints before exiting.
const BOOT_TEST_MARKER: &str = "kalium: 10 ticks";
/// The summary line the HAL self-test (`--features hal-selftest`) prints.
const SELFTEST_PASS_MARKER: &str = "kalium: selftest PASS";
/// QEMU's RAM size (`-m`) from the plan.
const QEMU_MEMORY: &str = "256M";
/// A RAM size that is not a multiple of 2 MiB, for the self-test's second
/// run: the identity map then has to end inside a 2 MiB block (page-mapped
/// tail), and the "past RAM" checks probe the real end of RAM rather than a
/// block boundary.
const SELFTEST_ODD_MEMORY: &str = "129M";

/// The SDK example modules in boot-image order (`init=0, ping=1, pong=2,
/// rt=3, fair=4, faulty=5, park=6, greedy=7, revoked=8, shelf=9, hog=10`);
/// the kernel's `build.rs` embeds `target/wasm/<name>.wasm` in this order.
const WASM_MODULES: [&str; 11] = ["init", "ping", "pong", "rt", "fair", "faulty", "park", "greedy", "revoked", "shelf", "hog"];
/// The WASI demo programs from `demos/`, which follow the SDK modules in
/// the boot image (`hello_std=11, hello_c=12`). `hello_std` is a Rust `std`
/// program built for `wasm32-wasip1`; `hello_c` is a C program compiled with
/// clang and wasi-libc, or taken from `demos/hello-c/prebuilt/` when that
/// toolchain is absent.
const DEMO_MODULES: [&str; 2] = ["hello_std", "hello_c"];
const WASM_TARGET: &str = "wasm32-unknown-unknown";
const WASIP1_TARGET: &str = "wasm32-wasip1";
const DEMOS_MANIFEST: &str = "demos/Cargo.toml";
/// Where the demos workspace builds (its own directory, like the SDK's).
const DEMOS_BUILD_DIR: &str = "target/demos-build";
const HELLO_C_SOURCE: &str = "demos/hello-c/main.c";
const HELLO_C_PREBUILT: &str = "demos/hello-c/prebuilt/hello_c.wasm";
/// Where a wasi-libc sysroot is looked for when `WASI_SYSROOT` is not set:
/// wasi-sdk's layout, then Ubuntu's `wasi-libc` package (headers in
/// `/usr/include/wasm32-wasi`, libraries in `/usr/lib/wasm32-wasi`, so the
/// sysroot is `/usr`).
const WASI_SYSROOT_CANDIDATES: [&str; 5] = [
    "/usr/share/wasi-sysroot",
    "/usr/lib/wasi-libc",
    "/opt/wasi-sdk/share/wasi-sysroot",
    "/usr/local/share/wasi-sysroot",
    "/usr",
];
/// Compilers tried, in order, when `CLANG` is not set.
const CLANG_CANDIDATES: [&str; 6] = ["clang", "clang-20", "clang-19", "clang-18", "clang-17", "clang-16"];
/// Where the SDK workspace builds (kept apart from the kernel's target dir
/// so the two workspaces never share a lock or a profile).
const WASM_BUILD_DIR: &str = "target/wasm-build";
/// Where the modules are copied for the kernel's `build.rs`.
const WASM_OUT_DIR: &str = "target/wasm";
const SDK_MANIFEST: &str = "sdk/Cargo.toml";

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
        "build" => match parse_flags(rest, "build") {
            Some(flags) => build(&root, flags),
            None => 2,
        },
        "run" => match parse_flags(rest, "run") {
            Some(flags) => run(&root, flags),
            None => 2,
        },
        "boot-test" => boot_test(&root),
        "selftest" => selftest(&root),
        "wasm" => wasm(&root),
        "demos" => demos(&root),
        "test" => test(&root),
        "clippy" => clippy(&root),
        "fmt-check" => fmt_check(&root),
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
         \x20 wasm                   build the SDK example modules and the WASI demos into\n\
         \x20                        target/wasm/\n\
         \x20 demos                  build the WASI demo programs (demos/) into target/wasm/:\n\
         \x20                        hello_std with cargo for {WASIP1_TARGET}, hello_c with clang and\n\
         \x20                        wasi-libc (WASI_SYSROOT, CLANG override the search), falling\n\
         \x20                        back to demos/hello-c/prebuilt/ without that toolchain\n\
         \x20 build [--boot-only] [--selftest]\n\
         \x20                        wasm, then the kernel for {KERNEL_TARGET} (release);\n\
         \x20                        --selftest adds the HAL hardening self-test\n\
         \x20 run [--boot-only] [--selftest]\n\
         \x20                        build, then boot the kernel in QEMU\n\
         \x20 boot-test              HAL-only check: build with `boot-only`, boot, require\n\
         \x20                        `{BOOT_TEST_MARKER}` and exit code 0\n\
         \x20 selftest               HAL hardening check: build with `boot-only` and\n\
         \x20                        `hal-selftest`, boot with -m {QEMU_MEMORY} and again with\n\
         \x20                        -m {SELFTEST_ODD_MEMORY} (RAM not a multiple of 2 MiB), require\n\
         \x20                        `{SELFTEST_PASS_MARKER}`, `{BOOT_TEST_MARKER}` and exit code 0\n\
         \x20 test                   host tests, selftest, wasm, build, boot in QEMU and\n\
         \x20                        check the M1 scenario output and exit code\n\
         \x20 clippy                 clippy with -D warnings on the host crates, the kernel\n\
         \x20                        and the SDK\n\
         \x20 fmt-check              cargo fmt --check on the three workspaces, with\n\
         \x20                        {RUSTFMT_CONFIG} (shared with trunk fmt)\n\
         \n\
         KALIUM_QEMU_TIMEOUT=<seconds> overrides the {QEMU_TIMEOUT_SECS} s QEMU limit."
    );
}

/// Which kernel to build: the cargo features of `kalium-kernel`.
#[derive(Clone, Copy, Default)]
struct BuildFlags {
    /// `boot-only`: HAL boot test, no boot image embedded.
    boot_only: bool,
    /// `hal-selftest`: run the HAL hardening self-test after boot.
    selftest: bool,
}

impl BuildFlags {
    fn features(self) -> Vec<&'static str> {
        let mut f = Vec::new();
        if self.boot_only {
            f.push("boot-only");
        }
        if self.selftest {
            f.push("hal-selftest");
        }
        f
    }
}

/// Parses the optional `--boot-only` and `--selftest` flags; `None` on a bad
/// argument.
fn parse_flags(args: &[String], cmd: &str) -> Option<BuildFlags> {
    let mut flags = BuildFlags::default();
    for arg in args {
        match arg.as_str() {
            "--boot-only" => flags.boot_only = true,
            "--selftest" => flags.selftest = true,
            other => {
                eprintln!("xtask: unknown argument to `{cmd}`: `{other}`");
                return None;
            }
        }
    }
    Some(flags)
}

/// What a QEMU exit code means, for failure messages.
fn describe_exit(code: Option<i32>) -> &'static str {
    match code {
        Some(1) => " (scenario failure or kernel panic)",
        Some(EXIT_KERNEL_FAULT) => {
            " (kernel fault: unhandled exception in kernel code, see the `kalium: kernel fault:` report)"
        }
        _ => "",
    }
}

/// The workspace root: the parent of this crate's directory.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask lives one level below the workspace root").to_path_buf()
}

fn cargo() -> Command {
    Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
}

fn status_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

fn qemu_timeout() -> Duration {
    env::var("KALIUM_QEMU_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(QEMU_TIMEOUT_SECS))
}

/// Runs a cargo invocation in `root`, printing what it does. Returns the
/// exit code (0 on success).
fn run_cargo(root: &Path, what: &str, args: &[&str]) -> i32 {
    eprintln!("xtask: {what}");
    eprintln!("xtask:   cargo {}", args.join(" "));
    match cargo().current_dir(root).args(args).status() {
        Ok(s) if s.success() => 0,
        Ok(s) => {
            eprintln!("xtask: {what}: failed");
            status_code(s)
        }
        Err(e) => {
            eprintln!("xtask: failed to run cargo: {e}");
            1
        }
    }
}

/// `cargo build --release --target wasm32-unknown-unknown --manifest-path
/// sdk/Cargo.toml --target-dir target/wasm-build`, then copy each example's
/// `.wasm` to `target/wasm/<name>.wasm`.
fn wasm(root: &Path) -> i32 {
    let code = run_cargo(
        root,
        &format!("building SDK examples for {WASM_TARGET}"),
        &["build", "--release", "--target", WASM_TARGET, "--manifest-path", SDK_MANIFEST, "--target-dir", WASM_BUILD_DIR],
    );
    if code != 0 {
        return code;
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
            eprintln!("xtask: cannot copy {} to {}: {e}", src.display(), dst.display());
            return 1;
        }
        let size = fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
        println!("module {index}: {} ({size} bytes)", dst.display());
    }
    demos(root)
}

/// Whether every boot image module is present in `target/wasm/`.
fn wasm_present(root: &Path) -> bool {
    WASM_MODULES.iter().chain(DEMO_MODULES.iter()).all(|name| root.join(WASM_OUT_DIR).join(format!("{name}.wasm")).is_file())
}

/// Copies a built module into `target/wasm/<name>.wasm` and prints its
/// boot image line.
fn install_module(root: &Path, index: usize, name: &str, src: &Path) -> i32 {
    let out_dir = root.join(WASM_OUT_DIR);
    if let Err(e) = fs::create_dir_all(&out_dir) {
        eprintln!("xtask: cannot create {}: {e}", out_dir.display());
        return 1;
    }
    let dst = out_dir.join(format!("{name}.wasm"));
    if let Err(e) = fs::copy(src, &dst) {
        eprintln!("xtask: cannot copy {} to {}: {e}", src.display(), dst.display());
        return 1;
    }
    let size = fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
    println!("module {index}: {} ({size} bytes)", dst.display());
    0
}

/// Builds the WASI demo programs into `target/wasm/` after the SDK modules:
/// `hello_std` with cargo for `wasm32-wasip1`, `hello_c` with clang and
/// wasi-libc when they are installed (a fresh build is preferred and
/// compared with the checked-in binary), else from `demos/hello-c/prebuilt/`.
fn demos(root: &Path) -> i32 {
    let code = run_cargo(
        root,
        &format!("building WASI demo hello_std for {WASIP1_TARGET}"),
        &["build", "--release", "--target", WASIP1_TARGET, "--manifest-path", DEMOS_MANIFEST, "--target-dir", DEMOS_BUILD_DIR],
    );
    if code != 0 {
        return code;
    }
    let base = WASM_MODULES.len();
    let built = root.join(DEMOS_BUILD_DIR).join(WASIP1_TARGET).join("release").join("hello_std.wasm");
    let code = install_module(root, base, DEMO_MODULES[0], &built);
    if code != 0 {
        return code;
    }

    let prebuilt = root.join(HELLO_C_PREBUILT);
    let fresh = root.join(DEMOS_BUILD_DIR).join("hello_c.wasm");
    let source = match find_wasi_toolchain() {
        Ok(tc) => match build_hello_c(root, &tc, &fresh) {
            Ok(command) => {
                eprintln!("xtask: built hello_c with: {command}");
                match (fs::read(&fresh), fs::read(&prebuilt)) {
                    (Ok(a), Ok(b)) if a == b => {
                        eprintln!("xtask: hello_c matches {HELLO_C_PREBUILT}")
                    }
                    (Ok(a), Ok(b)) => eprintln!(
                        "xtask: warning: freshly built hello_c ({} bytes) differs from \
                         {HELLO_C_PREBUILT} ({} bytes): another clang or wasi-libc version, \
                         or main.c changed; the fresh build is used (update the prebuilt \
                         and its README if the change is intended)",
                        a.len(),
                        b.len()
                    ),
                    (Ok(_), Err(e)) => {
                        eprintln!("xtask: warning: cannot read {HELLO_C_PREBUILT}: {e}")
                    }
                    (Err(e), _) => {
                        eprintln!("xtask: cannot read the built {}: {e}", fresh.display());
                        return 1;
                    }
                }
                fresh
            }
            Err(e) => {
                eprintln!("xtask: warning: building hello_c failed ({e}); using the prebuilt binary {HELLO_C_PREBUILT}");
                prebuilt
            }
        },
        Err(why) => {
            eprintln!(
                "xtask: skipping the hello_c build: {why}; using the prebuilt binary {HELLO_C_PREBUILT} (set CLANG and WASI_SYSROOT, or install clang, lld and wasi-libc, to rebuild it)"
            );
            prebuilt
        }
    };
    install_module(root, base + 1, DEMO_MODULES[1], &source)
}

/// A clang that targets wasm32 plus a wasi-libc sysroot.
struct WasiToolchain {
    clang: String,
    sysroot: PathBuf,
}

/// Looks for `clang` (`CLANG` or [`CLANG_CANDIDATES`]) and a wasi-libc
/// sysroot (`WASI_SYSROOT` or [`WASI_SYSROOT_CANDIDATES`]); the error says
/// what is missing.
fn find_wasi_toolchain() -> Result<WasiToolchain, String> {
    let candidates: Vec<String> = match env::var("CLANG") {
        Ok(c) => vec![c],
        Err(_) => CLANG_CANDIDATES.iter().map(|c| c.to_string()).collect(),
    };
    let clang = candidates
        .iter()
        .find(|c| {
            Command::new(c).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
        })
        .cloned()
        .ok_or_else(|| format!("no clang found (tried {})", candidates.join(", ")))?;
    let sysroots: Vec<PathBuf> = match env::var_os("WASI_SYSROOT") {
        Some(s) => vec![PathBuf::from(s)],
        None => WASI_SYSROOT_CANDIDATES.iter().map(PathBuf::from).collect(),
    };
    let sysroot = sysroots
        .iter()
        .find(|s| {
            s.join("lib/wasm32-wasi/libc.a").is_file()
                && (s.join("include/wasm32-wasi/stdio.h").is_file() || s.join("include/stdio.h").is_file())
        })
        .cloned()
        .ok_or_else(|| {
            format!(
                "no wasi-libc sysroot (lib/wasm32-wasi/libc.a) under {}",
                sysroots.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
            )
        })?;
    Ok(WasiToolchain { clang, sysroot })
}

/// Compiles `demos/hello-c/main.c` to `out`. The plain wasi-sdk style
/// command is tried first; when the only failure is a missing wasm32
/// compiler-rt (Ubuntu's clang ships none for wasm32, and wasi-libc does not
/// need it for this program), the link is retried with `-nodefaultlibs -lc`.
/// Returns the command that worked.
fn build_hello_c(root: &Path, tc: &WasiToolchain, out: &Path) -> Result<String, String> {
    let base = [
        "--target=wasm32-wasi".to_string(),
        format!("--sysroot={}", tc.sysroot.display()),
        "-O2".into(),
        "-Wall".into(),
        "-Wextra".into(),
        "-o".into(),
        out.display().to_string(),
        HELLO_C_SOURCE.into(),
    ];
    let variants: [&[&str]; 2] = [&[], &["-nodefaultlibs", "-lc"]];
    let mut last_err = String::new();
    for extra in variants {
        let mut args: Vec<String> = base.to_vec();
        args.extend(extra.iter().map(|s| s.to_string()));
        let command = format!("{} {}", tc.clang, args.join(" "));
        eprintln!("xtask: compiling hello_c");
        eprintln!("xtask:   {command}");
        let output = Command::new(&tc.clang)
            .current_dir(root)
            .args(&args)
            .output()
            .map_err(|e| format!("cannot run {}: {e}", tc.clang))?;
        if output.status.success() {
            return Ok(command);
        }
        last_err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let missing_rt = last_err.contains("libclang_rt.builtins");
        if !missing_rt {
            break;
        }
        eprintln!("xtask:   no wasm32 compiler-rt; retrying without default libraries");
    }
    Err(last_err)
}

/// `cargo build -p kalium-kernel --target aarch64-unknown-none --release`,
/// after `wasm` unless `boot_only` (which embeds no boot image).
fn build(root: &Path, flags: BuildFlags) -> i32 {
    if !flags.boot_only {
        let code = wasm(root);
        if code != 0 {
            return code;
        }
    }
    let mut args = vec!["build", "-p", "kalium-kernel", "--target", KERNEL_TARGET, "--release"];
    let features = flags.features().join(",");
    if !features.is_empty() {
        args.extend(["--features", &features]);
    }
    run_cargo(
        root,
        &format!(
            "building kernel for {KERNEL_TARGET}{}",
            if features.is_empty() { String::new() } else { format!(" ({features})") }
        ),
        &args,
    )
}

/// The QEMU command line from the plan, with `memory` as the RAM size
/// ([`QEMU_MEMORY`] except for the self-test's second run).
fn qemu_command(root: &Path, memory: &str) -> Command {
    let mut cmd = Command::new(QEMU);
    cmd.current_dir(root).args([
        "-M",
        "virt,gic-version=2",
        "-cpu",
        "cortex-a72",
        "-m",
        memory,
        "-nographic",
        "-semihosting-config",
        "enable=on,target=native",
        "-no-reboot",
        "-kernel",
        KERNEL_ELF,
    ]);
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

/// Build, then run QEMU interactively (the console is the terminal; the
/// kernel exits QEMU by itself when `init` exits; `Ctrl-A X` quits early).
fn run(root: &Path, flags: BuildFlags) -> i32 {
    let code = build(root, flags);
    if code != 0 {
        return code;
    }
    let timeout = qemu_timeout();
    eprintln!("xtask: starting QEMU (timeout {} s)", timeout.as_secs());
    let mut child = match qemu_command(root, QEMU_MEMORY).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("xtask: failed to start {QEMU}: {e}");
            return 1;
        }
    };
    match wait_with_timeout(&mut child, timeout) {
        Ok(Some(status)) => status_code(status),
        Ok(None) => {
            eprintln!("xtask: QEMU timed out after {} s, killed", timeout.as_secs());
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

/// What a captured QEMU run produced.
struct Captured {
    status: Option<ExitStatus>,
    stdout: String,
}

/// Boots the already built kernel in QEMU with `memory` of RAM and the
/// console captured, prints the console, and returns the output and exit
/// status (`None` on timeout).
fn boot_captured(root: &Path, label: &str, memory: &str) -> Result<Captured, i32> {
    let timeout = qemu_timeout();
    eprintln!("xtask: {label}: starting QEMU with -m {memory} (timeout {} s)", timeout.as_secs());
    let mut child = match qemu_command(root, memory).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("xtask: failed to start {QEMU}: {e}");
            return Err(1);
        }
    };
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let started = Instant::now();

    let outcome = wait_with_timeout(&mut child, timeout);
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
    eprintln!("xtask: {label}: QEMU ran {:.1} s", started.elapsed().as_secs_f64());

    match outcome {
        Ok(status) => Ok(Captured { status, stdout: out }),
        Err(e) => {
            eprintln!("xtask: {label}: waiting for QEMU failed: {e}");
            Err(1)
        }
    }
}

fn report(label: &str, failures: &[String]) -> i32 {
    if failures.is_empty() {
        println!("{label}: PASS");
        0
    } else {
        for f in failures {
            eprintln!("{label}: FAIL: {f}");
        }
        1
    }
}

/// The failure, if any, in how a captured QEMU run ended.
fn check_exit(captured: &Captured) -> Option<String> {
    match captured.status {
        None => Some(format!("QEMU timed out after {} s", qemu_timeout().as_secs())),
        Some(s) if s.code() != Some(0) => Some(format!("QEMU exited with {s}{}, expected code 0", describe_exit(s.code()))),
        Some(_) => None,
    }
}

/// Builds the kernel selected by `flags` (`boot_only` set), boots it with
/// the plan's RAM size and requires the tick marker, exit code 0 and every
/// line in `markers`.
fn boot_only_check(root: &Path, label: &str, flags: BuildFlags, markers: &[&str]) -> i32 {
    let code = build(root, flags);
    if code != 0 {
        return code;
    }
    boot_only_check_built(root, label, QEMU_MEMORY, markers)
}

/// Boots the already built `boot-only` kernel with `memory` of RAM and
/// requires the tick marker, exit code 0 and every line in `markers`.
fn boot_only_check_built(root: &Path, label: &str, memory: &str, markers: &[&str]) -> i32 {
    let captured = match boot_captured(root, label, memory) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let mut failures = Vec::new();
    failures.extend(check_exit(&captured));
    for marker in [BOOT_TEST_MARKER].iter().chain(markers) {
        if !captured.stdout.lines().any(|l| l.trim_end().starts_with(marker)) {
            failures.push(format!("console output does not contain `{marker}`"));
        }
    }
    for bad in ["kalium: panic", "kalium: unhandled", ": FAIL ("] {
        if let Some(l) = captured.stdout.lines().find(|l| l.contains(bad)) {
            failures.push(format!("unexpected console line: `{}`", l.trim_end()));
        }
    }
    let code = report(label, &failures);
    if captured.status.is_none() { EXIT_TIMEOUT } else { code }
}

/// HAL-only check: build with `boot-only`, boot in QEMU, require the tick
/// marker and exit code 0.
fn boot_test(root: &Path) -> i32 {
    boot_only_check(root, "boot-test", BuildFlags { boot_only: true, selftest: false }, &[])
}

/// HAL hardening check: `boot-only` plus `hal-selftest`; the self-test must
/// print `PASS` with no failed case, then the boot test must still pass.
/// Run twice: with the plan's RAM size and with [`SELFTEST_ODD_MEMORY`], so
/// that the mapping of a partial last 2 MiB block of RAM (and the fault on
/// the first byte past it) is checked as well.
fn selftest(root: &Path) -> i32 {
    let code = build(root, BuildFlags { boot_only: true, selftest: true });
    if code != 0 {
        return code;
    }
    for (label, memory) in [("selftest", QEMU_MEMORY), ("selftest (odd RAM size)", SELFTEST_ODD_MEMORY)] {
        let code = boot_only_check_built(root, label, memory, &[SELFTEST_PASS_MARKER]);
        if code != 0 {
            return code;
        }
    }
    0
}

/// The full M1 check: host tests, the HAL self-test, wasm, kernel, QEMU,
/// scenario output.
fn test(root: &Path) -> i32 {
    let code = run_cargo(root, "running host tests (workspace default members)", &["test"]);
    if code != 0 {
        return code;
    }
    let code = run_cargo(root, "running HAL host tests (device tree reader)", &["test", "-p", "kalium-hal"]);
    if code != 0 {
        return code;
    }
    let code = run_cargo(
        root,
        "running SDK host tests",
        &["test", "--manifest-path", SDK_MANIFEST, "-p", "kalium-sdk", "--target-dir", WASM_BUILD_DIR],
    );
    if code != 0 {
        return code;
    }
    let code = run_cargo(root, "running xtask's own tests", &["test", "-p", "xtask"]);
    if code != 0 {
        return code;
    }
    // The self-test before the full build, so the binary left behind is the
    // full kernel.
    let code = selftest(root);
    if code != 0 {
        return code;
    }
    let code = build(root, BuildFlags::default());
    if code != 0 {
        return code;
    }
    let captured = match boot_captured(root, "test", QEMU_MEMORY) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let mut failures = Vec::new();
    failures.extend(check_exit(&captured));
    failures.extend(check_scenario(&captured.stdout));
    let code = report("test", &failures);
    if captured.status.is_none() { EXIT_TIMEOUT } else { code }
}

/// `rt` must meet at least this many of its 400 periods (95%).
const RT_PERIODS: u32 = 400;
const RT_MIN_MET: u32 = 380;
/// How many `park` children `greedy` spawns before the kernel-wide instance
/// limit refuses the next one: everything but `init` and `greedy` itself.
const GREEDY_CHILDREN: u32 = kalium_abi::limits::MAX_INSTANCES as u32 - 2;
/// What `hog` gets before each quota refuses it: memory objects of the
/// largest size up to its memory object quota, linear memory up to the
/// kernel's page limit, channels up to its channel quota, and a full queue
/// on each.
const HOG_MEMOBJS: u64 = kalium_abi::limits::MAX_MEMOBJ_BYTES_PER_INSTANCE / kalium_abi::limits::MAX_MEMOBJ_SIZE;
const HOG_PAGES: u64 = 256;
const HOG_CHANNELS: u64 = kalium_abi::limits::MAX_CHANNELS_PER_INSTANCE as u64;
const HOG_QUEUE_DEPTH: u64 = kalium_abi::limits::CHANNEL_QUEUE_DEPTH as u64;
const HOG_MESSAGES: u64 = HOG_CHANNELS * HOG_QUEUE_DEPTH;
/// The messages with no payload but the most capabilities a message may
/// carry that `hog` sends on its fresh channel (the queue less the
/// 4096-byte message already in it) and how many messages of that queue
/// it takes back (half of it: the 4096-byte one and then cap-only ones).
const HOG_CAP_ONLY_SENT: u64 = HOG_QUEUE_DEPTH - 1;
const HOG_RECEIVED_BACK: u64 = HOG_QUEUE_DEPTH / 2;
/// What `hog`'s exit drains from the channels it still holds: a full queue
/// of 4096-byte messages on every channel but the one it gave back, and
/// what is left of the fresh channel, which is cap-only messages. The
/// bytes are what the kernel charged for them at `send` and must give back
/// to its budget: the cost of a queued message
/// (`kalium_ipc::Message::cost_of`: slot, payload and capabilities), not
/// just the payload, or a queue of cap-only messages is free and eats the
/// kernel's headroom.
const HOG_DRAINED_MESSAGES: u64 = (HOG_CHANNELS - 1) * HOG_QUEUE_DEPTH + (HOG_QUEUE_DEPTH - HOG_RECEIVED_BACK);
const HOG_DRAINED_BYTES: u64 =
    (HOG_CHANNELS - 1) * HOG_QUEUE_DEPTH * kalium_ipc::Message::cost_of(kalium_abi::limits::MAX_MESSAGE_LEN, 0) as u64
        + (HOG_QUEUE_DEPTH - HOG_RECEIVED_BACK) * kalium_ipc::Message::cost_of(0, kalium_abi::limits::MAX_MESSAGE_CAPS) as u64;
/// The reference cycle `hog` leaves before it hoards: one channel whose
/// queue holds one zero-byte message carrying one capability on that
/// channel, with both ends dropped. The kernel must collect the channel
/// (one object, one message, the message's cost) at once; a kernel that
/// only counts references keeps it, and then `hog` is a channel short of
/// its quota and the budget never settles.
const CYCLE_OBJECTS: u64 = 1;
const CYCLE_MESSAGES: u64 = 1;
const CYCLE_BYTES: u64 = kalium_ipc::Message::cost_of(0, 1) as u64;

/// One expected console line.
enum Expect {
    /// The whole line, exactly.
    Line(&'static str),
    /// The kernel's trap report for `faulty`.
    FaultyTrap,
    /// `rt: met N/400` with `N >= RT_MIN_MET`.
    RtMet,
    /// `greedy: spawn refused (ENOMEM) after N instances` with
    /// `N == GREEDY_CHILDREN`: the instance limit held exactly.
    GreedyLimit,
    /// `hello_c: time() ok (N s)`: C's `time()` worked (the value is
    /// seconds since boot and varies).
    HelloCTime,
    /// `init: shelf init N us, restore M us` with `M < N`: starting an
    /// instance from the snapshot was faster than its initialisation.
    ShelfRestoreFaster,
    /// `hog: memobjs N, pages P, channels C, messages M` with exactly the
    /// quotas' values: each quota held, and not before its limit.
    HogQuota,
    /// `hog: N cap-only messages sent, M received back` with exactly
    /// [`HOG_CAP_ONLY_SENT`] and [`HOG_RECEIVED_BACK`]: the queue took
    /// zero-byte messages carrying capabilities up to its depth and gave
    /// them back.
    HogCapOnly,
    /// `kalium: instance <id> (hog) exit drained N queued messages, B
    /// budget bytes` with exactly [`HOG_DRAINED_MESSAGES`] and
    /// [`HOG_DRAINED_BYTES`]: the kernel charged every queued message's
    /// slot, payload and capabilities to its budget and released them with
    /// the channels.
    HogDrain,
    /// `kalium: instance budget: 0 bytes in use`: once every instance has
    /// exited, nothing charged to the kernel-wide budget outlived its
    /// release.
    BudgetSettled,
    /// `kalium: collected N unreachable objects: M queued messages, B
    /// budget bytes` with exactly [`CYCLE_OBJECTS`], [`CYCLE_MESSAGES`]
    /// and [`CYCLE_BYTES`]: the channel `hog` sent over itself and
    /// dropped was freed although a queued capability still referred to
    /// it.
    CycleCollected,
}

impl Expect {
    fn matches(&self, line: &str) -> bool {
        match self {
            Expect::Line(l) => line == *l,
            Expect::FaultyTrap => {
                line.starts_with("kalium: instance ") && line.contains("(faulty) trapped: out of bounds memory access")
            }
            Expect::RtMet => parse_rt_met(line).is_some_and(|(met, total)| total == RT_PERIODS && met >= RT_MIN_MET),
            Expect::GreedyLimit => parse_greedy_refused(line) == Some(GREEDY_CHILDREN),
            Expect::HelloCTime => line
                .strip_prefix("hello_c: time() ok (")
                .and_then(|r| r.strip_suffix(" s)"))
                .is_some_and(|n| n.parse::<u64>().is_ok()),
            Expect::ShelfRestoreFaster => parse_shelf_times(line).is_some_and(|(init, restore)| restore < init),
            Expect::HogQuota => parse_hog(line) == Some((HOG_MEMOBJS, HOG_PAGES, HOG_CHANNELS, HOG_MESSAGES)),
            Expect::HogCapOnly => parse_hog_cap_only(line) == Some((HOG_CAP_ONLY_SENT, HOG_RECEIVED_BACK)),
            Expect::HogDrain => parse_hog_drain(line) == Some((HOG_DRAINED_MESSAGES, HOG_DRAINED_BYTES)),
            Expect::BudgetSettled => parse_budget_in_use(line) == Some(0),
            Expect::CycleCollected => parse_collected(line) == Some((CYCLE_OBJECTS, CYCLE_MESSAGES, CYCLE_BYTES)),
        }
    }

    fn describe(&self) -> String {
        match self {
            Expect::Line(l) => format!("`{l}`"),
            Expect::FaultyTrap => "`kalium: instance <id> (faulty) trapped: out of bounds memory access`".into(),
            Expect::RtMet => format!("`rt: met N/{RT_PERIODS}` with N >= {RT_MIN_MET}"),
            Expect::GreedyLimit => {
                format!("`greedy: spawn refused (ENOMEM) after {GREEDY_CHILDREN} instances`")
            }
            Expect::HelloCTime => "`hello_c: time() ok (N s)`".into(),
            Expect::ShelfRestoreFaster => "`init: shelf init N us, restore M us` with M < N".into(),
            Expect::HogQuota => {
                format!("`hog: memobjs {HOG_MEMOBJS}, pages {HOG_PAGES}, channels {HOG_CHANNELS}, messages {HOG_MESSAGES}`")
            }
            Expect::HogCapOnly => {
                format!("`hog: {HOG_CAP_ONLY_SENT} cap-only messages sent, {HOG_RECEIVED_BACK} received back`")
            }
            Expect::HogDrain => format!(
                "`kalium: instance <id> (hog) exit drained {HOG_DRAINED_MESSAGES} queued messages, {HOG_DRAINED_BYTES} budget bytes`"
            ),
            Expect::BudgetSettled => "`kalium: instance budget: 0 bytes in use`".into(),
            Expect::CycleCollected => format!(
                "`kalium: collected {CYCLE_OBJECTS} unreachable objects: {CYCLE_MESSAGES} queued messages, {CYCLE_BYTES} budget bytes`"
            ),
        }
    }
}

/// Parses `kalium: collected N unreachable objects: M queued messages, B
/// budget bytes` into `(N, M, B)`.
fn parse_collected(line: &str) -> Option<(u64, u64, u64)> {
    let rest = line.strip_prefix("kalium: collected ")?;
    let (objects, rest) = rest.split_once(" unreachable objects: ")?;
    let (messages, rest) = rest.split_once(" queued messages, ")?;
    let bytes = rest.strip_suffix(" budget bytes")?;
    Some((objects.parse().ok()?, messages.parse().ok()?, bytes.parse().ok()?))
}

/// Parses `hog: N cap-only messages sent, M received back` into `(N, M)`.
fn parse_hog_cap_only(line: &str) -> Option<(u64, u64)> {
    let rest = line.strip_prefix("hog: ")?;
    let (sent, rest) = rest.split_once(" cap-only messages sent, ")?;
    let received = rest.strip_suffix(" received back")?;
    Some((sent.parse().ok()?, received.parse().ok()?))
}

/// Parses `kalium: instance <id> (hog) exit drained N queued messages, B
/// budget bytes` into `(N, B)`.
fn parse_hog_drain(line: &str) -> Option<(u64, u64)> {
    let rest = line.strip_prefix("kalium: instance ")?;
    let (_, rest) = rest.split_once(" (hog) exit drained ")?;
    let (messages, rest) = rest.split_once(" queued messages, ")?;
    let bytes = rest.strip_suffix(" budget bytes")?;
    Some((messages.parse().ok()?, bytes.parse().ok()?))
}

/// Parses `kalium: instance budget: N bytes in use` into `N`.
fn parse_budget_in_use(line: &str) -> Option<u64> {
    line.strip_prefix("kalium: instance budget: ")?.strip_suffix(" bytes in use")?.parse().ok()
}

/// Parses `hog: memobjs N, pages P, channels C, messages M`.
fn parse_hog(line: &str) -> Option<(u64, u64, u64, u64)> {
    let rest = line.strip_prefix("hog: memobjs ")?;
    let (memobjs, rest) = rest.split_once(", pages ")?;
    let (pages, rest) = rest.split_once(", channels ")?;
    let (channels, messages) = rest.split_once(", messages ")?;
    Some((memobjs.parse().ok()?, pages.parse().ok()?, channels.parse().ok()?, messages.parse().ok()?))
}

/// Parses `init: shelf init N us, restore M us` into `(N, M)`.
fn parse_shelf_times(line: &str) -> Option<(u64, u64)> {
    let rest = line.strip_prefix("init: shelf init ")?;
    let (init, rest) = rest.split_once(" us, restore ")?;
    let restore = rest.strip_suffix(" us")?;
    Some((init.parse().ok()?, restore.parse().ok()?))
}

/// Parses `greedy: spawn refused (ENOMEM) after N instances` into `N`.
fn parse_greedy_refused(line: &str) -> Option<u32> {
    line.strip_prefix("greedy: spawn refused (ENOMEM) after ")?.strip_suffix(" instances")?.parse().ok()
}

/// Parses `rt: met N/T` into `(N, T)`.
fn parse_rt_met(line: &str) -> Option<(u32, u32)> {
    let rest = line.strip_prefix("rt: met ")?;
    let (met, total) = rest.trim().split_once('/')?;
    Some((met.parse().ok()?, total.parse().ok()?))
}

/// The scenario lines in the order they must appear. Lines whose relative
/// order is not guaranteed are checked separately.
const ORDERED: &[Expect] = &[
    Expect::Line("hello from init"),
    Expect::Line("pong: got memobj hello"),
    Expect::Line("pong: 100 messages"),
    Expect::Line("init: pong exited 0"),
    Expect::RtMet,
    Expect::Line("init: rt exited 0"),
    Expect::Line("init: fair exited 0"),
    Expect::Line("init: fair exited 0"),
    Expect::FaultyTrap,
    Expect::Line("init: faulty exited -1"),
    // The WASI demos: a Rust std program and a C program, each printing
    // through fd 1 and leaving through proc_exit.
    Expect::Line("hello_std: hello from Rust std on Kalium"),
    Expect::Line("hello_std: vec sum 55, len 10, squares [1, 4, 9, 16, 25]"),
    Expect::Line("hello_std: fmt   Kalium|Kalium  |0042|0xff|+7"),
    Expect::Line("hello_std: map [(\"one\", 1), (\"three\", 3), (\"two\", 2)] total 6"),
    Expect::Line("hello_std: sqrt(2) = 1.414214, leibniz pi ~ 3.1416, 1e3 ln = 6.908"),
    Expect::Line("hello_std: args [], env vars 0, HOME None"),
    Expect::Line("hello_std: slept 10 ms (clock advanced)"),
    Expect::Line("hello_std: exiting with 3"),
    Expect::Line("init: hello_std exited 3"),
    Expect::Line("hello_c: hello from C on Kalium"),
    Expect::Line("hello_c: printf 42|   42|42   |ff|10|K|str| 3.14|1.234500e+03|0.0001|%"),
    Expect::Line("hello_c: argc=0 argv0=(none)"),
    Expect::Line("hello_c: malloc/free ok (3 blocks)"),
    Expect::Line("hello_c: qsort [1, 2, 3, 5, 8, 13, 21]"),
    Expect::Line("hello_c: strtod 3.14159 rest \" tail\", strtol 42 -17 255"),
    Expect::Line("hello_c: snprintf \"x=7 y=3.50 z=end\" (16), \"truncat\" (9)"),
    Expect::Line("hello_c: fact(10)=3628800 fib(20)=6765"),
    Expect::HelloCTime,
    // stdin is a channel init filled with "hello\n", "world" and an empty
    // message: a stdio line, a readv with an empty first buffer (the empty
    // one is skipped, not reported as end of file), the empty message as
    // end of file, and a zero-length read with nothing queued, which must
    // return 0 at once instead of waiting for input (init holds the sender
    // and waits for hello_c: waiting there deadlocks the kernel).
    Expect::Line("hello_c: stdin fgets \"hello\""),
    Expect::Line("hello_c: stdin readv (empty, 15) got 5 \"world\""),
    Expect::Line("hello_c: stdin read got 0 (end of file)"),
    Expect::Line("hello_c: stdin read of 0 bytes got 0 at once"),
    Expect::Line("hello_c: exiting with 5"),
    Expect::Line("init: hello_c exited 5"),
    // Revocation: `revoked` prints once through a console derived from a
    // handle init keeps; init then revokes the handle and the receive end
    // it gave away, drops its own receiver and finds the channel closed;
    // `revoked` sees EBADCAP on both capabilities and exits 42.
    Expect::Line("revoked: hello through a derived console"),
    Expect::Line("init: send to revoked receiver: ECLOSED"),
    Expect::Line("init: handle derives again after revoke"),
    Expect::Line("init: revoked exited 42"),
    // The runtime shelf: `shelf` builds its state, says so and parks; init
    // snapshots it and starts three instances from the snapshot at
    // `resume`, each finding the state intact; the restore is the faster
    // start.
    Expect::Line("shelf: initialised"),
    Expect::Line("shelf: resumed ok"),
    Expect::Line("init: resumed shelf exited 0"),
    Expect::Line("shelf: resumed ok"),
    Expect::Line("init: resumed shelf exited 0"),
    Expect::Line("shelf: resumed ok"),
    Expect::Line("init: resumed shelf exited 0"),
    Expect::Line("init: shelf exited 0"),
    Expect::ShelfRestoreFaster,
    // Resource quotas: `hog` hoards memory objects, linear memory, channels
    // and queue space until each quota refuses it (the counts are the
    // quotas' values exactly), finds the kernel still working, and exits
    // holding everything; init then spawns an ordinary instance and
    // creates a memory object of the largest size to show the kernel is
    // unharmed and the memory was released.
    // The kernel-wide budget charges a queued message's capabilities and
    // slot, not only its payload: `hog` fills the rest of its fresh queue
    // with zero-byte messages carrying four capabilities each, takes half
    // back, and the kernel's report of what its exit drained gives back
    // exactly their cost (`Message::cost_of`).
    // Before any of that, `hog` leaves a reference cycle (a channel's send
    // end queued in that same channel, both ends dropped) that only a
    // reachability rule frees: the kernel reports the collection, and the
    // quota and the budget lines below show it was given back.
    Expect::Line("hog: cycle: a channel's send end sent over itself, both ends dropped"),
    Expect::CycleCollected,
    Expect::HogQuota,
    Expect::HogCapOnly,
    Expect::Line("hog: quota reached, kernel alive"),
    Expect::HogDrain,
    Expect::Line("init: hog exited 0"),
    Expect::Line("init: after hog, spawn ok"),
    Expect::GreedyLimit,
    Expect::Line("greedy: slot reclaimed, park exited 0"),
    Expect::Line("init: greedy exited 0"),
    Expect::Line("init: done"),
    // Everything charged to the budget was released: `send`, `spawn`,
    // `memobj_create` and `instance_snapshot` charge what `recv`, exit
    // and the objects' destruction give back.
    Expect::BudgetSettled,
    Expect::Line("kalium: init exited 0"),
];

/// Checks the console output of the M1 scenario. Returns the failures.
fn check_scenario(out: &str) -> Vec<String> {
    let lines: Vec<&str> = out.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut failures = Vec::new();

    let mut pos = 0;
    for expect in ORDERED {
        match lines[pos.min(lines.len())..].iter().position(|l| expect.matches(l)) {
            Some(i) => pos += i + 1,
            None => {
                let detail = match expect {
                    Expect::RtMet => match lines.iter().find_map(|l| parse_rt_met(l)) {
                        Some((met, total)) => format!(" (found `rt: met {met}/{total}`)"),
                        None => String::new(),
                    },
                    Expect::GreedyLimit => match lines.iter().find_map(|l| parse_greedy_refused(l)) {
                        Some(n) => format!(" (found `... after {n} instances`)"),
                        None => String::new(),
                    },
                    Expect::ShelfRestoreFaster => match lines.iter().find_map(|l| parse_shelf_times(l)) {
                        Some((init, restore)) => {
                            format!(" (found `init {init} us, restore {restore} us`)")
                        }
                        None => String::new(),
                    },
                    Expect::HogQuota => match lines.iter().find_map(|l| parse_hog(l)) {
                        Some((m, p, c, n)) => {
                            format!(" (found `memobjs {m}, pages {p}, channels {c}, messages {n}`)")
                        }
                        None => String::new(),
                    },
                    Expect::HogCapOnly => match lines.iter().find_map(|l| parse_hog_cap_only(l)) {
                        Some((s, r)) => {
                            format!(" (found `{s} cap-only messages sent, {r} received back`)")
                        }
                        None => String::new(),
                    },
                    Expect::HogDrain => match lines.iter().find_map(|l| parse_hog_drain(l)) {
                        Some((n, b)) => {
                            format!(" (found `drained {n} queued messages, {b} budget bytes`)")
                        }
                        None => String::new(),
                    },
                    Expect::BudgetSettled => match lines.iter().find_map(|l| parse_budget_in_use(l)) {
                        Some(b) => format!(" (found `{b} bytes in use`)"),
                        None => String::new(),
                    },
                    Expect::CycleCollected => match lines.iter().find_map(|l| parse_collected(l)) {
                        Some((o, m, b)) => {
                            format!(" (found `collected {o} unreachable objects: {m} queued messages, {b} budget bytes`)")
                        }
                        None => String::new(),
                    },
                    _ => String::new(),
                };
                failures.push(format!("missing {} after line {pos}{detail}", expect.describe()));
            }
        }
    }

    // `ping` may exit before or after `pong` prints, but init reports it
    // before `pong`.
    let ping = lines.iter().position(|l| *l == "init: ping exited 0");
    let pong = lines.iter().position(|l| *l == "init: pong exited 0");
    match (ping, pong) {
        (None, _) => failures.push("missing `init: ping exited 0`".into()),
        (Some(a), Some(b)) if a > b => failures.push("`init: ping exited 0` must come before `init: pong exited 0`".into()),
        _ => {}
    }

    for bad in [
        "kalium: panic",
        "kalium: deadlock",
        "still running",
        "kalium: unhandled",
        "kalium: kernel fault",
        // `revoked` printing after its console was revoked.
        "must not appear",
    ] {
        if let Some(l) = lines.iter().find(|l| l.contains(bad)) {
            failures.push(format!("unexpected console line: `{l}`"));
        }
    }
    failures
}

/// `cargo clippy` with `-D warnings` on the host crates (all targets), the
/// kernel (aarch64), the SDK (wasm32) and the WASI demos (wasm32-wasip1).
fn clippy(root: &Path) -> i32 {
    let code = run_cargo(
        root,
        "clippy: host crates",
        &["clippy", "--workspace", "--exclude", "kalium-kernel", "--all-targets", "--", "-D", "warnings"],
    );
    if code != 0 {
        return code;
    }
    if !wasm_present(root) {
        let code = wasm(root);
        if code != 0 {
            return code;
        }
    }
    let code = run_cargo(
        root,
        "clippy: kernel",
        &["clippy", "-p", "kalium-kernel", "--target", KERNEL_TARGET, "--", "-D", "warnings"],
    );
    if code != 0 {
        return code;
    }
    let code = run_cargo(
        root,
        "clippy: SDK",
        &[
            "clippy",
            "--manifest-path",
            SDK_MANIFEST,
            "--workspace",
            "--target",
            WASM_TARGET,
            "--target-dir",
            WASM_BUILD_DIR,
            "--",
            "-D",
            "warnings",
        ],
    );
    if code != 0 {
        return code;
    }
    run_cargo(
        root,
        "clippy: WASI demos",
        &[
            "clippy",
            "--manifest-path",
            DEMOS_MANIFEST,
            "--workspace",
            "--target",
            WASIP1_TARGET,
            "--target-dir",
            DEMOS_BUILD_DIR,
            "--",
            "-D",
            "warnings",
        ],
    )
}

/// The rustfmt configuration shared with Trunk (`trunk fmt`), so both format
/// the same way.
const RUSTFMT_CONFIG: &str = ".trunk/configs/.rustfmt.toml";

/// `cargo fmt --all -- --check` on the three workspaces (root, SDK, demos),
/// with [`RUSTFMT_CONFIG`].
fn fmt_check(root: &Path) -> i32 {
    let config = root.join(RUSTFMT_CONFIG);
    let config = config.to_str().expect("workspace path is UTF-8");
    let workspaces: [(&str, Option<&str>); 3] = [
        ("fmt-check: workspace", None),
        ("fmt-check: SDK", Some(SDK_MANIFEST)),
        ("fmt-check: WASI demos", Some(DEMOS_MANIFEST)),
    ];
    for (label, manifest) in workspaces {
        let mut args = vec!["fmt", "--all"];
        if let Some(manifest) = manifest {
            args.extend(["--manifest-path", manifest]);
        }
        args.extend(["--", "--check", "--config-path", config]);
        let code = run_cargo(root, label, &args);
        if code != 0 {
            return code;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::LazyLock;

    /// [`GOOD_TEMPLATE`] with `hog`'s figures filled in from the constants
    /// the scenario is checked against.
    static GOOD: LazyLock<String> = LazyLock::new(|| {
        GOOD_TEMPLATE
            .replace(
                "{hog_cap_only}",
                &format!("{HOG_CAP_ONLY_SENT} cap-only messages sent, {HOG_RECEIVED_BACK} received back"),
            )
            .replace("{hog_drain}", &format!("{HOG_DRAINED_MESSAGES} queued messages, {HOG_DRAINED_BYTES} budget bytes"))
            .replace(
                "{cycle}",
                &format!("{CYCLE_OBJECTS} unreachable objects: {CYCLE_MESSAGES} queued messages, {CYCLE_BYTES} budget bytes"),
            )
    });

    const GOOD_TEMPLATE: &str = "kalium: boot\n\
        hello from init\n\
        kalium: instance 0 (init) spawned instance 1 (ping, fair)\n\
        kalium: instance 1 (ping) exited 0\n\
        init: ping exited 0\n\
        pong: got memobj hello\n\
        pong: 100 messages\n\
        init: pong exited 0\n\
        rt: met 398/400\n\
        init: rt exited 0\n\
        init: fair exited 0\n\
        init: fair exited 0\n\
        kalium: instance 1 (faulty) trapped: out of bounds memory access\n\
        kalium: instance 1 (faulty) exited -1 after 3 instructions\n\
        init: faulty exited -1\n\
        hello_std: hello from Rust std on Kalium\n\
        hello_std: vec sum 55, len 10, squares [1, 4, 9, 16, 25]\n\
        hello_std: fmt   Kalium|Kalium  |0042|0xff|+7\n\
        hello_std: map [(\"one\", 1), (\"three\", 3), (\"two\", 2)] total 6\n\
        hello_std: sqrt(2) = 1.414214, leibniz pi ~ 3.1416, 1e3 ln = 6.908\n\
        hello_std: args [], env vars 0, HOME None\n\
        hello_std: slept 10 ms (clock advanced)\n\
        hello_std: exiting with 3\n\
        init: hello_std exited 3\n\
        hello_c: hello from C on Kalium\n\
        hello_c: printf 42|   42|42   |ff|10|K|str| 3.14|1.234500e+03|0.0001|%\n\
        hello_c: argc=0 argv0=(none)\n\
        hello_c: malloc/free ok (3 blocks)\n\
        hello_c: qsort [1, 2, 3, 5, 8, 13, 21]\n\
        hello_c: strtod 3.14159 rest \" tail\", strtol 42 -17 255\n\
        hello_c: snprintf \"x=7 y=3.50 z=end\" (16), \"truncat\" (9)\n\
        hello_c: fact(10)=3628800 fib(20)=6765\n\
        hello_c: time() ok (2 s)\n\
        hello_c: stdin fgets \"hello\"\n\
        hello_c: stdin readv (empty, 15) got 5 \"world\"\n\
        hello_c: stdin read got 0 (end of file)\n\
        hello_c: stdin read of 0 bytes got 0 at once\n\
        hello_c: exiting with 5\n\
        init: hello_c exited 5\n\
        kalium: instance 0 (init) spawned instance 1 (revoked, fair)\n\
        revoked: hello through a derived console\n\
        kalium: instance 0 (init) revoked slot 16: 1 derived capability invalidated\n\
        kalium: instance 0 (init) revoked slot 15: 1 derived capability invalidated\n\
        init: send to revoked receiver: ECLOSED\n\
        init: handle derives again after revoke\n\
        kalium: instance 1 (revoked) exited 42 after 657 instructions\n\
        init: revoked exited 42\n\
        kalium: instance 0 (init) spawned instance 1 (shelf, fair)\n\
        shelf: initialised\n\
        kalium: instance 0 (init) snapshotted instance 1 (shelf): 21 pages, 1376294 bytes\n\
        kalium: instance 0 (init) spawned instance 2 (shelf, fair) from snapshot\n\
        shelf: resumed ok\n\
        kalium: instance 2 (shelf) exited 0 after 1048831 instructions\n\
        init: resumed shelf exited 0\n\
        kalium: instance 0 (init) spawned instance 2 (shelf, fair) from snapshot\n\
        shelf: resumed ok\n\
        kalium: instance 2 (shelf) exited 0 after 1048831 instructions\n\
        init: resumed shelf exited 0\n\
        kalium: instance 0 (init) spawned instance 2 (shelf, fair) from snapshot\n\
        shelf: resumed ok\n\
        kalium: instance 2 (shelf) exited 0 after 1048831 instructions\n\
        init: resumed shelf exited 0\n\
        kalium: instance 1 (shelf) exited 0 after 11665700 instructions\n\
        init: shelf exited 0\n\
        init: shelf init 284494 us, restore 27429 us\n\
        kalium: instance 0 (init) spawned instance 1 (hog, fair)\n\
        hog: cycle: a channel's send end sent over itself, both ends dropped\n\
        kalium: collected {cycle}\n\
        hog: memobjs 2, pages 256, channels 64, messages 1024\n\
        hog: send 4096 bytes on a fresh channel: ok\n\
        hog: {hog_cap_only}\n\
        hog: quota reached, kernel alive\n\
        kalium: instance 1 (hog) exited 0 after 20000 instructions\n\
        kalium: instance 1 (hog) exit drained {hog_drain}\n\
        init: hog exited 0\n\
        kalium: instance 0 (init) spawned instance 1 (park, fair)\n\
        kalium: instance 1 (park) exited 0 after 53 instructions\n\
        init: after hog, spawn ok\n\
        kalium: instance 0 (init) spawned instance 1 (greedy, fair)\n\
        kalium: instance 1 (greedy) spawned instance 2 (park, fair)\n\
        greedy: spawn refused (ENOMEM) after 30 instances\n\
        kalium: instance 2 (park) exited 0\n\
        greedy: slot reclaimed, park exited 0\n\
        kalium: instance 1 (greedy) exited 0\n\
        init: greedy exited 0\n\
        init: done\n\
        kalium: instance budget: 0 bytes in use\n\
        kalium: init exited 0\n";

    #[test]
    fn good_output_passes() {
        assert!(check_scenario(&GOOD).is_empty(), "{:?}", check_scenario(&GOOD));
        assert!(check_scenario(&GOOD.replace('\n', "\r\n")).is_empty());
    }

    #[test]
    fn rt_threshold() {
        assert_eq!(parse_rt_met("rt: met 398/400"), Some((398, 400)));
        assert_eq!(parse_rt_met("rt: met x/400"), None);
        let bad = GOOD.replace("rt: met 398/400", "rt: met 379/400");
        let f = check_scenario(&bad);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("found `rt: met 379/400`"));
        let ok = GOOD.replace("rt: met 398/400", "rt: met 380/400");
        assert!(check_scenario(&ok).is_empty());
    }

    #[test]
    fn missing_and_misordered_lines_fail() {
        let missing = GOOD.replace("init: done\n", "");
        assert_eq!(check_scenario(&missing).len(), 1);
        let one_fair = GOOD.replacen("init: fair exited 0\n", "", 1);
        assert_eq!(check_scenario(&one_fair).len(), 1);
        let no_trap = GOOD.replace("kalium: instance 1 (faulty) trapped: out of bounds memory access\n", "");
        assert_eq!(check_scenario(&no_trap).len(), 1);
        let late_ping = GOOD.replace("init: ping exited 0\n", "").replace("init: done\n", "init: ping exited 0\ninit: done\n");
        assert!(check_scenario(&late_ping)[0].contains("before"));
        let panicked = format!("{}kalium: panic at x: y\n", *GOOD);
        assert_eq!(check_scenario(&panicked).len(), 1);
        let faulted = format!(
            "{}kalium: unhandled Synchronous exception (CurrentElSp0)\n\
             kalium: kernel fault: data abort (permission fault, level 3, write) at FAR_EL1 = 0x1, ELR_EL1 = 0x2\n",
            *GOOD
        );
        assert_eq!(check_scenario(&faulted).len(), 2);
    }

    #[test]
    fn wasi_demos_are_checked() {
        let wrong_code = GOOD.replace("init: hello_std exited 3", "init: hello_std exited 0");
        assert_eq!(check_scenario(&wrong_code).len(), 1);
        let no_time = GOOD.replace("hello_c: time() ok (2 s)\n", "hello_c: time() failed (-1 s)\n");
        let f = check_scenario(&no_time);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("time() ok"));
        let no_sleep = GOOD.replace("(clock advanced)", "(clock did not advance)");
        assert_eq!(check_scenario(&no_sleep).len(), 1);
        let c_failed = GOOD.replace("init: hello_c exited 5", "init: hello_c exited -1");
        assert_eq!(check_scenario(&c_failed).len(), 1);
        // Revocation: the console must still have worked before, must not
        // work after, the channel must have closed, and the exit code is
        // the instance's verdict.
        let printed_after =
            GOOD.replace("init: revoked exited 42\n", "revoked: this line must not appear\ninit: revoked exited 42\n");
        assert_eq!(check_scenario(&printed_after).len(), 1);
        let console_kept = GOOD.replace("init: revoked exited 42", "init: revoked exited 43");
        assert_eq!(check_scenario(&console_kept).len(), 1);
        let not_closed = GOOD.replace("init: send to revoked receiver: ECLOSED", "init: send to revoked receiver succeeded");
        assert_eq!(check_scenario(&not_closed).len(), 1);
        let no_hello = GOOD.replace("revoked: hello through a derived console\n", "");
        assert_eq!(check_scenario(&no_hello).len(), 1);
    }

    #[test]
    fn shelf_restore_must_beat_init() {
        assert_eq!(parse_shelf_times("init: shelf init 400000 us, restore 40000 us"), Some((400000, 40000)));
        assert_eq!(parse_shelf_times("init: shelf init x us, restore 1 us"), None);
        assert_eq!(parse_shelf_times("init: shelf init 1 us"), None);
        // A restore that is not faster than the initialisation fails
        // exactly once, with the numbers found.
        for slow in ["284494", "500000"] {
            let not_faster = GOOD.replace(
                "init: shelf init 284494 us, restore 27429 us",
                &format!("init: shelf init 284494 us, restore {slow} us"),
            );
            let f = check_scenario(&not_faster);
            assert_eq!(f.len(), 1, "{f:?}");
            assert!(f[0].contains(&format!("found `init 284494 us, restore {slow} us`")));
        }
        let faster =
            GOOD.replace("init: shelf init 284494 us, restore 27429 us", "init: shelf init 284494 us, restore 284493 us");
        assert!(check_scenario(&faster).is_empty());
        // Every restored instance must report, and the state must check.
        let two_resumes = GOOD.replacen("init: resumed shelf exited 0\n", "", 1);
        assert!(!check_scenario(&two_resumes).is_empty());
        let mismatch = GOOD.replacen("shelf: resumed ok\n", "shelf: checksum mismatch\n", 1);
        assert!(!check_scenario(&mismatch).is_empty());
        let no_init = GOOD.replace("shelf: initialised\n", "");
        assert_eq!(check_scenario(&no_init).len(), 1);
        let no_timing = GOOD.replace("init: shelf init 284494 us, restore 27429 us\n", "");
        let f = check_scenario(&no_timing);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("M < N"));
    }

    #[test]
    fn exit_codes_are_described() {
        assert_eq!(describe_exit(Some(0)), "");
        assert!(describe_exit(Some(1)).contains("panic"));
        assert!(describe_exit(Some(EXIT_KERNEL_FAULT)).contains("kernel fault"));
        assert_eq!(describe_exit(None), "");
    }

    #[test]
    fn greedy_must_hit_the_limit_exactly() {
        assert_eq!(GREEDY_CHILDREN, 30);
        assert_eq!(parse_greedy_refused("greedy: spawn refused (ENOMEM) after 30 instances"), Some(30));
        assert_eq!(parse_greedy_refused("greedy: spawn refused (ENOMEM) after x instances"), None);
        // One child too many or too few means the kernel's limit is off.
        for n in [29, 31] {
            let off = GOOD.replace("after 30 instances", &format!("after {n} instances"));
            let f = check_scenario(&off);
            assert_eq!(f.len(), 1, "{f:?}");
            assert!(f[0].contains(&format!("found `... after {n} instances`")));
        }
        // A kernel without the limit: greedy gives up and init fails.
        let unlimited = GOOD
            .replace("greedy: spawn refused (ENOMEM) after 30 instances\n", "greedy: no instance limit\n")
            .replace("greedy: slot reclaimed, park exited 0\n", "")
            .replace("init: greedy exited 0", "init: greedy exited 1");
        assert_eq!(check_scenario(&unlimited).len(), 3);
    }

    #[test]
    fn hog_must_hit_every_quota_exactly() {
        assert_eq!(parse_hog("hog: memobjs 2, pages 256, channels 64, messages 1024"), Some((2, 256, 64, 1024)));
        assert_eq!(parse_hog("hog: memobjs x, pages 256, channels 64, messages 1024"), None);
        assert_eq!((HOG_MEMOBJS, HOG_CHANNELS, HOG_MESSAGES), (2, 64, 1024));
        // A quota that did not hold (or held too early) fails the scenario.
        for wrong in [
            "hog: memobjs 3, pages 256, channels 64, messages 1024",
            "hog: memobjs 2, pages 255, channels 64, messages 1024",
            "hog: memobjs 2, pages 256, channels 63, messages 1024",
            "hog: memobjs 2, pages 256, channels 64, messages 1023",
        ] {
            let off = GOOD.replace("hog: memobjs 2, pages 256, channels 64, messages 1024", wrong);
            let f = check_scenario(&off);
            assert_eq!(f.len(), 1, "{f:?}");
            assert!(f[0].contains("found `memobjs"), "{f:?}");
        }
        // A kernel that panics under the hog: the forbidden line and
        // everything after it missing.
        let panicked = GOOD
            .split("hog: memobjs")
            .next()
            .map(|head| format!("{head}kalium: panic at x: memory allocation of 4096 bytes failed\n"))
            .unwrap();
        assert!(check_scenario(&panicked).len() > 5);
        // The kernel survived but a later spawn did not.
        let no_spawn = GOOD.replace("init: after hog, spawn ok\n", "");
        assert_eq!(check_scenario(&no_spawn).len(), 1);
        // An ENOMEM on the fresh send is acceptable; a panic line is not.
        let enomem =
            GOOD.replace("hog: send 4096 bytes on a fresh channel: ok", "hog: send 4096 bytes on a fresh channel: ENOMEM");
        assert!(check_scenario(&enomem).is_empty());
    }

    /// Regression: the kernel once charged a queued message's payload
    /// only, so a zero-byte message carrying capabilities parked uncharged
    /// heap. The scenario checks what `hog`'s exit drains against the full
    /// cost of a queued message, and that the budget settles at zero.
    #[test]
    fn hog_cap_only_messages_are_charged_and_released() {
        assert_eq!((HOG_CAP_ONLY_SENT, HOG_RECEIVED_BACK), (15, 8));
        assert_eq!(HOG_DRAINED_MESSAGES, 63 * 16 + 8);
        let payload_only = 63 * 16 * kalium_abi::limits::MAX_MESSAGE_LEN as u64;
        // Slot, payload and capabilities: strictly more than the payload,
        // and the cap-only messages are not free.
        assert!(HOG_DRAINED_BYTES > payload_only);
        assert!(kalium_ipc::Message::cost_of(0, 4) > 0);
        assert_eq!(parse_hog_cap_only("hog: 15 cap-only messages sent, 8 received back"), Some((15, 8)));
        assert_eq!(parse_hog_cap_only("hog: x cap-only messages sent, 8 received back"), None);
        assert_eq!(
            parse_hog_drain("kalium: instance 1 (hog) exit drained 1016 queued messages, 4179200 budget bytes"),
            Some((1016, 4179200))
        );
        assert_eq!(parse_hog_drain("kalium: instance 1 (park) exit drained 1 queued messages, 1 budget bytes"), None);
        assert_eq!(parse_budget_in_use("kalium: instance budget: 0 bytes in use"), Some(0));
        assert_eq!(parse_budget_in_use("kalium: instance budget: 4096 bytes in use"), Some(4096));
        assert_eq!(parse_budget_in_use("kalium: instance budget: x bytes in use"), None);
        let drain = format!("{HOG_DRAINED_MESSAGES} queued messages, {HOG_DRAINED_BYTES} budget bytes");
        // Payload-only accounting: the right number of messages, too few bytes.
        let payload_only_line = format!("{HOG_DRAINED_MESSAGES} queued messages, {payload_only} budget bytes");
        let f = check_scenario(&GOOD.replace(&drain, &payload_only_line));
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("found `drained"), "{f:?}");
        // A message not drained (or drained twice).
        let fewer = format!("{} queued messages, {HOG_DRAINED_BYTES} budget bytes", HOG_DRAINED_MESSAGES - 1);
        assert_eq!(check_scenario(&GOOD.replace(&drain, &fewer)).len(), 1);
        // The queue refused the cap-only messages early, or gave none back.
        for wrong in ["hog: 14 cap-only messages sent, 8 received back", "hog: 15 cap-only messages sent, 0 received back"] {
            let f = check_scenario(&GOOD.replace("hog: 15 cap-only messages sent, 8 received back", wrong));
            assert_eq!(f.len(), 1, "{f:?}");
            assert!(f[0].contains("found `"), "{f:?}");
        }
        // Something charged to the budget was never released.
        let leaked = GOOD.replace("kalium: instance budget: 0 bytes in use", "kalium: instance budget: 4096 bytes in use");
        let f = check_scenario(&leaked);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("found `4096 bytes in use`"), "{f:?}");
        let no_report = GOOD.replace("kalium: instance budget: 0 bytes in use\n", "");
        assert_eq!(check_scenario(&no_report).len(), 1);
    }

    /// Regression: a channel whose own queue held a capability on it was
    /// never freed by the reference count alone. `hog` leaves exactly
    /// that; the kernel must report collecting it, and the leak shows up
    /// again in `hog`'s channel quota and in the budget at the end.
    #[test]
    fn hog_cycle_is_collected() {
        assert_eq!((CYCLE_OBJECTS, CYCLE_MESSAGES), (1, 1));
        assert_eq!(CYCLE_BYTES, kalium_ipc::Message::cost_of(0, 1) as u64);
        let line = format!(
            "kalium: collected {CYCLE_OBJECTS} unreachable objects: {CYCLE_MESSAGES} queued messages, {CYCLE_BYTES} budget bytes"
        );
        assert_eq!(parse_collected(&line), Some((CYCLE_OBJECTS, CYCLE_MESSAGES, CYCLE_BYTES)));
        assert_eq!(
            parse_collected("kalium: collected 2 unreachable objects: 3 queued messages, 4 budget bytes"),
            Some((2, 3, 4))
        );
        assert_eq!(parse_collected("kalium: collected x unreachable objects: 3 queued messages, 4 budget bytes"), None);
        assert!(GOOD.contains(&line));
        // The pre-fix kernel: no collection, one channel and one message
        // short of the quota, the exit drain a queue short, and the
        // message's cost still in use at the end.
        let leaked = GOOD
            .replace(&format!("{line}\n"), "")
            .replace(
                "hog: memobjs 2, pages 256, channels 64, messages 1024",
                "hog: memobjs 2, pages 256, channels 63, messages 1008",
            )
            .replace(
                "kalium: instance budget: 0 bytes in use",
                &format!("kalium: instance budget: {CYCLE_BYTES} bytes in use"),
            );
        let f = check_scenario(&leaked);
        assert_eq!(f.len(), 3, "{f:?}");
        assert!(f[0].contains("collected"), "{f:?}");
        assert!(f[1].contains("found `memobjs 2, pages 256, channels 63"), "{f:?}");
        assert!(f[2].contains(&format!("found `{CYCLE_BYTES} bytes in use`")), "{f:?}");
        // A collection that freed the wrong thing, or reported it wrongly.
        for wrong in [
            "kalium: collected 2 unreachable objects: 1 queued messages, 100 budget bytes",
            "kalium: collected 1 unreachable objects: 0 queued messages, 0 budget bytes",
        ] {
            let f = check_scenario(&GOOD.replace(&line, wrong));
            assert_eq!(f.len(), 1, "{f:?}");
            assert!(f[0].contains("found `collected"), "{f:?}");
        }
        let no_step = GOOD.replace("hog: cycle: a channel's send end sent over itself, both ends dropped\n", "");
        assert_eq!(check_scenario(&no_step).len(), 1);
    }
}
