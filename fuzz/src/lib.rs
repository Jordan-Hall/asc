//! Bodies of the fuzz targets (SPEC §17.3). Each `fuzz_targets/<name>.rs` calls the function of
//! the same name, and `tests/regressions.rs` replays `regressions/<name>/*` through it, so a
//! crash input committed there fails `cargo test` until the bug is fixed.
//!
//! Every function must return normally on any input: no panic, no unbounded allocation.
//! Decoders also assert `encode(decode(b)) == b` whenever decode succeeds.

use std::time::SystemTime;

use asc_core::change::ChangeMeta;
use asc_core::codec::{Enveloped, decode_envelope};
use asc_core::git::{canonicalise_commit, canonicalise_tag, parse_commit, parse_tag, render_tree};
use asc_core::id::{ChangesetId, TreeId};
use asc_core::object::{Changeset, ChunkList, TagRecord, Tree};
use asc_core::path::{Name, RefName, RepoPath, casefold, checkout_safe, windows_valid};

/// Every target, in the order `run-all.sh` runs them.
pub const TARGETS: &[(&str, fn(&[u8]))] = &[
    ("envelope_decode", envelope_decode),
    (
        "git_commit_parse_canonicalise",
        git_commit_parse_canonicalise,
    ),
    ("git_tree_parse", git_tree_parse),
    ("git_tag_parse", git_tag_parse),
    ("index_read", index_read),
    ("revspec_parse", revspec_parse),
    ("patch_parse", patch_parse),
    ("path_validate", path_validate),
    ("proto_convert", proto_convert),
    ("bucket_ref_entry", bucket_ref_entry),
    ("projects_toml", projects_toml),
    ("jsonc", jsonc),
    ("lock_bun", lock_bun),
    ("lock_cargo", lock_cargo),
    ("lock_nuget", lock_nuget),
    ("lock_uv", lock_uv),
    ("ssh_command", ssh_command),
];

fn envelope_round_trip<T: Enveloped>(data: &[u8]) {
    if let Ok(value) = decode_envelope::<T>(data) {
        assert_eq!(value.to_envelope(), data, "decode is not canonical");
    }
}

/// Every envelope kind.
pub fn envelope_decode(data: &[u8]) {
    envelope_round_trip::<Tree>(data);
    envelope_round_trip::<Changeset>(data);
    envelope_round_trip::<TagRecord>(data);
    envelope_round_trip::<ChunkList>(data);
    envelope_round_trip::<ChangeMeta>(data);
}

/// `parse_commit`, then the import rule with the parent count the commit names.
pub fn git_commit_parse_canonicalise(data: &[u8]) {
    let Ok(parsed) = parse_commit(data) else {
        return;
    };
    let parents = vec![ChangesetId::from_bytes([7; 32]); parsed.parents.len()];
    let _ = canonicalise_commit(data, TreeId::from_bytes([1; 32]), parents, Vec::new());
}

/// The asc tree decoder and the git tree renderer behind it.
pub fn git_tree_parse(data: &[u8]) {
    if let Ok(tree) = decode_envelope::<Tree>(data) {
        assert_eq!(tree.to_envelope(), data);
        let _ = render_tree(&tree);
    }
}

pub fn git_tag_parse(data: &[u8]) {
    if parse_tag(data).is_ok() {
        let _ = canonicalise_tag(data, ChangesetId::from_bytes([7; 32]));
    }
}

pub fn index_read(data: &[u8]) {
    if let Ok(index) = asc_git::index::IndexFile::from_bytes(data, SystemTime::UNIX_EPOCH) {
        let _ = index.to_bytes();
    }
}

pub fn revspec_parse(data: &[u8]) {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = asc_engine::revspec::parse(text);
    }
}

pub fn patch_parse(data: &[u8]) {
    for opts in [
        asc_engine::diff::apply::ParseOptions::default(),
        asc_engine::diff::apply::ParseOptions {
            strip: 0,
            recount: true,
        },
    ] {
        let _ = asc_engine::diff::apply::parse(data, &opts);
    }
}

