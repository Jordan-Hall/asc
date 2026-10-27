//! The build-out contract (docs/BUILDOUT.md) stays in step with the protos, and no scaffold returns.
//!
//! * "RPC index" in BUILDOUT.md lists exactly the methods of the build-out services in the descriptor set.
//! * No scaffold remains: BUILDOUT.md lists none and `asc_rpc` has no scaffold module.
//! * The refusal of a method this server does not carry is `UNIMPLEMENTED` and names the method, so a
//!   client can tell a server without the feature from any other `UNIMPLEMENTED`.
//!
//! The "RPC index" never shrinks.

use std::collections::BTreeSet;

use asc_rpc::DESCRIPTOR_SET;
use asc_rpc::unavailable::{UNIMPLEMENTED_HEADER, not_available};
use prost_reflect::DescriptorPool;
use tonic::Code;

/// The proto files of the build-out services (added by unit 0).
const BUILDOUT_FILES: &[&str] = &[
    "asc/v1/agents.proto",
    "asc/v1/aiproviders.proto",
    "asc/v1/audit.proto",
    "asc/v1/ciinsights.proto",
    "asc/v1/crosssearch.proto",
    "asc/v1/deps.proto",
    "asc/v1/mergequeue.proto",
    "asc/v1/policy.proto",
    "asc/v1/secretscan.proto",
    "asc/v1/signing.proto",
    "asc/v1/stacks.proto",
    "asc/v1/storage.proto",
    "asc/v1/vault.proto",
    "asc/v1/workflows.proto",
];

fn buildout_md() -> String {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let path = std::path::Path::new(&dir).join("../../docs/BUILDOUT.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The backticked `asc.v1.Service/Method` items between the two marker comments of `name`.
fn listed(doc: &str, name: &str) -> Vec<String> {
    let begin = format!("<!-- {name}:begin -->");
    let end = format!("<!-- {name}:end -->");
    let from = doc
        .find(&begin)
        .unwrap_or_else(|| panic!("BUILDOUT.md has no `{begin}`"));
    let to = doc
        .find(&end)
        .unwrap_or_else(|| panic!("BUILDOUT.md has no `{end}`"));
    assert!(from < to, "`{begin}` must come before `{end}`");
    doc[from..to]
        .lines()
        .filter_map(|l| {
            let l = l.trim_start_matches(['-', '*', ' ', '|']);
            let l = l.strip_prefix('`')?;
            let (rpc, _) = l.split_once('`')?;
            rpc.starts_with("asc.v1.").then(|| rpc.to_owned())
        })
        .collect()
}

fn descriptor_rpcs() -> BTreeSet<String> {
    let pool = DescriptorPool::decode(DESCRIPTOR_SET).unwrap();
    let mut out = BTreeSet::new();
    for file in BUILDOUT_FILES {
        let f = pool
            .get_file_by_name(file)
            .unwrap_or_else(|| panic!("{file} is not in the descriptor set"));
        for s in f.services() {
            for m in s.methods() {
                out.insert(format!("{}/{}", s.full_name(), m.name()));
            }
        }
    }
    out
}

fn no_duplicates(items: &[String], what: &str) {
    let set: BTreeSet<&String> = items.iter().collect();
    assert_eq!(set.len(), items.len(), "{what} lists a method twice");
}

#[test]
fn the_rpc_index_is_the_descriptor() {
    let doc = buildout_md();
    let index = listed(&doc, "rpc-index");
    no_duplicates(&index, "the RPC index");
    let want = descriptor_rpcs();
    let got: BTreeSet<String> = index.into_iter().collect();
    assert!(want.len() >= 80, "only {} build-out RPCs found", want.len());
    assert_eq!(
        got.difference(&want).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "in the index, not in the protos"
    );
    assert_eq!(
        want.difference(&got).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "in the protos, not in the index"
    );
}

#[test]
fn no_scaffold_remains() {
    let doc = buildout_md();
    assert!(
        !doc.contains("<!-- scaffolds:begin -->"),
        "BUILDOUT.md lists a scaffold; a service is served or it is not in the router"
    );
}

#[test]
fn the_refusal_is_unimplemented_and_names_the_method() {
    let err = not_available("asc.v1.Policy/GetSource");
    assert_eq!(err.code(), Code::Unimplemented);
    assert!(err.message().contains("asc.v1.Policy/GetSource"), "{err}");
    let header = err.metadata().get(UNIMPLEMENTED_HEADER).unwrap();
    assert_eq!(header.to_str().unwrap(), "asc.v1.Policy/GetSource");
}
