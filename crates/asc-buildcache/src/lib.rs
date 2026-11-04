//! The machine-wide build cache (docs/designs/build-cache.md §2): one content-addressed store
//! under `<ASC_HOME>/cache/build/`, shared by every repository, worktree and workflow of the user.
//!
//! ```text
//! cas/<2 hex>/<FileId>     output bytes as they are, written once, read-only
//! ac/<2 hex>/<action key>  action entry (JSON): the outputs and where the entry came from
//! events/<day>.jsonl       hits, misses and writes, for `asc cache status` and `show`
//! tmp/                     files being written
//! ```
//!
//! Everything is a file. Many compiler processes use the store at once (`cargo -j` starts one
//! wrapper per crate), so there is no database to lock: a blob is named by its content and
//! renamed into place, an action entry is renamed into place after its blobs, the last use of an
//! entry is its file's modification time, and events are appended a line at a time.

mod events;
mod gc;
mod key;
mod store;

pub use events::{Event, EventKind, ToolStats};
pub use gc::{GcReport, Usage};
pub use key::{ActionKey, KeyBuilder};
pub use store::{Action, EntryInfo, Origin, Output, OutputKind, Store};

/// The default size the store is kept under (`cache.build.max_size`).
pub const DEFAULT_MAX_SIZE: u64 = 20 << 30;

/// Parses a size: bytes, or a number with `K`, `M`, `G` or `T` (powers of 1024, `B`/`iB`
/// optional).
pub fn parse_size(text: &str) -> Option<u64> {
    let t = text.trim();
    let t = t
        .strip_suffix("iB")
        .or_else(|| t.strip_suffix('B'))
        .unwrap_or(t);
    let (num, shift) = match t.chars().last()? {
        'k' | 'K' => (&t[..t.len() - 1], 10),
        'm' | 'M' => (&t[..t.len() - 1], 20),
        'g' | 'G' => (&t[..t.len() - 1], 30),
        't' | 'T' => (&t[..t.len() - 1], 40),
        _ => (t, 0),
    };
    let n: u64 = num.trim().parse().ok()?;
    n.checked_mul(1u64 << shift)
}

/// `bytes` for people: `1.5 GiB`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Lowercase hex of 32 bytes.
pub fn hex(id: &[u8; 32]) -> String {
    data_encoding::HEXLOWER.encode(id)
}

/// 32 bytes from 64 hex digits.
pub fn unhex(text: &str) -> Option<[u8; 32]> {
    let v = data_encoding::HEXLOWER_PERMISSIVE
        .decode(text.as_bytes())
        .ok()?;
    v.try_into().ok()
}

/// Seconds since the Unix epoch.
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("512"), Some(512));
        assert_eq!(parse_size("20G"), Some(20 << 30));
        assert_eq!(parse_size("20GiB"), Some(20 << 30));
        assert_eq!(parse_size("1.5G"), None);
        assert_eq!(parse_size("300 M"), Some(300 << 20));
        assert_eq!(parse_size("x"), None);
        assert_eq!(parse_size("99999999999T"), None, "overflow");
        assert_eq!(human_size(10), "10 B");
        assert_eq!(human_size(3 << 29), "1.5 GiB");
    }

    #[test]
    fn hex_round_trips() {
        let id = [0xa5; 32];
        assert_eq!(unhex(&hex(&id)), Some(id));
        assert_eq!(unhex("abc"), None);
    }
}
