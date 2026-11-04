//! Developer tasks: `cargo xtask build|run|boot-test|wasm|test|clippy|fmt-check`.
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

/// The line the HAL boot test (`--features boot-only`) prints before exiting.
const BOOT_TEST_MARKER: &str = "kalium: 10 ticks";

/// The SDK example modules in boot-image order (`init=0, ping=1, pong=2,
/// rt=3, fair=4, faulty=5, park=6, greedy=7`); the kernel's `build.rs`
/// embeds `target/wasm/<name>.wasm` in this order.
const WASM_MODULES: [&str; 8] = [
    "init", "ping", "pong", "rt", "fair", "faulty", "park", "greedy",
];
const WASM_TARGET: &str = "wasm32-unknown-unknown";
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
        "build" => match parse_boot_only(rest, "build") {
            Some(boot_only) => build(&root, boot_only),
            None => 2,
        },
        "run" => match parse_boot_only(rest, "run") {
            Some(boot_only) => run(&root, boot_only),
            None => 2,
        },
        "boot-test" => boot_test(&root),
        "wasm" => wasm(&root),
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
         \x20 wasm                   build the SDK example modules into target/wasm/\n\
         \x20 build [--boot-only]    wasm, then the kernel for {KERNEL_TARGET} (release)\n\
         \x20 run [--boot-only]      build, then boot the kernel in QEMU\n\
         \x20 boot-test              HAL-only check: build with `boot-only`, boot, require\n\
         \x20                        `{BOOT_TEST_MARKER}` and exit code 0\n\
         \x20 test                   host tests, wasm, build, boot in QEMU and check the\n\
         \x20                        M1 scenario output and exit code\n\
         \x20 clippy                 clippy with -D warnings on the host crates, the kernel\n\
         \x20                        and the SDK\n\
         \x20 fmt-check              cargo fmt --check on both workspaces\n\
         \n\
         KALIUM_QEMU_TIMEOUT=<seconds> overrides the {QEMU_TIMEOUT_SECS} s QEMU limit."
    );
}

