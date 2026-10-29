//! Every `bytes` field of the repo, workspace, projects, changes, inbox, review, ci and build-out protos carries a
//! `(asc.v1.json)` option, so the asc JSON profile (SPEC 22.4.2) never guesses how to print it.

use prost_reflect::{DescriptorPool, DynamicMessage, Kind, MessageDescriptor};

const FILES: &[&str] = &[
    "asc/v1/repo.proto",
    "asc/v1/workspace.proto",
    "asc/v1/projects.proto",
    "asc/v1/changes.proto",
    "asc/v1/inbox.proto",
    "asc/v1/review.proto",
    "asc/v1/ci.proto",
    "asc/v1/agents.proto",
    "asc/v1/aiproviders.proto",
    "asc/v1/audit.proto",
    "asc/v1/ciinsights.proto",
    "asc/v1/common.proto",
    "asc/v1/crosssearch.proto",
    "asc/v1/deps.proto",
    "asc/v1/insights.proto",
    "asc/v1/mergequeue.proto",
    "asc/v1/policy.proto",
    "asc/v1/secretscan.proto",
    "asc/v1/signing.proto",
    "asc/v1/stacks.proto",
    "asc/v1/storage.proto",
    "asc/v1/vault.proto",
    "asc/v1/workflows.proto",
];

fn walk(m: &MessageDescriptor, has: &dyn Fn(&DynamicMessage) -> bool, miss: &mut Vec<String>) {
    for f in m.fields() {
        let bytes = match f.kind() {
            Kind::Bytes => true,
            Kind::Message(e) if f.is_map() => {
                matches!(e.map_entry_value_field().kind(), Kind::Bytes)
            }
            _ => false,
        };
        if bytes && !has(&f.options()) {
            miss.push(format!("{}.{}", m.full_name(), f.name()));
        }
    }
    for n in m.child_messages() {
        if !n.is_map_entry() {
            walk(&n, has, miss);
        }
    }
}

#[test]
fn every_bytes_field_has_a_json_option() {
    let pool = DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).unwrap();
    let ext = pool
        .get_extension_by_name("asc.v1.json")
        .expect("options.proto defines asc.v1.json");
    let has = |o: &DynamicMessage| o.has_extension(&ext);
    let mut miss = Vec::new();
    let mut seen = 0;
    for name in FILES {
        let file = pool
            .get_file_by_name(name)
            .unwrap_or_else(|| panic!("{name} is not in the descriptor set"));
        for m in file.messages() {
            seen += 1;
            walk(&m, &has, &mut miss);
        }
    }
    assert!(seen > 50, "walked only {seen} messages");
    assert!(
        miss.is_empty(),
        "bytes fields without (asc.v1.json): {miss:?}"
    );
}

#[test]
fn the_walk_detects_a_missing_option() {
    let pool = DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).unwrap();
    let ext = pool.get_extension_by_name("asc.v1.json").unwrap();
    // Treat every option as absent: the walk must then report fields.
    let none = |_: &DynamicMessage| false;
    let mut miss = Vec::new();
    let m = pool.get_message_by_name("asc.v1.ObjKey").unwrap();
    walk(&m, &none, &mut miss);
    assert_eq!(miss, ["asc.v1.ObjKey.id"]);
    let id = m.get_field_by_name("id").unwrap();
    assert!(id.options().has_extension(&ext));
}

#[test]
fn kinds_are_as_designed() {
    let pool = DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET).unwrap();
    let ext = pool.get_extension_by_name("asc.v1.json").unwrap();
    let kind = |m: &str, f: &str| {
        let m = pool.get_message_by_name(m).unwrap();
        let f = m.get_field_by_name(f).unwrap();
        f.options().get_extension(&ext).as_enum_number().unwrap()
    };
    assert_eq!(kind("asc.v1.ObjKey", "id"), 1);
    assert_eq!(kind("asc.v1.ChangeInfo", "change"), 2);
    assert_eq!(kind("asc.v1.PathChange", "path"), 3);
    assert_eq!(kind("asc.v1.LogEntry", "message"), 4);
    assert_eq!(kind("asc.v1.ReadFileResponse", "data"), 5);
}
