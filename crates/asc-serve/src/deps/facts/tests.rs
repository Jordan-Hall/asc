use super::*;
use crate::deps::ordered::Ordered;

const NPM: &[u8] = include_bytes!("../fixtures/registry/npm-lodash-4.17.21.json");
const NPM_KEYS: &[u8] = include_bytes!("../fixtures/registry/npm-keys.json");
const CRATES_API: &[u8] = include_bytes!("../fixtures/registry/crates-itoa-1.0.9.json");
const CRATES_INDEX: &[u8] = include_bytes!("../fixtures/registry/crates-index-itoa.txt");
const PYPI: &[u8] = include_bytes!("../fixtures/registry/pypi-six-1.16.0.json");
const PYPI_SIMPLE: &[u8] = include_bytes!("../fixtures/registry/pypi-simple-six.json");
const GEM_INFO: &[u8] = include_bytes!("../fixtures/registry/rubygems-info-rack.txt");
const GEM_API: &[u8] = include_bytes!("../fixtures/registry/rubygems-rack-2.2.4.json");
const PACKAGIST: &[u8] = include_bytes!("../fixtures/registry/packagist-psr-log.json");
const SUMDB: &[u8] = include_bytes!("../fixtures/registry/sumdb-pkg-errors.txt");
const GO_MOD: &[u8] = include_bytes!("../fixtures/registry/pkg-errors-v0.9.1.mod");
const GO_ZIP: &[u8] = include_bytes!("../fixtures/registry/pkg-errors-v0.9.1.zip");

#[test]
fn an_npm_version_gives_integrity_tarball_and_the_registry_signature_verifies() {
    let f = parse_npm_version(NPM, "lodash", "4.17.21").unwrap();
    assert_eq!(
        f.integrity.as_deref(),
        Some(
            "sha512-v2kDEe57lecTulaDIuNTPy3Ry4gLGJ6Z1O3vE1krgXZNrsQ+LFTGHVxVjcXPs17LhbZVGedAJv8XZ1tvj5FvSg=="
        )
    );
    assert_eq!(
        f.shasum.as_deref(),
        Some("679591c564c3bffaae8454cf0b3df370c3d6911c")
    );
    assert_eq!(
        f.tarball.as_deref(),
        Some("https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz")
    );
    let sigs = npm_signatures(NPM);
    let keys = npm_keys(NPM_KEYS);
    assert_eq!(sigs.len(), 1);
    assert_eq!(keys.len(), 2);
    let integrity = f.integrity.as_deref().unwrap();
    assert_eq!(
        verify_npm_signature("lodash", "4.17.21", integrity, &sigs, &keys),
        Ok(true)
    );
    // A different integrity, name or version is not what was signed.
    let other = format!("sha512-{}", B64.encode([7u8; 64]));
    assert!(verify_npm_signature("lodash", "4.17.21", &other, &sigs, &keys).is_err());
    assert!(verify_npm_signature("lodash", "4.17.22", integrity, &sigs, &keys).is_err());
    assert!(verify_npm_signature("lodahs", "4.17.21", integrity, &sigs, &keys).is_err());
    // No key of the registry for the signature: nothing was verified, and nothing is claimed.
    assert_eq!(
        verify_npm_signature("lodash", "4.17.21", integrity, &sigs, &[]),
        Ok(false)
    );
    assert_eq!(
        verify_npm_signature("lodash", "4.17.21", integrity, &[], &keys),
        Ok(false)
    );
}

#[test]
fn an_npm_signature_made_with_a_fresh_key_verifies_and_a_wrong_key_does_not() {
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};
    let rng = SystemRandom::new();
    let mk = || {
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng).unwrap()
    };
    let (a, b) = (mk(), mk());
    let spki = |k: &EcdsaKeyPair| {
        let mut v = P256_SPKI_PREFIX.to_vec();
        v.extend_from_slice(k.public_key().as_ref());
        B64.encode(v)
    };
    let integrity = format!("sha512-{}", B64.encode([1u8; 64]));
    let sig = B64.encode(
        a.sign(&rng, format!("p@1.0.0:{integrity}").as_bytes())
            .unwrap()
            .as_ref(),
    );
    let sigs = vec![("A".to_owned(), sig)];
    assert_eq!(
        verify_npm_signature("p", "1.0.0", &integrity, &sigs, &[("A".into(), spki(&a))]),
        Ok(true)
    );
    assert!(
        verify_npm_signature("p", "1.0.0", &integrity, &sigs, &[("A".into(), spki(&b))]).is_err()
    );
    assert!(
        verify_npm_signature(
            "p",
            "1.0.0",
            &integrity,
            &sigs,
            &[("A".into(), "AAAA".into())]
        )
        .is_err(),
        "a key that is not a P-256 key"
    );
}

