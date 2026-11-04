//! Runs the decoding/validation part of the official WebAssembly spec test
//! suite (release 2.0, vendored under `tests/spec/`, see
//! `tests/spec/README.md`).
//!
//! For every `.wast` script:
//!
//! - `(module ...)`, `(assert_unlinkable (module ...))` and
//!   `(assert_trap (module ...))` modules must decode and validate;
//! - `(assert_malformed (module ...))` must be rejected with
//!   `Error::Malformed` or `Error::Unsupported`;
//! - `(assert_invalid (module ...))` must be rejected with `Error::Invalid`;
//! - the error message must agree with the expected one (prefix match, as
//!   the reference interpreter's own test runner does).
//!
//! Execution directives (`invoke`, `assert_return`, `assert_trap` on an
//! invocation, `assert_exhaustion`, `register`) are left to the interpreter's
//! runner (`tests/spec.rs`).
//!
//! The few places where this implementation deliberately differs from the
//! reference interpreter's error category or wording are listed in
//! `ALLOWED_DIFFERENCES`, each with a reason; anything else is a failure.

use kalium_wasm::{Error, Module};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use wast::lexer::Lexer;
use wast::parser::{self, ParseBuffer};
use wast::{QuoteWat, Wast, WastDirective, WastExecute};

/// Known, accepted differences from the reference interpreter:
/// `(file, expected message, our message)`.
///
/// The decoder reports two "invalid" cases of the spec as malformed because
/// its instruction representation cannot hold them (see `decode.rs`), and
/// the reference interpreter uses a few message variants for conditions that
/// this validator reports with one stable message.
const ALLOWED_DIFFERENCES: &[(&str, &str, &str)] = &[
    // The decoder represents typed `select` with one type only and cannot
    // return a `ConstExpr` holding `br_table`, so it reports these two
    // "invalid" cases as malformed, with the spec's message.
    (
        "select.wast",
        "invalid result arity",
        "invalid result arity",
    ),
    // Limits flags 2 and 3 are the threads proposal's shared memories: the
    // decoder names the unsupported feature instead of "integer too large".
    (
        "binary.wast",
        "integer too large",
        "shared memory (threads) is not supported",
    ),
    // The `wast` crate accepts `offset=4294967296` (memory64 syntax) in a
    // quoted module, so the overflow is caught by the decoder's LEB reader.
    ("address.wast", "i32 constant", "integer too large"),
    (
        "memory.wast",
        "i32 constant out of range",
        "integer too large",
    ),
    (
        "table.wast",
        "i32 constant out of range",
        "integer too large",
    ),
    // Decoder wording: a truncated LEB128, section or body is reported as
    // running out of input before the more specific message applies.
    (
        "binary-leb128.wast",
        "integer representation too long",
        "unexpected end of section or function",
    ),
    (
        "binary-leb128.wast",
        "integer representation too long",
        "malformed function type",
    ),
    ("binary.wast", "unexpected end", "magic header not detected"),
    ("binary.wast", "unexpected end", "unknown binary version"),
    (
        "binary.wast",
        "END opcode expected",
        "unexpected end of section or function",
    ),
    (
        "binary.wast",
        "section size mismatch",
        "unexpected end of section or function",
    ),
    (
        "binary.wast",
        "illegal opcode",
        "unexpected end of section or function",
    ),
    (
        "binary.wast",
        "length out of bounds",
        "unexpected end of section or function",
    ),
    // Decoder wording: limits flags above 3 and a duplicate or misplaced
    // section are detected by a different check than the reference uses.
    (
        "binary.wast",
        "integer representation too long",
        "malformed limits flags",
    ),
    (
        "binary.wast",
        "unexpected content after last section",
        "function and code section have inconsistent lengths",
    ),
    (
        "start.wast",
        "multiple start sections",
        "unexpected content after last section",
    ),
];

#[derive(Default)]
struct Stats {
    files: usize,
    valid: usize,
    malformed: usize,
    invalid: usize,
    /// `assert_malformed`/`assert_invalid` whose text the `wast` parser
    /// itself rejected (text-format errors; nothing to feed the decoder).
    text_rejected: usize,
    skipped: usize,
}

#[derive(Default)]
struct Failures {
    /// Accepted when it should have been rejected, or vice versa.
    hard: Vec<String>,
    /// Rejected, but with the wrong error category or message.
    diff: Vec<String>,
}

fn spec_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/spec")
}

fn spec_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(spec_dir())
        .expect("tests/spec exists")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "wast"))
        .collect();
    files.sort();
    files
}

