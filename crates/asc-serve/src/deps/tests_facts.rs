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
    // The fixtures are real answers, whose artifact URLs name the registries' own hosts.
    latest_with(m, 1_000_000)
        .await
        .0
        .with_artifact_hosts(vec!["registry.npmjs.org".into()])
}

/// The tiles of the real log the lookup fixture's proof needs.
fn serve_tiles(m: &Mock) {
    for (path, data) in [
        (
            "/tile/8/2/000",
            &include_bytes!("fixtures/registry/sumdb-tiles/2_000")[..],
        ),
        (
            "/tile/8/1/010",
            &include_bytes!("fixtures/registry/sumdb-tiles/1_010")[..],
        ),
        (
            "/tile/8/0/x002/760",
            &include_bytes!("fixtures/registry/sumdb-tiles/0_x002_760")[..],
        ),
        (
            "/tile/8/3/000.p/4",
            &include_bytes!("fixtures/registry/sumdb-tiles/3_000.p_4")[..],
        ),
        (
            "/tile/8/2/004.p/46",
            &include_bytes!("fixtures/registry/sumdb-tiles/2_004.p_46")[..],
        ),
        (
            "/tile/8/1/x001/070.p/213",
            &include_bytes!("fixtures/registry/sumdb-tiles/1_x001_070.p_213")[..],
        ),
        (
            "/tile/8/0/x274/133.p/94",
            &include_bytes!("fixtures/registry/sumdb-tiles/0_x274_133.p_94")[..],
        ),
    ] {
        m.blob(path, data);
    }
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
    let f = l.facts(Ecosystem::PyPi, "six", "1.16.0").await.unwrap();
    assert_eq!(f.files.len(), 2);
    assert!(f.verified[0].contains("every file digest"));
    let doc: serde_json::Value = serde_json::from_str(PYPI_SIMPLE).unwrap();
    // The fixture also lists the files of the version before; change a file of this one.
    let sha = doc["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["filename"].as_str().unwrap().contains("1.16.0"))
        .unwrap()["hashes"]["sha256"]
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

const GO_MODULE: &str = "github.com/pkg/errors";
const GO_LOOKUP: &str = "/lookup/github.com/pkg/errors@v0.9.1";

fn go_mock(m: &Mock) {
    m.route(GO_LOOKUP, 200, SUMDB);
    m.route("/github.com/pkg/errors/@v/v0.9.1.mod", 200, GO_MOD);
    m.blob("/github.com/pkg/errors/@v/v0.9.1.zip", GO_ZIP);
    serve_tiles(m);
}

#[tokio::test]
async fn go_hashes_are_believed_after_the_signed_tree_head_and_the_inclusion_proof_verify() {
    let m = Mock::start().await;
    go_mock(&m);
    let l = latest(&m).await;
    let short = l
        .declared(Ecosystem::Go, GO_MODULE, "v0.9.1")
        .await
        .unwrap();
    assert_eq!(m.count("GET /lookup"), 0);
    assert_eq!(m.count("GET /tile"), 0);
    assert!(
        short.go.is_some_and(|g| g.zip.is_empty()),
        "only the go.mod hash without the database"
    );
    let f = l.facts(Ecosystem::Go, GO_MODULE, "v0.9.1").await.unwrap();
    let g = f.go.unwrap();
    assert_eq!(g.zip, "h1:FEBLx1zS214owpjy7qsBeixbURkuhQAwrK5UwLGTwt4=");
    assert_eq!(g.module, "h1:bwawxfHBFNV+L2hUp1rHADufV3IMtnDRdf1r5NINEl0=");
    assert!(
        f.verified[0].contains("inclusion proof"),
        "{:?}",
        f.verified
    );
    assert_eq!(m.count("GET /tile"), 7, "the seven tiles of the proof");
}

#[tokio::test]
async fn a_checksum_database_answer_that_does_not_verify_is_refused() {
    let m = Mock::start().await;
    go_mock(&m);
    let l = latest(&m).await;
    async fn facts(l: &Latest) -> Result<super::facts::Facts, String> {
        l.facts(Ecosystem::Go, GO_MODULE, "v0.9.1").await
    }
    // A bad signature: one character of it changed.
    let sig = SUMDB.lines().last().unwrap().rsplit(' ').next().unwrap();
    let first = if sig.starts_with('A') { 'B' } else { 'A' };
    let flipped = format!("{first}{}", &sig[1..]);
    m.route(GO_LOOKUP, 200, &SUMDB.replace(sig, &flipped));
    let e = facts(&l).await.unwrap_err();
    assert!(e.contains("signature"), "{e}");
    // A truncated note.
    m.route(GO_LOOKUP, 200, &SUMDB[..SUMDB.len() - 80]);
    assert!(facts(&l).await.is_err());
    m.route(
        GO_LOOKUP,
        200,
        &SUMDB[..SUMDB.find("go.sum database").unwrap()],
    );
    assert!(facts(&l).await.is_err());
    // A record other than the one logged: the signature holds, the proof does not.
    m.route(
        GO_LOOKUP,
        200,
        &SUMDB.replacen("FEBLx1zS214", "AAAAx1zS214", 1),
    );
    let e = facts(&l).await.unwrap_err();
    assert!(e.contains("not in the tree"), "{e}");
    m.route(GO_LOOKUP, 200, SUMDB);
    // A tile the log does not serve.
    m.with(|s| s.blobs.remove("/tile/8/0/x002/760"));
    assert!(facts(&l).await.is_err());
    serve_tiles(&m);
    // A tile that was changed on the way.
    let mut tile = include_bytes!("fixtures/registry/sumdb-tiles/1_010").to_vec();
    tile[40] ^= 1;
    m.blob("/tile/8/1/010", &tile);
    assert!(facts(&l).await.is_err());
    serve_tiles(&m);
    assert!(facts(&l).await.is_ok(), "the untouched answer verifies");
    // A key that is not the database's: nothing it answers is believed.
    use ring::signature::KeyPair as _;
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let mut enc = vec![1u8];
    enc.extend_from_slice(pair.public_key().as_ref());
    let mut h = <sha2::Sha256 as sha2::Digest>::new();
    sha2::Digest::update(&mut h, b"sum.golang.org\n");
    sha2::Digest::update(&mut h, &enc);
    let digest = sha2::Digest::finalize(h);
    let key = format!(
        "sum.golang.org+{:02x}{:02x}{:02x}{:02x}+{}",
        digest[0],
        digest[1],
        digest[2],
        digest[3],
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &enc)
    );
    let other = latest(&m)
        .await
        .with_sumdb_key(super::sumdb::parse_key(&key).unwrap());
    let e = facts(&other).await.unwrap_err();
    assert!(e.contains("signature") || e.contains("not signed"), "{e}");
}

#[tokio::test]
async fn the_proxys_files_must_hash_to_the_records_and_the_download_is_bounded() {
    let m = Mock::start().await;
    go_mock(&m);
    let l = latest(&m).await;
    // The proxy serves a go.mod the database does not know.
    m.route(
        "/github.com/pkg/errors/@v/v0.9.1.mod",
        200,
        "module github.com/pkg/other\n",
    );
    let e = l
        .facts(Ecosystem::Go, GO_MODULE, "v0.9.1")
        .await
        .unwrap_err();
    assert!(e.contains("go.mod") && e.contains("does not match"), "{e}");
    m.route("/github.com/pkg/errors/@v/v0.9.1.mod", 200, GO_MOD);
    // ... or a zip it does not.
    let mut zip = GO_ZIP.to_vec();
    let n = zip.len();
    zip[n / 2] ^= 1;
    m.blob("/github.com/pkg/errors/@v/v0.9.1.zip", &zip);
    assert!(l.facts(Ecosystem::Go, GO_MODULE, "v0.9.1").await.is_err());
    m.blob("/github.com/pkg/errors/@v/v0.9.1.zip", GO_ZIP);
    // A zip larger than the bound is not read.
    let small = latest(&m).await.with_archive(1000, 5);
    let e = small
        .facts(Ecosystem::Go, GO_MODULE, "v0.9.1")
        .await
        .unwrap_err();
    assert!(e.contains("larger than the limit"), "{e}");
}

#[tokio::test]
async fn an_npm_package_without_a_usable_signature_is_refused_unless_the_operator_allows_it() {
    let m = Mock::start().await;
    m.route("/lodash/4.17.21", 200, NPM);
    // The keys cannot be had: the answer is an error, and the next question asks again.
    m.route("/-/npm/v1/keys", 500, "{}");
    let l = latest(&m).await;
    let e = l
        .facts(Ecosystem::Npm, "lodash", "4.17.21")
        .await
        .unwrap_err();
    assert!(e.contains("signing keys"), "{e}");
    let asked = m.count("GET /-/npm/v1/keys");
    assert!(asked >= 3, "retried: {asked}");
    m.route("/-/npm/v1/keys", 200, NPM_KEYS);
    let f = l.facts(Ecosystem::Npm, "lodash", "4.17.21").await.unwrap();
    assert!(f.verified.iter().any(|v| v.contains("signature")));
    // No key of the registry matches the signature: refused, not downgraded.
    let m2 = Mock::start().await;
    m2.route("/lodash/4.17.21", 200, NPM);
    m2.route(
        "/-/npm/v1/keys",
        200,
        r#"{"keys":[{"keyid":"SHA256:other","keytype":"ecdsa-sha2-nistp256","scheme":"ecdsa-sha2-nistp256","key":"MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE1Olb3zMAFFxXKHiIkQO5cJ3Yhl5i6UPp+IhuteBJbuHcA5UogKo0EWtlWwW6KSaKoTNEYL7JlCQiVnkhBktUgg=="}]}"#,
    );
    let e = latest(&m2)
        .await
        .facts(Ecosystem::Npm, "lodash", "4.17.21")
        .await
        .unwrap_err();
    assert!(e.contains("no key the registry publishes"), "{e}");
    // No keys at all.
    m2.route("/-/npm/v1/keys", 200, r#"{"keys":[]}"#);
    assert!(
        latest(&m2)
            .await
            .facts(Ecosystem::Npm, "lodash", "4.17.21")
            .await
            .is_err()
    );
    // A package the registry did not sign.
    let unsigned = {
        let mut doc: serde_json::Value = serde_json::from_str(NPM).unwrap();
        doc["dist"].as_object_mut().unwrap().remove("signatures");
        doc.to_string()
    };
    m2.route("/lodash/4.17.21", 200, &unsigned);
    let e = latest(&m2)
        .await
        .facts(Ecosystem::Npm, "lodash", "4.17.21")
        .await
        .unwrap_err();
    assert!(e.contains("no registry signature"), "{e}");
    // A registry the operator accepts as unsigned: used, and never reported as verified.
    let f = latest(&m2)
        .await
        .with_npm_unsigned(true)
        .facts(Ecosystem::Npm, "lodash", "4.17.21")
        .await
        .unwrap();
    assert!(f.verified.is_empty());
    assert!(f.unverified.iter().any(|v| v.contains("no signature")));
}

#[tokio::test]
async fn an_artifact_url_on_a_host_the_registry_does_not_use_is_refused() {
    let m = Mock::start().await;
    m.route("/lodash/4.17.21", 200, NPM);
    m.route("/-/npm/v1/keys", 200, NPM_KEYS);
    // No artifact host listed: the tarball names registry.npmjs.org, not the mock.
    let l = latest_with(&m, 1_000_000).await.0;
    let e = l
        .facts(Ecosystem::Npm, "lodash", "4.17.21")
        .await
        .unwrap_err();
    assert!(
        e.contains("host") && e.contains("registry.npmjs.org"),
        "{e}"
    );
    // The same host as the registry's own is fine without a list.
    let own = NPM.replace(
        "https://registry.npmjs.org",
        &m.url().replace("http://", "https://"),
    );
    m.route("/lodash/4.17.21", 200, &own);
    // (The signature no longer fits the changed URL? It signs the integrity only.)
    let e = l.facts(Ecosystem::Npm, "lodash", "4.17.21").await;
    assert!(e.is_ok(), "{e:?}");
    // PyPI files on a foreign host.
    let pypi = PYPI.replace("https://files.pythonhosted.org", "https://evil.example");
    m.route("/pypi/six/1.16.0/json", 200, &pypi);
    m.route("/simple/six/", 200, PYPI_SIMPLE);
    let e = l.facts(Ecosystem::PyPi, "six", "1.16.0").await.unwrap_err();
    assert!(e.contains("evil.example"), "{e}");
    // A Composer dist on a foreign host.
    let pkg = PACKAGIST.replace("https://api.github.com", "https://evil.example");
    m.route("/p2/psr/log.json", 200, &pkg);
    let e = l
        .facts(Ecosystem::Packagist, "psr/log", "3.0.2")
        .await
        .unwrap_err();
    assert!(e.contains("evil.example"), "{e}");
    // A URL that is not plain https is refused whatever its host.
    let bad = NPM.replace("https://registry.npmjs.org", "http://registry.npmjs.org");
    m.route("/lodash/4.17.21", 200, &bad);
    let e = latest(&m)
        .await
        .facts(Ecosystem::Npm, "lodash", "4.17.21")
        .await
        .unwrap_err();
    assert!(e.contains("plain https"), "{e}");
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
