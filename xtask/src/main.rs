//! Developer tasks: `cargo xtask build|run|test|wasm`. See docs/prototype-plan.md.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    eprintln!("xtask: not implemented yet (args: {args:?})");
    std::process::exit(2);
}