/// `Name`, `RepoPath`, `RefName`, `checkout_safe` and case folding.
pub fn path_validate(data: &[u8]) {
    if let Ok(name) = Name::new(data) {
        assert_eq!(name.as_bytes(), data);
        let _ = windows_valid(&name);
    }
    if let Ok(path) = RepoPath::new(data) {
        assert_eq!(path.as_bytes(), data);
        let _ = checkout_safe(&path);
        assert!(path.components().all(|c| Name::new(c).is_ok()));
        let _ = path.parent();
        let _ = path.file_name();
    }
    let _ = RefName::new(data);
    let _ = RefName::new_onelevel(data);
    let once = casefold(data);
    assert_eq!(casefold(&once), once, "casefold is not idempotent");
}

/// The JSON profile of every `asc.v1` message: the first byte picks the message, the rest is
/// the JSON text (and, separately, protobuf wire bytes).
pub fn proto_convert(data: &[u8]) {
    use std::sync::OnceLock;

    use prost_reflect::{DescriptorPool, DynamicMessage};
    static POOL: OnceLock<DescriptorPool> = OnceLock::new();
    let pool = POOL.get_or_init(|| {
        DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).expect("embedded descriptor set")
    });
    let Some((&pick, rest)) = data.split_first() else {
        return;
    };
    let count = pool.all_messages().count();
    if count == 0 {
        return;
    }
    let desc = pool.all_messages().nth(usize::from(pick) % count).unwrap();
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(rest) {
        if let Ok(msg) = asc_rpc::json::decode(&desc, &value) {
            let _ = asc_rpc::json::encode(&msg);
        }
    }
    let _ = DynamicMessage::decode(desc, rest);
}

pub fn bucket_ref_entry(data: &[u8]) {
    if let Ok(entry) = asc_store::bucketrefs::BucketRefEntry::decode(data) {
        let encoded = entry.encode().expect("a decoded entry re-encodes");
        assert_eq!(&encoded[..], data, "decode is not canonical");
    }
}

pub fn projects_toml(data: &[u8]) {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = asc_graph::config::parse(text);
    }
}

pub fn jsonc(data: &[u8]) {
    let _ = asc_graph::jsonc::strip(data);
    let _ = asc_graph::jsonc::parse(data);
}

pub fn lock_bun(data: &[u8]) {
    let _ = asc_graph::lock::bun::parse(data);
}

pub fn lock_cargo(data: &[u8]) {
    let _ = asc_graph::lock::cargo::parse(data);
}

pub fn lock_nuget(data: &[u8]) {
    let _ = asc_graph::lock::nuget::parse(data);
}

pub fn lock_uv(data: &[u8]) {
    let _ = asc_graph::lock::uv::parse(data);
}

/// The SSH listener's command parser: whatever it accepts is a plain `owner/name`.
pub fn ssh_command(data: &[u8]) {
    use asc_serve::ssh::policy::{MAX_COMMAND_LINE, parse_command};
    let Ok(command) = parse_command(data) else {
        return;
    };
    assert!(data.len() <= MAX_COMMAND_LINE);
    let mut parts = command.repo.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        panic!("not owner/name: {:?}", command.repo);
    };
    for segment in [owner, name] {
        assert!(!segment.is_empty() && segment.len() <= 100);
        assert!(!segment.starts_with('.') && !segment.ends_with('.'));
        assert!(
            segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        );
    }
    // The accepted bytes are one verb, one space and one quoted word of plain bytes.
    assert!(data.iter().all(|&b| (0x20..0x7f).contains(&b)));
}

#[cfg(test)]
mod regressions {
    use std::fs;
    use std::path::Path;

    #[test]
    fn committed_crash_inputs_replay_cleanly() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("regressions");
        let mut replayed = 0;
        for (name, body) in super::TARGETS {
            let dir = root.join(name);
            assert!(dir.is_dir(), "regressions/{name}/ is missing");
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.file_name().is_some_and(|n| n == ".gitkeep") {
                    continue;
                }
                body(&fs::read(&path).unwrap());
                replayed += 1;
            }
        }
        eprintln!("replayed {replayed} regression inputs");
    }

    #[test]
    fn every_target_survives_trivial_inputs() {
        let inputs: [&[u8]; 6] = [b"", b"\0", b"asc", b"{}", b"[[", &[0xff; 64]];
        for (_, body) in super::TARGETS {
            for input in inputs {
                body(input);
            }
        }
    }

    #[test]
    fn every_target_has_a_binary_and_a_directory() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        for (name, _) in super::TARGETS {
            let bin = root.join("fuzz_targets").join(format!("{name}.rs"));
            assert!(bin.is_file(), "{name}");
            assert!(root.join("regressions").join(name).is_dir(), "{name}");
        }
    }
}
