//! The `asc` binary.

fn main() {
    // `--version` is the only behaviour until the CLI frame lands (U20).
    println!("asc {}", env!("CARGO_PKG_VERSION"));
}
