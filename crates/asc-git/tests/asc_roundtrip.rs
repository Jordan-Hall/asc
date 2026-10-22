//! ASC round-trip milestone (U35): import, export and compare every oid of a real repository.
//! Run with `cargo test -p asc-git --release --test asc_roundtrip -- --ignored`.
//! The repositories are only read.

#[path = "../examples/roundtrip.rs"]
mod roundtrip;

use std::path::Path;
use std::time::Duration;

const ASC: &str = r"D:\dev\monorepo";
const SOMA: &str = r"D:\dev\soma";
/// SPEC §14.4 ASC targets: full import in 5 minutes, peak RSS 1 GiB.
const ASC_IMPORT_MAX: Duration = Duration::from_secs(5 * 60);
const ASC_RSS_MAX: u64 = 1 << 30;

fn assert_clean(path: &Path, report: &roundtrip::Report) {
    print!("{}", report.render(path));
    assert_eq!(
        report.mismatches, 0,
        "{} oids differ: {:?}",
        report.mismatches, report.examples
    );
    assert!(
        report.rejected_tags.is_empty(),
        "tags failed the round trip"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "reads D:\\dev\\monorepo (minutes, release build)"]
async fn asc_round_trips() {
    let asc = Path::new(ASC);
    assert!(asc.exists(), "{ASC} is missing");
    let report = roundtrip::run(asc).await.expect("asc round trip");
    assert_clean(asc, &report);
    assert!(
        report.import_time <= ASC_IMPORT_MAX,
        "asc import took {:?}, target {ASC_IMPORT_MAX:?}",
        report.import_time
    );

    let soma = Path::new(SOMA);
    if soma.exists() {
        let report = roundtrip::run(soma).await.expect("soma round trip");
        assert_clean(soma, &report);
    }

    // Peak RSS covers both imports; the target is for ASC alone.
    if let Some(rss) = roundtrip::peak_rss_bytes() {
        println!("peak RSS {} MiB", rss >> 20);
        assert!(rss <= ASC_RSS_MAX, "peak RSS {rss} bytes exceeds 1 GiB");
    }
}
