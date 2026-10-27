//! The questions a lockfile move asks of the registries, against a mock that serves real answers.
//!
//! The fixtures are the answers of the public registries for small, old packages. The mock is the only
//! thing between the code and them, so what is checked here is the paths asked, the second source that
//! has to agree, the signature, and that a hostile name never reaches the network.

use super::latest::Latest;
use super::model::Ecosystem;
use super::tests::{Mock, latest_with};

const NPM: &str = include_str!("fixtures/registry/npm-lodash-4.17.21.json");
const NPM_KEYS: &str = include_str!("fixtures/registry/npm-keys.json");
const CRATES_API: &str = include_str!("fixtures/registry/crates-itoa-1.0.9.json");
const CRATES_INDEX: &str = include_str!("fixtures/registry/crates-index-itoa.txt");
const PYPI: &str = include_str!("fixtures/registry/pypi-six-1.16.0.json");
const PYPI_SIMPLE: &str = include_str!("fixtures/registry/pypi-simple-six.json");
const GEM_INFO: &str = include_str!("fixtures/registry/rubygems-info-rack.txt");
const GEM_API: &str = include_str!("fixtures/registry/rubygems-rack-2.2.4.json");
const PACKAGIST: &str = include_str!("fixtures/registry/packagist-psr-log.json");
const SUMDB: &str = include_str!("fixtures/registry/sumdb-pkg-errors.txt");
const GO_MOD: &str = include_str!("fixtures/registry/pkg-errors-v0.9.1.mod");
const GO_ZIP: &[u8] = include_bytes!("fixtures/registry/pkg-errors-v0.9.1.zip");

async fn latest(m: &Mock) -> Latest {
    latest_with(m, 1_000_000).await.0
}

#[tokio::test]
async fn an_npm_package_is_checked_against_the_registrys_signature_and_the_short_question_skips_it()
{
    let m = Mock::start().await;
    m.route("/lodash/4.17.21", 200, NPM);
    m.route("/-/npm/v1/keys", 200, NPM_KEYS);
    let l = latest(&m).await;
    let short = l
        .declared(Ecosystem::Npm, "lodash", "4.17.21")
        .await
        .unwrap();
    assert!(short.verified.is_empty());
    assert_eq!(
        m.count("GET /-/npm"),
        0,
        "the keys are not asked for a short question"
    );
    let f = l.facts(Ecosystem::Npm, "lodash", "4.17.21").await.unwrap();
    assert!(
        f.verified.iter().any(|v| v.contains("signature")),
        "{:?}",
        f.verified
    );
    assert_eq!(
        f.integrity.as_deref(),
        Some(
            "sha512-v2kDEe57lecTulaDIuNTPy3Ry4gLGJ6Z1O3vE1krgXZNrsQ+LFTGHVxVjcXPs17LhbZVGedAJv8XZ1tvj5FvSg=="
        )
    );
    // The integrity is changed after the registry signed it: the signature no longer fits.
    let forged = NPM.replace("v2kDEe57lecTulaDIuNT", "v2kDEe57lecTulaDIuNU");
    assert_ne!(forged, NPM);
    m.route("/lodash/4.17.21", 200, &forged);
    let e = l
        .facts(Ecosystem::Npm, "lodash", "4.17.21")
        .await
        .unwrap_err();
    assert!(e.contains("signature"), "{e}");
}

#[tokio::test]
async fn a_crate_checksum_must_agree_between_the_api_and_the_sparse_index() {
    let m = Mock::start().await;
    m.route("/it/oa/itoa", 200, CRATES_INDEX);
    m.route("/api/v1/crates/itoa/1.0.9", 200, CRATES_API);
    let l = latest(&m).await;
    let short = l
        .declared(Ecosystem::CratesIo, "itoa", "1.0.9")
        .await
        .unwrap();
    assert!(short.sha256.is_some() && short.verified.is_empty());
    assert_eq!(m.count("GET /api/v1/crates/itoa/1.0.9"), 0);
    let f = l.facts(Ecosystem::CratesIo, "itoa", "1.0.9").await.unwrap();
    assert!(f.verified[0].contains("agree"));
    // The API says another checksum than the index.
    let doc: serde_json::Value = serde_json::from_str(CRATES_API).unwrap();
    let sum = doc["version"]["checksum"].as_str().unwrap().to_owned();
    m.route(
        "/api/v1/crates/itoa/1.0.9",
        200,
        &CRATES_API.replace(&sum, &"0".repeat(64)),
    );
    let e = l
        .facts(Ecosystem::CratesIo, "itoa", "1.0.9")
        .await
        .unwrap_err();
    assert!(e.contains("disagree"), "{e}");
    // A version the index does not list.
    let e = l
        .declared(Ecosystem::CratesIo, "itoa", "9.9.9")
        .await
        .unwrap_err();
    assert!(e.contains("does not list"), "{e}");
}