#[test]
fn an_npm_document_for_another_version_or_with_a_bad_digest_is_refused() {
    assert!(
        parse_npm_version(NPM, "lodash", "4.17.20")
            .unwrap_err()
            .contains("another")
    );
    assert!(parse_npm_version(NPM, "underscore", "4.17.21").is_err());
    let doc = |dist: &str| {
        format!(
            r#"{{"name":"p","version":"1.0.0","dist":{{"tarball":"https://r.example/p.tgz",{dist}}}}}"#
        )
        .into_bytes()
    };
    // Too short for sha512.
    let e = parse_npm_version(&doc(r#""integrity":"sha512-AAAA""#), "p", "1.0.0").unwrap_err();
    assert!(e.contains("integrity"), "{e}");
    let e = parse_npm_version(&doc(r#""shasum":"xyz""#), "p", "1.0.0").unwrap_err();
    assert!(e.contains("shasum"), "{e}");
    assert!(
        parse_npm_version(&doc(r#""x":1"#), "p", "1.0.0").is_err(),
        "neither digest"
    );
    // Only the SHA-1: the lockfile form is derived from it.
    let sha1 = "679591c564c3bffaae8454cf0b3df370c3d6911c";
    let f = parse_npm_version(&doc(&format!(r#""shasum":"{sha1}""#)), "p", "1.0.0").unwrap();
    let i = f.integrity.unwrap();
    assert!(i.starts_with("sha1-"));
    assert_eq!(sri(&i).unwrap().1, hex_decode(sha1).unwrap());
    // A sha1 integrity that disagrees with the shasum.
    let wrong = format!("sha1-{}", B64.encode([9u8; 20]));
    let e = parse_npm_version(
        &doc(&format!(r#""shasum":"{sha1}","integrity":"{wrong}""#)),
        "p",
        "1.0.0",
    )
    .unwrap_err();
    assert!(e.contains("disagree"), "{e}");
}

#[test]
fn npm_dependencies_and_install_properties_make_the_profile() {
    let doc = br#"{"name":"p","version":"1.0.0","dependencies":{"a":"^1.0.0"},"optionalDependencies":{"fsevents":"2"},
        "peerDependencies":{"react":">=16"},"peerDependenciesMeta":{"react":{"optional":true}},
        "engines":{"node":">=12"},"os":["linux"],"bin":{"p":"bin/p.js"},"hasInstallScript":true,
        "dist":{"tarball":"https://r.example/p.tgz","shasum":"679591c564c3bffaae8454cf0b3df370c3d6911c"}}"#;
    let f = parse_npm_version(doc, "p", "1.0.0").unwrap();
    assert_eq!(f.deps["dependencies:a"], "^1.0.0");
    assert_eq!(f.deps["optionalDependencies:fsevents"], "2");
    assert_eq!(f.deps["peerDependencies:react"], ">=16");
    assert_eq!(f.deps["peerDependenciesMeta:react"], r#"{"optional":true}"#);
    assert_eq!(f.traits["engines"], r#"{"node":">=12"}"#);
    assert_eq!(f.traits["hasInstallScript"], "true");
    let mut g = f.clone();
    g.deps.insert("dependencies:a".into(), "^2.0.0".into());
    g.deps.insert("dependencies:b".into(), "1".into());
    g.deps.remove("optionalDependencies:fsevents");
    g.traits
        .insert("engines".into(), r#"{"node":">=14"}"#.into());
    let d = f.differences(&g, true);
    assert_eq!(d.len(), 4, "{d:?}");
    assert!(
        d.iter()
            .any(|l| l.contains("dependencies:a") && l.contains("^2.0.0"))
    );
    assert!(
        d.iter()
            .any(|l| l.contains("dependencies:b") && l.contains("is new"))
    );
    assert!(
        d.iter()
            .any(|l| l.contains("fsevents") && l.contains("is gone"))
    );
    assert!(f.differences(&f, true).is_empty());
    // Without the install properties only the dependencies count.
    assert_eq!(f.differences(&g, false).len(), 3);
}

#[test]
fn a_crate_checksum_must_agree_between_the_api_and_the_index() {
    let api = parse_crates_version(CRATES_API, "1.0.9").unwrap();
    let idx = parse_crates_index(CRATES_INDEX, "itoa", "1.0.9").unwrap();
    assert_eq!(Some(api.clone()), idx.sha256);
    assert!(is_hex(&api, 64));
    assert!(parse_crates_version(CRATES_API, "1.0.8").is_err());
    assert!(
        parse_crates_index(CRATES_INDEX, "itoa", "9.9.9")
            .unwrap_err()
            .contains("does not list")
    );
    assert!(parse_crates_index(CRATES_INDEX, "other", "1.0.9").is_err());
    assert_eq!(crates_index_path("itoa").unwrap(), ["it", "oa", "itoa"]);
    assert_eq!(crates_index_path("a").unwrap(), ["1", "a"]);
    assert_eq!(crates_index_path("ab").unwrap(), ["2", "ab"]);
    assert_eq!(crates_index_path("Abc").unwrap(), ["3", "a", "abc"]);
    // A name outside crates.io's grammar never reaches a byte slice.
    for bad in [
        "",
        "é",
        "aé",
        "日本語",
        "ab\u{e9}",
        &"a".repeat(65),
        "a/b",
        "..",
    ] {
        assert!(crates_index_path(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn crate_dependencies_are_declared_with_kind_target_features_and_optionality() {
    let line = br#"{"name":"foo","vers":"1.0.0","cksum":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","features":{"std":[]},"yanked":false,"rust_version":"1.60","deps":[{"name":"serde","req":"^1.0","features":["b","a"],"optional":true,"default_features":false,"target":null,"kind":"normal"},{"name":"serde_core","package":"serde_ren","req":"=1.0.1","features":[],"optional":false,"default_features":true,"target":"cfg(unix)","kind":"build"},{"name":"tempfile","req":"3","features":[],"optional":false,"default_features":true,"target":null,"kind":"dev"}]}"#;
    let f = parse_crates_index(line, "foo", "1.0.0").unwrap();
    assert_eq!(f.deps["normal:serde:"], "^1.0|a,b|true|false");
    assert_eq!(f.deps["build:serde_ren:cfg(unix)"], "=1.0.1||false|true");
    assert_eq!(f.traits["rust_version"], "1.60");
    assert_eq!(crate_requirements(&f.deps, "serde"), ["^1.0"]);
    assert_eq!(crate_requirements(&f.deps, "serde-ren"), ["=1.0.1"]);
    assert!(
        crate_requirements(&f.deps, "tempfile").is_empty(),
        "dev dependencies do not constrain a lock"
    );
}

#[test]
fn a_pypi_release_lists_its_files_and_the_simple_index_must_agree() {
    let f = parse_pypi_release(PYPI, "1.16.0").unwrap();
    assert_eq!(f.files.len(), 2);
    assert_eq!(f.files[0].filename, "six-1.16.0-py2.py3-none-any.whl");
    assert!(!f.files[0].sdist && f.files[1].sdist);
    assert!(is_hex(&f.files[0].sha256, 64));
    assert!(
        f.files[0]
            .url
            .starts_with("https://files.pythonhosted.org/")
    );
    assert_eq!(
        f.traits["requires_python"],
        ">=2.7, !=3.0.*, !=3.1.*, !=3.2.*"
    );
    assert!(f.deps.is_empty());
    let simple = parse_pypi_simple(PYPI_SIMPLE).unwrap();
    cross_check_pypi(&f.files, &simple).unwrap();
    let mut tampered = simple.clone();
    tampered.insert("six-1.16.0.tar.gz".into(), "0".repeat(64));
    let e = cross_check_pypi(&f.files, &tampered).unwrap_err();
    assert!(
        e.contains("disagree") && e.contains("six-1.16.0.tar.gz"),
        "{e}"
    );
    tampered.remove("six-1.16.0.tar.gz");
    assert!(
        cross_check_pypi(&f.files, &tampered)
            .unwrap_err()
            .contains("not in the simple index")
    );
    assert!(parse_pypi_release(PYPI, "1.15.0").is_err());
}

#[test]
fn a_gem_checksum_must_agree_between_the_compact_index_and_the_api() {
    let idx = parse_compact_index(GEM_INFO, "2.2.4").unwrap();
    let api = parse_rubygems_version(GEM_API, "2.2.4").unwrap();
    assert_eq!(idx.sha256.as_deref(), Some(api.as_str()));
    assert_eq!(idx.traits["ruby"], ">= 2.3.0");
    assert!(parse_compact_index(GEM_INFO, "9.9.9").is_err());
    assert!(parse_rubygems_version(GEM_API, "2.2.3").is_err());
    let with_deps = format!(
        "---\n3.0.0 rack:>= 2&< 3,rake:>= 1|checksum:{},ruby:>= 2.5\n",
        "a".repeat(64)
    );
    let f = parse_compact_index(with_deps.as_bytes(), "3.0.0").unwrap();
    assert_eq!(f.deps["runtime:rack"], ">= 2, < 3");
    assert_eq!(f.deps["runtime:rake"], ">= 1");
    assert!(parse_compact_index(b"---\n1.0 |checksum:short\n", "1.0").is_err());
    assert!(
        parse_compact_index(b"---\n1.0 |ruby:>= 2\n", "1.0").is_err(),
        "no checksum"
    );
}

fn field<'a>(e: &'a Ordered, k: &str) -> &'a str {
    e.get(k).and_then(Ordered::as_str).unwrap_or("")
}

#[test]
fn a_minified_packagist_list_is_expanded_to_the_entry_of_a_version() {
    let f = parse_packagist_version(PACKAGIST, "psr/log", "3.0.2").unwrap();
    let e = f.entry.unwrap();
    assert_eq!(field(&e, "name"), "psr/log");
    assert_eq!(field(&e, "version"), "3.0.2");
    assert!(field(e.get("dist").unwrap(), "url").starts_with("https://api.github.com/"));
    assert!(e.get("version_normalized").is_none() && e.get("uid").is_none());
    // An older entry inherits what its file does not repeat.
    let old = parse_packagist_version(PACKAGIST, "psr/log", "3.0.0")
        .unwrap()
        .entry
        .unwrap();
    assert_eq!(field(&old, "version"), "3.0.0");
    assert_eq!(field(&old, "name"), "psr/log");
    assert!(old.get("license").is_some(), "{old:?}");
    assert_ne!(
        field(old.get("dist").unwrap(), "reference"),
        field(e.get("dist").unwrap(), "reference")
    );
    // The `v` of a lock's version is not part of the match.
    assert!(parse_packagist_version(PACKAGIST, "psr/log", "v3.0.2").is_ok());
    assert!(parse_packagist_version(PACKAGIST, "psr/log", "9.9.9").is_err());
    assert!(parse_packagist_version(PACKAGIST, "psr/other", "3.0.2").is_err());
}

#[test]
fn packagist_unset_removes_a_key_and_the_registrys_key_order_is_kept() {
    let doc = br#"{"minified":"composer/2.0","packages":{"a/b":[
        {"name":"a/b","version":"2.0.0","require":{"php":">=8"},"abandoned":true,"dist":{"url":"https://x/2","type":"zip","shasum":"","reference":"r2"}},
        {"version":"1.0.0","require":{"php":">=7"},"abandoned":"__unset","dist":{"url":"https://x/1","type":"zip","shasum":"","reference":"r1"}}]}}"#;
    let f = parse_packagist_version(doc, "a/b", "1.0.0").unwrap();
    let e = f.entry.unwrap();
    assert!(e.get("abandoned").is_none());
    assert_eq!(field(e.get("require").unwrap(), "php"), ">=7");
    assert_eq!(f.deps["require:php"], ">=7");
    // Inherited keys keep their place; the dist's own members keep the registry's order.
    let Ordered::Obj(m) = &e else { panic!() };
    assert_eq!(
        m.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        ["name", "version", "require", "dist"]
    );
    let Some(Ordered::Obj(d)) = e.get("dist") else {
        panic!()
    };
    assert_eq!(
        d.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        ["url", "type", "shasum", "reference"]
    );
    let latest = parse_packagist_version(doc, "a/b", "2.0.0")
        .unwrap()
        .entry
        .unwrap();
    assert_eq!(latest.get("abandoned"), Some(&Ordered::Bool(true)));
}

#[test]
fn go_digests_of_the_module_and_zip_match_the_checksum_database() {
    let s = parse_sumdb(SUMDB, "github.com/pkg/errors", "v0.9.1").unwrap();
    assert_eq!(s.zip, "h1:FEBLx1zS214owpjy7qsBeixbURkuhQAwrK5UwLGTwt4=");
    assert_eq!(s.module, "h1:bwawxfHBFNV+L2hUp1rHADufV3IMtnDRdf1r5NINEl0=");
    assert_eq!(
        go_mod_h1(GO_MOD),
        s.module,
        "the go.mod the proxy serves hashes to the database's record"
    );
    let z = go_zip_h1(GO_ZIP, "github.com/pkg/errors", "v0.9.1", 10_000, 64 << 20).unwrap();
    assert_eq!(
        z, s.zip,
        "the zip the proxy serves hashes to the database's record"
    );
}

#[test]
fn a_go_zip_that_is_not_the_module_or_is_too_big_is_refused() {
    let big = 64 << 20;
    let m = "github.com/pkg/errors";
    assert!(
        go_zip_h1(GO_ZIP, m, "v0.9.0", 10_000, big)
            .unwrap_err()
            .contains("outside")
    );
    assert!(
        go_zip_h1(GO_ZIP, m, "v0.9.1", 3, big)
            .unwrap_err()
            .contains("too many")
    );
    assert!(
        go_zip_h1(GO_ZIP, m, "v0.9.1", 10_000, 100)
            .unwrap_err()
            .contains("larger")
    );
    assert!(go_zip_h1(b"not a zip", "m", "v1.0.0", 10, 10).is_err());
}

#[test]
fn the_checksum_database_answer_must_name_the_module_and_carry_both_digests() {
    assert!(parse_sumdb(SUMDB, "github.com/other/errors", "v0.9.1").is_err());
    assert!(parse_sumdb(SUMDB, "github.com/pkg/errors", "v0.9.2").is_err());
    let bad = b"1\ngithub.com/a/b v1.0.0 h1:AAAA\ngithub.com/a/b v1.0.0/go.mod h1:AAAA\n";
    assert!(
        parse_sumdb(bad, "github.com/a/b", "v1.0.0")
            .unwrap_err()
            .contains("h1:")
    );
}

#[test]
fn a_go_mod_gives_its_requirements_and_go_directive() {
    let f = parse_go_mod_facts(
        b"module m\n\ngo 1.21\n\nrequire (\n\ta.com/x v1.2.3\n\tb.com/y v0.1.0 // indirect\n)\nrequire c.com/z v2.0.0\nreplace a.com/x => ../x\n",
    )
    .unwrap();
    assert_eq!(f.traits["go"], "1.21");
    assert_eq!(f.deps["require:a.com/x"], "v1.2.3");
    assert_eq!(f.deps["require:b.com/y"], "v0.1.0");
    assert_eq!(f.deps["require:c.com/z"], "v2.0.0");
    assert_eq!(f.deps.len(), 3);
}

#[test]
fn sri_checks_the_length_of_the_digest_for_its_algorithm() {
    assert!(sri(&format!("sha512-{}", B64.encode([0u8; 64]))).is_some());
    assert!(sri(&format!("sha512-{}", B64.encode([0u8; 32]))).is_none());
    assert!(sri(&format!("sha256-{}", B64.encode([0u8; 32]))).is_some());
    assert!(sri("md5-AAAA").is_none());
    assert!(sri("sha512-!!!").is_none());
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}