/// Parses the optional `--boot-only` flag; `None` on a bad argument.
fn parse_boot_only(args: &[String], cmd: &str) -> Option<bool> {
    let mut boot_only = false;
    for arg in args {
        match arg.as_str() {
            "--boot-only" => boot_only = true,
            other => {
                eprintln!("xtask: unknown argument to `{cmd}`: `{other}`");
                return None;
            }
        }
    }
    Some(boot_only)
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
        &[
            "build",
            "--release",
            "--target",
            WASM_TARGET,
            "--manifest-path",
            SDK_MANIFEST,
            "--target-dir",
            WASM_BUILD_DIR,
        ],
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

/// Whether every boot image module is present in `target/wasm/`.
fn wasm_present(root: &Path) -> bool {
    WASM_MODULES.iter().all(|name| {
        root.join(WASM_OUT_DIR)
            .join(format!("{name}.wasm"))
            .is_file()
    })
}

/// `cargo build -p kalium-kernel --target aarch64-unknown-none --release`,
/// after `wasm` unless `boot_only` (which embeds no boot image).
fn build(root: &Path, boot_only: bool) -> i32 {
    if !boot_only {
        let code = wasm(root);
        if code != 0 {
            return code;
        }
    }
    let mut args = vec![
        "build",
        "-p",
        "kalium-kernel",
        "--target",
        KERNEL_TARGET,
        "--release",
    ];
    if boot_only {
        args.extend(["--features", "boot-only"]);
    }
    run_cargo(
        root,
        &format!(
            "building kernel for {KERNEL_TARGET}{}",
            if boot_only { " (boot-only)" } else { "" }
        ),
        &args,
    )
}

/// The QEMU command line from the plan.
fn qemu_command(root: &Path) -> Command {
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
fn run(root: &Path, boot_only: bool) -> i32 {
    let code = build(root, boot_only);
    if code != 0 {
        return code;
    }
    let timeout = qemu_timeout();
    eprintln!("xtask: starting QEMU (timeout {} s)", timeout.as_secs());
    let mut child = match qemu_command(root).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("xtask: failed to start {QEMU}: {e}");
            return 1;
        }
    };
    match wait_with_timeout(&mut child, timeout) {
        Ok(Some(status)) => status_code(status),
        Ok(None) => {
            eprintln!(
                "xtask: QEMU timed out after {} s, killed",
                timeout.as_secs()
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

/// What a captured QEMU run produced.
struct Captured {
    status: Option<ExitStatus>,
    stdout: String,
}

/// Boots the already built kernel in QEMU with the console captured, prints
/// the console, and returns the output and exit status (`None` on timeout).
fn boot_captured(root: &Path, label: &str) -> Result<Captured, i32> {
    let timeout = qemu_timeout();
    eprintln!(
        "xtask: {label}: starting QEMU (timeout {} s)",
        timeout.as_secs()
    );
    let mut child = match qemu_command(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
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
    eprintln!(
        "xtask: {label}: QEMU ran {:.1} s",
        started.elapsed().as_secs_f64()
    );

    match outcome {
        Ok(status) => Ok(Captured {
            status,
            stdout: out,
        }),
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

/// HAL-only check: build with `boot-only`, boot in QEMU, require the tick
/// marker and exit code 0.
fn boot_test(root: &Path) -> i32 {
    let code = build(root, true);
    if code != 0 {
        return code;
    }
    let captured = match boot_captured(root, "boot-test") {
        Ok(c) => c,
        Err(code) => return code,
    };
    let mut failures = Vec::new();
    match captured.status {
        None => failures.push(format!(
            "QEMU timed out after {} s",
            qemu_timeout().as_secs()
        )),
        Some(s) if s.code() != Some(0) => {
            failures.push(format!("QEMU exited with {s}, expected code 0"))
        }
        Some(_) => {}
    }
    if !captured.stdout.contains(BOOT_TEST_MARKER) {
        failures.push(format!(
            "console output does not contain `{BOOT_TEST_MARKER}`"
        ));
    }
    let code = report("boot-test", &failures);
    if captured.status.is_none() {
        EXIT_TIMEOUT
    } else {
        code
    }
}

/// The full M1 check: host tests, wasm, kernel, QEMU, scenario output.
fn test(root: &Path) -> i32 {
    let code = run_cargo(
        root,
        "running host tests (workspace default members)",
        &["test"],
    );
    if code != 0 {
        return code;
    }
    let code = run_cargo(
        root,
        "running SDK host tests",
        &[
            "test",
            "--manifest-path",
            SDK_MANIFEST,
            "-p",
            "kalium-sdk",
            "--target-dir",
            WASM_BUILD_DIR,
        ],
    );
    if code != 0 {
        return code;
    }
    let code = build(root, false);
    if code != 0 {
        return code;
    }
    let captured = match boot_captured(root, "test") {
        Ok(c) => c,
        Err(code) => return code,
    };
    let mut failures = Vec::new();
    match captured.status {
        None => failures.push(format!(
            "QEMU timed out after {} s",
            qemu_timeout().as_secs()
        )),
        Some(s) if s.code() != Some(0) => {
            failures.push(format!("QEMU exited with {s}, expected code 0"))
        }
        Some(_) => {}
    }
    failures.extend(check_scenario(&captured.stdout));
    let code = report("test", &failures);
    if captured.status.is_none() {
        EXIT_TIMEOUT
    } else {
        code
    }
}

/// `rt` must meet at least this many of its 400 periods (95%).
const RT_PERIODS: u32 = 400;
const RT_MIN_MET: u32 = 380;
/// How many `park` children `greedy` spawns before the kernel-wide instance
/// limit refuses the next one: everything but `init` and `greedy` itself.
const GREEDY_CHILDREN: u32 = kalium_abi::limits::MAX_INSTANCES as u32 - 2;

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
}

impl Expect {
    fn matches(&self, line: &str) -> bool {
        match self {
            Expect::Line(l) => line == *l,
            Expect::FaultyTrap => {
                line.starts_with("kalium: instance ")
                    && line.contains("(faulty) trapped: out of bounds memory access")
            }
            Expect::RtMet => parse_rt_met(line)
                .is_some_and(|(met, total)| total == RT_PERIODS && met >= RT_MIN_MET),
            Expect::GreedyLimit => parse_greedy_refused(line) == Some(GREEDY_CHILDREN),
        }
    }

    fn describe(&self) -> String {
        match self {
            Expect::Line(l) => format!("`{l}`"),
            Expect::FaultyTrap => {
                "`kalium: instance <id> (faulty) trapped: out of bounds memory access`".into()
            }
            Expect::RtMet => format!("`rt: met N/{RT_PERIODS}` with N >= {RT_MIN_MET}"),
            Expect::GreedyLimit => {
                format!("`greedy: spawn refused (ENOMEM) after {GREEDY_CHILDREN} instances`")
            }
        }
    }
}

/// Parses `greedy: spawn refused (ENOMEM) after N instances` into `N`.
fn parse_greedy_refused(line: &str) -> Option<u32> {
    line.strip_prefix("greedy: spawn refused (ENOMEM) after ")?
        .strip_suffix(" instances")?
        .parse()
        .ok()
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
    Expect::GreedyLimit,
    Expect::Line("greedy: slot reclaimed, park exited 0"),
    Expect::Line("init: greedy exited 0"),
    Expect::Line("init: done"),
    Expect::Line("kalium: init exited 0"),
];

/// Checks the console output of the M1 scenario. Returns the failures.
fn check_scenario(out: &str) -> Vec<String> {
    let lines: Vec<&str> = out.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut failures = Vec::new();

    let mut pos = 0;
    for expect in ORDERED {
        match lines[pos.min(lines.len())..]
            .iter()
            .position(|l| expect.matches(l))
        {
            Some(i) => pos += i + 1,
            None => {
                let detail = match expect {
                    Expect::RtMet => match lines.iter().find_map(|l| parse_rt_met(l)) {
                        Some((met, total)) => format!(" (found `rt: met {met}/{total}`)"),
                        None => String::new(),
                    },
                    Expect::GreedyLimit => {
                        match lines.iter().find_map(|l| parse_greedy_refused(l)) {
                            Some(n) => format!(" (found `... after {n} instances`)"),
                            None => String::new(),
                        }
                    }
                    _ => String::new(),
                };
                failures.push(format!(
                    "missing {} after line {pos}{detail}",
                    expect.describe()
                ));
            }
        }
    }

    // `ping` may exit before or after `pong` prints, but init reports it
    // before `pong`.
    let ping = lines.iter().position(|l| *l == "init: ping exited 0");
    let pong = lines.iter().position(|l| *l == "init: pong exited 0");
    match (ping, pong) {
        (None, _) => failures.push("missing `init: ping exited 0`".into()),
        (Some(a), Some(b)) if a > b => {
            failures.push("`init: ping exited 0` must come before `init: pong exited 0`".into())
        }
        _ => {}
    }

    for bad in [
        "kalium: panic",
        "kalium: deadlock",
        "still running",
        "kalium: unhandled",
    ] {
        if let Some(l) = lines.iter().find(|l| l.contains(bad)) {
            failures.push(format!("unexpected console line: `{l}`"));
        }
    }
    failures
}

/// `cargo clippy` with `-D warnings` on the host crates (all targets), the
/// kernel (aarch64) and the SDK (wasm32).
fn clippy(root: &Path) -> i32 {
    let code = run_cargo(
        root,
        "clippy: host crates",
        &[
            "clippy",
            "--workspace",
            "--exclude",
            "kalium-kernel",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
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
        &[
            "clippy",
            "-p",
            "kalium-kernel",
            "--target",
            KERNEL_TARGET,
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
    )
}

/// `cargo fmt --all -- --check` on both workspaces.
fn fmt_check(root: &Path) -> i32 {
    let code = run_cargo(
        root,
        "fmt-check: workspace",
        &["fmt", "--all", "--", "--check"],
    );
    if code != 0 {
        return code;
    }
    run_cargo(
        root,
        "fmt-check: SDK",
        &[
            "fmt",
            "--all",
            "--manifest-path",
            SDK_MANIFEST,
            "--",
            "--check",
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "kalium: boot\n\
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
        kalium: instance 1 (faulty) exited -1\n\
        init: faulty exited -1\n\
        kalium: instance 0 (init) spawned instance 1 (greedy, fair)\n\
        kalium: instance 1 (greedy) spawned instance 2 (park, fair)\n\
        greedy: spawn refused (ENOMEM) after 30 instances\n\
        kalium: instance 2 (park) exited 0\n\
        greedy: slot reclaimed, park exited 0\n\
        kalium: instance 1 (greedy) exited 0\n\
        init: greedy exited 0\n\
        init: done\n\
        kalium: init exited 0\n";

    #[test]
    fn good_output_passes() {
        assert!(
            check_scenario(GOOD).is_empty(),
            "{:?}",
            check_scenario(GOOD)
        );
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
        let no_trap = GOOD.replace(
            "kalium: instance 1 (faulty) trapped: out of bounds memory access\n",
            "",
        );
        assert_eq!(check_scenario(&no_trap).len(), 1);
        let late_ping = GOOD
            .replace("init: ping exited 0\n", "")
            .replace("init: done\n", "init: ping exited 0\ninit: done\n");
        assert!(check_scenario(&late_ping)[0].contains("before"));
        let panicked = format!("{GOOD}kalium: panic at x: y\n");
        assert_eq!(check_scenario(&panicked).len(), 1);
    }

    #[test]
    fn greedy_must_hit_the_limit_exactly() {
        assert_eq!(GREEDY_CHILDREN, 30);
        assert_eq!(
            parse_greedy_refused("greedy: spawn refused (ENOMEM) after 30 instances"),
            Some(30)
        );
        assert_eq!(
            parse_greedy_refused("greedy: spawn refused (ENOMEM) after x instances"),
            None
        );
        // One child too many or too few means the kernel's limit is off.
        for n in [29, 31] {
            let off = GOOD.replace("after 30 instances", &format!("after {n} instances"));
            let f = check_scenario(&off);
            assert_eq!(f.len(), 1, "{f:?}");
            assert!(f[0].contains(&format!("found `... after {n} instances`")));
        }
        // A kernel without the limit: greedy gives up and init fails.
        let unlimited = GOOD
            .replace(
                "greedy: spawn refused (ENOMEM) after 30 instances\n",
                "greedy: no instance limit\n",
            )
            .replace("greedy: slot reclaimed, park exited 0\n", "")
            .replace("init: greedy exited 0", "init: greedy exited 1");
        assert_eq!(check_scenario(&unlimited).len(), 3);
    }
}
