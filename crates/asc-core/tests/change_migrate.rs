use asc_core::change::ChangeId;
use asc_core::change_migrate::{import_id, migration_id};
use asc_core::id::ChangesetId;

fn cs(n: u8) -> ChangesetId {
    ChangesetId::from_bytes([n; 32])
}

#[test]
fn golden_import_id() {
    assert_eq!(import_id(&cs(1)).to_string(), "znxotwlsqoknzwwzozsstznnlzrvoqxo");
    assert_eq!(import_id(&cs(2)).to_string(), "unqwyourxlnwvpxtxvrzwuuqtyxlykyx");
}

#[test]
fn deterministic_and_distinct() {
    assert_eq!(import_id(&cs(1)), import_id(&cs(1)));
    assert_ne!(import_id(&cs(1)), import_id(&cs(2)));
    assert_eq!(
        migration_id(&cs(1), None, None, b"s"),
        migration_id(&cs(1), None, None, b"other")
    );
}

#[test]
fn jj_header_wins() {
    let jj = "zyxwvutsrqponmlkzyxwvutsrqponmlk";
    let want: ChangeId = jj.parse().unwrap();
    let got = migration_id(&cs(1), Some(jj.as_bytes()), Some(b"I1"), b"s");
    assert_eq!(got, Some(want));
}

#[test]
fn gerrit_second() {
    let got = migration_id(&cs(1), None, Some(b"I1"), b"refs/heads/a");
    assert_eq!(got, Some(ChangeId::from_gerrit(b"refs/heads/a", b"I1")));
}

#[test]
fn bad_jj_header_falls_through() {
    assert_eq!(
        migration_id(&cs(1), Some(b"nope"), Some(b"I1"), b"s"),
        Some(ChangeId::from_gerrit(b"s", b"I1"))
    );
    assert_eq!(
        migration_id(&cs(1), Some(b"nope"), Some(b""), b"s"),
        Some(import_id(&cs(1)))
    );
}

#[test]
fn import_tier_when_nothing_else() {
    assert_eq!(migration_id(&cs(3), None, None, b""), Some(import_id(&cs(3))));
}