#[tokio::test]
async fn a_pypi_release_must_agree_with_the_simple_index_on_every_file() {
    let m = Mock::start().await;
    m.route("/pypi/six/1.16.0/json", 200, PYPI);
    m.route("/simple/six/", 200, PYPI_SIMPLE);
    let l = latest(&m).await;
    let f = l.facts(Ecosystem::PyPi, "Six", "1.16.0").await.unwrap();
    assert_eq!(f.files.len(), 2);
    assert!(f.verified[0].contains("every file digest"));
    let doc: serde_json::Value = serde_json::from_str(PYPI_SIMPLE).unwrap();
    let sha = doc["files"][0]["hashes"]["sha256"]
        .as_str()
        .unwrap()
        .to_owned();
    m.route(
        "/simple/six/",
        200,
        &PYPI_SIMPLE.replace(&sha, &"1".repeat(64)),
    );
    let e = l.facts(Ecosystem::PyPi, "six", "1.16.0").await.unwrap_err();
    assert!(e.contains("disagree"), "{e}");
    // Without the second source nothing is claimed.
    m.route("/simple/six/", 404, "{}");
    let e = l.facts(Ecosystem::PyPi, "six", "1.16.0").await.unwrap_err();
    assert!(e.contains("simple index"), "{e}");
    assert!(l.declared(Ecosystem::PyPi, "six", "1.16.0").await.is_ok());
}

#[tokio::test]
async fn a_gem_checksum_must_agree_between_the_compact_index_and_the_api() {
    let m = Mock::start().await;
    m.route("/info/rack", 200, GEM_INFO);
    m.route("/api/v2/rubygems/rack/versions/2.2.4.json", 200, GEM_API);
    let l = latest(&m).await;
    let f = l.facts(Ecosystem::RubyGems, "rack", "2.2.4").await.unwrap();
    assert!(f.sha256.is_some() && f.verified[0].contains("agree"));
    let doc: serde_json::Value = serde_json::from_str(GEM_API).unwrap();
    let sha = doc["sha"].as_str().unwrap().to_owned();
    m.route(
        "/api/v2/rubygems/rack/versions/2.2.4.json",
        200,
        &GEM_API.replace(&sha, &"2".repeat(64)),
    );
    let e = l
        .facts(Ecosystem::RubyGems, "rack", "2.2.4")
        .await
        .unwrap_err();
    assert!(e.contains("disagree"), "{e}");
}