/// `names.wast` deliberately uses confusable unicode in export names, which
/// the `wast` lexer refuses by default.
fn parse_buffer(text: &str) -> wast::parser::Result<ParseBuffer<'_>> {
    let mut lexer = Lexer::new(text);
    lexer.allow_confusing_unicode(true);
    ParseBuffer::new_with_lexer(lexer)
}

fn messages_agree(expected: &str, ours: &str) -> bool {
    ours.starts_with(expected) || expected.starts_with(ours)
}

fn is_allowed(file: &str, expected: &str, ours: &str) -> bool {
    ALLOWED_DIFFERENCES
        .iter()
        .any(|(f, e, o)| *f == file && *e == expected && *o == ours)
}

fn category(e: &Error) -> &'static str {
    match e {
        Error::Malformed { .. } => "malformed",
        Error::Unsupported { .. } => "unsupported",
        Error::Invalid { .. } => "invalid",
        Error::Limit { .. } => "limit",
        _ => "other",
    }
}

struct Runner<'a> {
    file: &'a str,
    text: &'a str,
    stats: Stats,
    failures: Failures,
}

impl Runner<'_> {
    fn line(&self, span: wast::token::Span) -> usize {
        span.linecol_in(self.text).0 + 1
    }

    fn expect_valid(&mut self, span: wast::token::Span, bytes: &[u8], what: &str) {
        match Module::new(bytes) {
            Ok(_) => self.stats.valid += 1,
            Err(e) => self.failures.hard.push(format!(
                "{}:{}: {what} rejected: {e}",
                self.file,
                self.line(span)
            )),
        }
    }

    fn expect_rejected(
        &mut self,
        span: wast::token::Span,
        bytes: &[u8],
        expected_msg: &str,
        want_invalid: bool,
    ) {
        let what = if want_invalid {
            "assert_invalid"
        } else {
            "assert_malformed"
        };
        let line = self.line(span);
        match Module::new(bytes) {
            Ok(_) => self.failures.hard.push(format!(
                "{}:{line}: {what} \"{expected_msg}\" accepted",
                self.file
            )),
            Err(e) => {
                if want_invalid {
                    self.stats.invalid += 1;
                } else {
                    self.stats.malformed += 1;
                }
                let ours = e.message();
                let cat_ok = matches!(
                    (&e, want_invalid),
                    (Error::Invalid { .. }, true)
                        | (Error::Malformed { .. } | Error::Unsupported { .. }, false)
                );
                if (!cat_ok || !messages_agree(expected_msg, ours))
                    && !is_allowed(self.file, expected_msg, ours)
                {
                    self.failures.diff.push(format!(
                        "{}:{line}: {what} \"{expected_msg}\" got {} \"{ours}\"",
                        self.file,
                        category(&e)
                    ));
                }
            }
        }
    }

    fn encode(&mut self, module: &mut QuoteWat<'_>, span: wast::token::Span) -> Option<Vec<u8>> {
        match module.encode() {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                self.failures.hard.push(format!(
                    "{}:{}: wast could not encode module: {e}",
                    self.file,
                    self.line(span)
                ));
                None
            }
        }
    }

    fn directive(&mut self, directive: WastDirective<'_>) {
        match directive {
            WastDirective::Module(mut m) | WastDirective::ModuleDefinition(mut m) => {
                let span = m.span();
                if let Some(bytes) = self.encode(&mut m, span) {
                    self.expect_valid(span, &bytes, "module");
                }
            }
            WastDirective::AssertMalformed {
                span,
                mut module,
                message,
            } => match module.encode() {
                Ok(bytes) => self.expect_rejected(span, &bytes, message, false),
                // The text itself is malformed; the text parser rejected it.
                Err(_) => self.stats.text_rejected += 1,
            },
            WastDirective::AssertInvalid {
                span,
                mut module,
                message,
            } => match module.encode() {
                Ok(bytes) => self.expect_rejected(span, &bytes, message, true),
                Err(_) => self.stats.text_rejected += 1,
            },
            WastDirective::AssertUnlinkable {
                span, mut module, ..
            } => match module.encode() {
                Ok(bytes) => self.expect_valid(span, &bytes, "assert_unlinkable module"),
                Err(e) => self.failures.hard.push(format!(
                    "{}:{}: wast could not encode module: {e}",
                    self.file,
                    self.line(span)
                )),
            },
            WastDirective::AssertTrap {
                span,
                exec: WastExecute::Wat(mut module),
                ..
            } => match module.encode() {
                Ok(bytes) => self.expect_valid(span, &bytes, "assert_trap module"),
                Err(e) => self.failures.hard.push(format!(
                    "{}:{}: wast could not encode module: {e}",
                    self.file,
                    self.line(span)
                )),
            },
            WastDirective::Thread(thread) => {
                for d in thread.directives {
                    self.directive(d);
                }
            }
            _ => self.stats.skipped += 1,
        }
    }
}