#[tokio::test]
async fn a_composer_entry_is_expanded_from_the_minified_list() {
    let m = Mock::start().await;
    m.route("/p2/psr/log.json", 200, PACKAGIST);
    let l = latest(&m).await;
    let f = l
        .facts(Ecosystem::Packagist, "Psr/Log", "v3.0.2")
        .await
        .unwrap();
    assert!(f.entry.is_some() && f.tarball.is_some());
    assert!(
        l.facts(Ecosystem::Packagist, "psr/log", "9.9.9")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn go_hashes_must_match_the_checksum_database_and_the_download_is_bounded() {
    let m = Mock::start().await;
    m.route("/lookup/github.com/pkg/errors@v0.9.1", 200, SUMDB);
    m.route("/github.com/pkg/errors/@v/v0.9.1.mod", 200, GO_MOD);
    m.blob("/github.com/pkg/errors/@v/v0.9.1.zip", GO_ZIP);
    let l = latest(&m).await;
    let short = l
        .declared(Ecosystem::Go, "github.com/pkg/errors", "v0.9.1")
        .await
        .unwrap();
    assert_eq!(m.count("GET /lookup"), 0);
    assert!(
        short.go.is_some_and(|g| g.zip.is_empty()),
        "only the go.mod hash without the database"
    );
    let f = l
        .facts(Ecosystem::Go, "github.com/pkg/errors", "v0.9.1")
        .await
        .unwrap();
    let g = f.go.unwrap();
    assert_eq!(g.zip, "h1:FEBLx1zS214owpjy7qsBeixbURkuhQAwrK5UwLGTwt4=");
    assert_eq!(g.module, "h1:bwawxfHBFNV+L2hUp1rHADufV3IMtnDRdf1r5NINEl0=");
    assert!(f.verified[0].contains("checksum database"));

    // The database records another zip hash than the proxy's file gives.
    m.route(
        "/lookup/github.com/pkg/errors@v0.9.1",
        200,
        &SUMDB.replace(
            "FEBLx1zS214owpjy7qsBeixbURkuhQAwrK5UwLGTwt4=",
            "AAAAx1zS214owpjy7qsBeixbURkuhQAwrK5UwLGTwt4=",
        ),
    );
    let e = l
        .facts(Ecosystem::Go, "github.com/pkg/errors", "v0.9.1")
        .await
        .unwrap_err();
    assert!(e.contains("zip") && e.contains("does not match"), "{e}");
    m.route("/lookup/github.com/pkg/errors@v0.9.1", 200, SUMDB);

    // The proxy serves a go.mod the database does not know.
    m.route(
        "/github.com/pkg/errors/@v/v0.9.1.mod",
        200,
        "module github.com/pkg/other\n",
    );
    let e = l
        .facts(Ecosystem::Go, "github.com/pkg/errors", "v0.9.1")
        .await
        .unwrap_err();
    assert!(e.contains("go.mod") && e.contains("does not match"), "{e}");
    m.route("/github.com/pkg/errors/@v/v0.9.1.mod", 200, GO_MOD);

    // A zip larger than the bound is not read.
    let small = latest(&m).await.with_archive(1000, 5);
    let e = small
        .facts(Ecosystem::Go, "github.com/pkg/errors", "v0.9.1")
        .await
        .unwrap_err();
    assert!(e.contains("larger than the limit"), "{e}");
}

#[tokio::test]
async fn a_name_outside_the_registrys_grammar_never_reaches_the_network() {
    let m = Mock::start().await;
    let l = latest(&m).await;
    for (eco, name) in [
        (Ecosystem::CratesIo, "é"),
        (Ecosystem::CratesIo, "日本語"),
        (Ecosystem::CratesIo, ""),
        (Ecosystem::CratesIo, "a/../b"),
        (Ecosystem::Npm, "@scope"),
        (Ecosystem::Npm, "a b"),
        (Ecosystem::PyPi, "x?y"),
        (Ecosystem::Go, "github.com/é/x"),
        (Ecosystem::Go, "github.com/a/../b"),
        (Ecosystem::Maven, "g:a:v"),
        (Ecosystem::NuGet, "bad\u{0}id"),
        (Ecosystem::Packagist, "vendor"),
        (Ecosystem::RubyGems, &"a".repeat(300)),
        (Ecosystem::Hex, "UPPER-case"),
        (Ecosystem::Pub, "x%2Fy"),
    ] {
        let e1 = l.declared(eco, name, "1.0.0").await;
        let e2 = l.versions(eco, name, true).await;
        assert!(e1.is_err() && e2.is_err(), "{eco} {name:?}");
    }
    for v in ["", "../1", "1/2", "1 0", "é"] {
        assert!(l.facts(Ecosystem::Npm, "lodash", v).await.is_err(), "{v:?}");
    }
    assert!(m.hits().is_empty(), "{:?}", m.hits());
}

#[tokio::test]
async fn a_registry_that_is_switched_off_is_not_asked() {
    let m = Mock::start().await;
    let mut c = super::tests::config(&m);
    c.registries.npm.url.clear();
    let (clock, _) = super::tests::clock(1);
    let l = Latest::new(
        super::tests::egress_for(&c),
        super::tests::pool().await,
        c.registries.clone(),
        60,
        clock,
    );
    let e = l
        .facts(Ecosystem::Npm, "lodash", "4.17.21")
        .await
        .unwrap_err();
    assert!(e.contains("switched off"), "{e}");
    assert!(m.hits().is_empty());
}