fn run_file(path: &Path, stats: &mut Stats, failures: &mut Failures) {
    let file = path
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("?")
        .to_owned();
    let text = std::fs::read_to_string(path).expect("readable spec file");
    let buf = parse_buffer(&text).unwrap_or_else(|e| panic!("{file}: lexing failed: {e}"));
    let wast: Wast = parser::parse(&buf).unwrap_or_else(|e| panic!("{file}: parsing failed: {e}"));

    let mut runner = Runner {
        file: &file,
        text: &text,
        stats: Stats::default(),
        failures: Failures::default(),
    };
    for directive in wast.directives {
        runner.directive(directive);
    }
    stats.files += 1;
    stats.valid += runner.stats.valid;
    stats.malformed += runner.stats.malformed;
    stats.invalid += runner.stats.invalid;
    stats.text_rejected += runner.stats.text_rejected;
    stats.skipped += runner.stats.skipped;
    failures.hard.extend(runner.failures.hard);
    failures.diff.extend(runner.failures.diff);
}

#[test]
fn spec_suite_decodes_and_validates() {
    let files = spec_files();
    assert!(
        files.len() >= 80,
        "expected the vendored spec suite, found {} files",
        files.len()
    );

    let mut stats = Stats::default();
    let mut failures = Failures::default();
    for path in &files {
        run_file(path, &mut stats, &mut failures);
    }

    let mut report = String::new();
    let _ = writeln!(
        report,
        "spec validation: {} files, {} valid modules, {} assert_malformed, {} assert_invalid, \
         {} rejected by the text parser, {} execution directives skipped",
        stats.files,
        stats.valid,
        stats.malformed,
        stats.invalid,
        stats.text_rejected,
        stats.skipped
    );
    if !failures.hard.is_empty() {
        let _ = writeln!(report, "\n{} wrong verdicts:", failures.hard.len());
        for f in &failures.hard {
            let _ = writeln!(report, "  {f}");
        }
    }
    if !failures.diff.is_empty() {
        let _ = writeln!(
            report,
            "\n{} category/message differences (not in ALLOWED_DIFFERENCES):",
            failures.diff.len()
        );
        for f in &failures.diff {
            let _ = writeln!(report, "  {f}");
        }
    }
    println!("{report}");
    assert!(
        failures.hard.is_empty() && failures.diff.is_empty(),
        "spec validation failures:\n{report}"
    );
    // Sanity: the suite really was exercised.
    assert!(stats.valid > 500, "too few valid modules: {}", stats.valid);
    assert!(
        stats.invalid > 500,
        "too few assert_invalid: {}",
        stats.invalid
    );
    assert!(
        stats.malformed > 100,
        "too few assert_malformed: {}",
        stats.malformed
    );
}

/// Every entry of the allow-list must still be hit, so it cannot go stale.
#[test]
fn allowed_differences_are_all_used() {
    let mut used = vec![false; ALLOWED_DIFFERENCES.len()];
    for path in spec_files() {
        let file = path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("?")
            .to_owned();
        let text = std::fs::read_to_string(&path).expect("readable spec file");
        let buf = parse_buffer(&text).expect("lexes");
        let wast: Wast = parser::parse(&buf).expect("parses");
        for directive in wast.directives {
            let (mut module, message) = match directive {
                WastDirective::AssertMalformed {
                    module, message, ..
                }
                | WastDirective::AssertInvalid {
                    module, message, ..
                } => (module, message),
                _ => continue,
            };
            let Ok(bytes) = module.encode() else { continue };
            let Err(e) = Module::new(&bytes) else {
                continue;
            };
            for (i, (f, exp, ours)) in ALLOWED_DIFFERENCES.iter().enumerate() {
                if *f == file && *exp == message && *ours == e.message() {
                    used[i] = true;
                }
            }
        }
    }
    let stale: Vec<_> = ALLOWED_DIFFERENCES
        .iter()
        .zip(&used)
        .filter(|(_, u)| !**u)
        .map(|(d, _)| format!("{d:?}"))
        .collect();
    assert!(
        stale.is_empty(),
        "stale ALLOWED_DIFFERENCES entries: {stale:?}"
    );
}
