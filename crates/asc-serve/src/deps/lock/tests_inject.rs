//! A registry that lies must not be able to write lines or fields of its choosing into a lockfile.
//!
//! Two layers are tested for every writer. The gate (`safe::check`) refuses the facts before anything is
//! written; and, with the gate out of the way, the writer's own escaping keeps the hostile value inside
//! the string it belongs to, which is shown by reading the result back with a parser of the format.

use super::*;
use crate::deps::facts::{FactStore, PyFile};
use crate::deps::model::Ecosystem;
use crate::deps::ordered::parse_ordered;

const HOSTILE: [&str; 8] = [
    "\"\nname = \"evil\"",
    "x\n[[package]]\nname = \"evil\"",
    "x\"]\n[evil]",
    "x # c",
    "&anchor *alias",
    "x\\",
    "x\u{2028}y",
    "x\r\n  version \"9.9.9\"",
];

fn pin(eco: Ecosystem, name: &'static str, from: &'static str, to: &'static str) -> Pin<'static> {
    Pin {
        eco,
        name,
        from,
        to,
    }
}

fn file(name: &str, url: &str, sha: &str, time: &str) -> PyFile {
    PyFile {
        filename: name.into(),
        url: url.into(),
        sha256: sha.into(),
        size: 1,
        upload_time: time.into(),
        sdist: name.ends_with(".tar.gz"),
    }
}

fn run<'a>(
    f: impl FnOnce(&LockIn<'_>) -> Outcome,
    p: &Pin<'a>,
    from: &Facts,
    to: &Facts,
) -> Outcome {
    let store = FactStore::new();
    f(&LockIn {
        pin: p,
        from,
        to,
        requirement: None,
        manifest: None,
        store: &store,
    })
}

const OLD_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn poetry_lock() -> String {
    format!(
        "[[package]]\nname = \"six\"\nversion = \"1.15.0\"\ndescription = \"x\"\noptional = false\npython-versions = \"*\"\nfiles = [\n    {{file = \"six-1.15.0.tar.gz\", hash = \"sha256:{OLD_SHA}\"}},\n]\n\n[metadata]\nlock-version = \"2.0\"\npython-versions = \"*\"\ncontent-hash = \"abc\"\n"
    )
}

fn uv_lock() -> String {
    format!(
        "version = 1\n\n[[package]]\nname = \"six\"\nversion = \"1.15.0\"\nsource = {{ registry = \"https://pypi.org/simple\" }}\nsdist = {{ url = \"https://files.pythonhosted.org/packages/aa/six-1.15.0.tar.gz\", hash = \"sha256:{OLD_SHA}\", size = 1, upload-time = \"2022-07-13T14:00:09Z\" }}\n"
    )
}

fn old_files() -> Facts {
    Facts {
        files: vec![file(
            "six-1.15.0.tar.gz",
            "https://files.pythonhosted.org/packages/aa/six-1.15.0.tar.gz",
            OLD_SHA,
            "2022-07-13T14:00:09Z",
        )],
        ..Facts::default()
    }
}

#[test]
fn the_gate_refuses_every_hostile_field_for_every_lock_format() {
    let p = pin(Ecosystem::PyPi, "six", "1.15.0", "1.16.0");
    for h in HOSTILE {
        let candidates = [
            Facts {
                tarball: Some(format!("https://r.example/{h}")),
                ..Facts::default()
            },
            Facts {
                integrity: Some(format!("sha512-{h}")),
                ..Facts::default()
            },
            Facts {
                sha256: Some(h.to_owned()),
                ..Facts::default()
            },
            Facts {
                shasum: Some(h.to_owned()),
                ..Facts::default()
            },
            Facts {
                files: vec![file(
                    &format!("a{h}"),
                    "https://f.example/a",
                    &"b".repeat(64),
                    "",
                )],
                ..Facts::default()
            },
            Facts {
                files: vec![file(
                    "a.whl",
                    &format!("https://f.example/{h}/a.whl"),
                    &"b".repeat(64),
                    "",
                )],
                ..Facts::default()
            },
            Facts {
                files: vec![file(
                    "a.whl",
                    "https://f.example/a.whl",
                    &"b".repeat(64),
                    &format!("2022-07-13T14:00:09Z{h}"),
                )],
                ..Facts::default()
            },
            Facts {
                go: Some(crate::deps::facts::GoSums {
                    zip: format!("h1:{h}"),
                    module: format!("h1:{h}"),
                }),
                ..Facts::default()
            },
        ];
        for to in candidates {
            for format in [
                Format::PackageLock,
                Format::CargoLock,
                Format::GemfileLock,
                Format::YarnLock,
                Format::PnpmLock,
                Format::PoetryLock,
                Format::UvLock,
                Format::PipfileLock,
                Format::ComposerLock,
                Format::GoSum,
            ] {
                let e = run(|c| rewrite_lock(format, "", c), &p, &old_files(), &to).unwrap_err();
                assert!(!e.is_empty(), "{format:?} {h:?}");
            }
        }
    }
}

#[test]
fn poetry_keeps_a_hostile_file_name_inside_its_string() {
    let p = pin(Ecosystem::PyPi, "six", "1.15.0", "1.16.0");
    for h in HOSTILE {
        let name = format!("six-1.16.0{h}.tar.gz");
        let to = Facts {
            files: vec![file(&name, "https://f.example/x", &"b".repeat(64), "")],
            ..Facts::default()
        };
        let out = run(
            |c| poetry::rewrite(&poetry_lock(), c),
            &p,
            &old_files(),
            &to,
        )
        .unwrap()
        .unwrap()
        .text;
        let doc: toml::Value = toml::from_str(&out).unwrap_or_else(|e| panic!("{h:?}: {e}\n{out}"));
        assert_eq!(doc["package"].as_array().unwrap().len(), 1, "{h:?}");
        let files = doc["package"][0]["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["file"].as_str().unwrap(), name, "{h:?}");
        assert!(doc.get("evil").is_none());
    }
}

#[test]
fn uv_keeps_hostile_url_digest_and_time_inside_their_strings() {
    let p = pin(Ecosystem::PyPi, "six", "1.15.0", "1.16.0");
    for h in HOSTILE {
        let url = format!("https://files.pythonhosted.org/{h}/six-1.16.0.tar.gz");
        let to = Facts {
            files: vec![file(
                "six-1.16.0.tar.gz",
                &url,
                &"b".repeat(64),
                &format!("2022-07-13T14:00:09Z{h}"),
            )],
            ..Facts::default()
        };
        let out = run(|c| uv::rewrite(&uv_lock(), c), &p, &old_files(), &to)
            .unwrap()
            .unwrap()
            .text;
        let doc: toml::Value = toml::from_str(&out).unwrap_or_else(|e| panic!("{h:?}: {e}\n{out}"));
        assert_eq!(doc["package"].as_array().unwrap().len(), 1, "{h:?}");
        assert_eq!(
            doc["package"][0]["sdist"]["url"].as_str().unwrap(),
            url,
            "{h:?}"
        );
        assert_eq!(
            doc["package"][0]["sdist"]["upload-time"].as_str().unwrap(),
            format!("2022-07-13T14:00:09Z{h}")
        );
        assert!(doc.get("evil").is_none());
    }
}

#[test]
fn yarn_keeps_a_hostile_tarball_on_its_one_line_or_refuses_it() {
    let int_a = "sha512-AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQ==";
    let sha = "a".repeat(40);
    let lock = format!(
        "# yarn lockfile v1\n\n\nlodash@^4.17.20:\n  version \"4.17.20\"\n  resolved \"https://registry.yarnpkg.com/lodash/-/lodash-4.17.20.tgz#{sha}\"\n  integrity {int_a}\n"
    );
    let p = pin(Ecosystem::Npm, "lodash", "4.17.20", "4.17.21");
    let from = Facts {
        tarball: Some("https://registry.npmjs.org/lodash/-/lodash-4.17.20.tgz".into()),
        shasum: Some(sha.clone()),
        integrity: Some(int_a.into()),
        ..Facts::default()
    };
    for h in HOSTILE {
        // A hostile tarball path that still parses as a URL on the registry's host.
        let to = Facts {
            tarball: Some(format!(
                "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz?{h}"
            )),
            shasum: Some(format!("{h}{}", "b".repeat(40))),
            integrity: Some(int_a.into()),
            ..Facts::default()
        };
        match run(|c| yarn::rewrite(&lock, c), &p, &from, &to) {
            Err(_) => {}
            Ok(Some(d)) => {
                assert_eq!(
                    d.text.lines().count(),
                    lock.lines().count(),
                    "{h:?}\n{}",
                    d.text
                );
                let resolved: Vec<&str> = d
                    .text
                    .lines()
                    .filter(|l| l.starts_with("  resolved "))
                    .collect();
                assert_eq!(resolved.len(), 1, "{h:?}");
                assert!(
                    !d.text
                        .lines()
                        .any(|l| l.starts_with("evil") || l.starts_with("  version \"9")),
                    "{h:?}"
                );
            }
            Ok(None) => panic!("nothing written for {h:?}"),
        }
    }
}

#[test]
fn pnpm_go_sum_and_the_text_rewriters_refuse_what_is_not_a_digest() {
    let p = pin(Ecosystem::Npm, "lodash", "4.17.20", "4.17.21");
    let int_a = "sha512-AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQ==";
    let lock = format!(
        "lockfileVersion: '9.0'\n\npackages:\n\n  lodash@4.17.20:\n    resolution: {{integrity: {int_a}}}\n\nsnapshots:\n\n  lodash@4.17.20: {{}}\n"
    );
    let from = Facts {
        integrity: Some(int_a.into()),
        ..Facts::default()
    };
    for h in HOSTILE {
        let to = Facts {
            integrity: Some(format!("sha512-{h}")),
            tarball: Some("https://r.example/x.tgz".into()),
            ..Facts::default()
        };
        assert!(
            run(|c| pnpm::rewrite(&lock, c), &p, &from, &to).is_err(),
            "{h:?}"
        );
        let to = Facts {
            go: Some(crate::deps::facts::GoSums {
                zip: format!("h1:{h}"),
                module: format!("h1:{h}"),
            }),
            ..Facts::default()
        };
        let gp = pin(Ecosystem::Go, "github.com/a/b", "v1.0.0", "v1.1.0");
        let sum = "github.com/a/b v1.0.0 h1:x=\ngithub.com/a/b v1.0.0/go.mod h1:y=\n";
        assert!(
            run(|c| gosum::rewrite(sum, c), &gp, &Facts::default(), &to).is_err(),
            "{h:?}"
        );
        let to = Facts {
            integrity: Some(format!("sha512-{h}")),
            tarball: Some(format!("https://r.example/{h}")),
            ..Facts::default()
        };
        let node = "{\"lockfileVersion\":3,\"packages\":{\"node_modules/lodash\":{\"version\":\"4.17.20\",\"resolved\":\"https://r.example/a.tgz\",\"integrity\":\"x\"}}}";
        let extra = crate::deps::rewrite::Extra {
            tarball: to.tarball.clone(),
            integrity: to.integrity.clone(),
            ..crate::deps::rewrite::Extra::default()
        };
        assert!(
            crate::deps::rewrite::rewrite(Format::PackageLock, node, &p, &extra).is_err(),
            "{h:?}"
        );
    }
}

#[test]
fn pipfile_and_composer_keep_hostile_values_inside_their_json_strings() {
    let p = pin(Ecosystem::PyPi, "requests", "2.28.1", "2.31.0");
    let lock = format!(
        "{{\n    \"default\": {{\n        \"requests\": {{\n            \"hashes\": [\n                \"sha256:{OLD_SHA}\"\n            ],\n            \"version\": \"==2.28.1\"\n        }}\n    }},\n    \"develop\": {{}}\n}}\n"
    );
    let from = Facts {
        files: vec![file("f", "https://x/f", OLD_SHA, "")],
        ..Facts::default()
    };
    let pf = "[packages]\nrequests = \"*\"\n";
    for h in HOSTILE {
        let to = Facts {
            files: vec![file("f", "https://x/f", h, "")],
            ..Facts::default()
        };
        let store = FactStore::new();
        let c = LockIn {
            pin: &p,
            from: &from,
            to: &to,
            requirement: None,
            manifest: Some((pf, pf)),
            store: &store,
        };
        let out = pipfile::rewrite(&lock, &c).unwrap().unwrap().text;
        let v: serde_json::Value =
            serde_json::from_str(&out).unwrap_or_else(|e| panic!("{h:?}: {e}\n{out}"));
        assert_eq!(
            v["default"]["requests"]["hashes"][0].as_str().unwrap(),
            format!("sha256:{h}")
        );
        assert_eq!(v["default"].as_object().unwrap().len(), 1);
    }

    // A composer entry: every string is written as a JSON string, whatever it holds.
    let cp = pin(Ecosystem::Packagist, "a/b", "1.0.0", "1.0.1");
    for h in HOSTILE {
        let entry = parse_ordered(&format!(
            "{{\"name\":\"a/b\",\"version\":\"1.0.1\",\"description\":{}}}",
            serde_json::to_string(h).unwrap()
        ))
        .unwrap();
        let to = Facts {
            entry: Some(entry),
            ..Facts::default()
        };
        let lock = "{\n    \"packages\": [\n        {\n            \"name\": \"a/b\",\n            \"version\": \"1.0.0\"\n        }\n    ],\n    \"packages-dev\": []\n}\n";
        let store = FactStore::new();
        let c = LockIn {
            pin: &cp,
            from: &Facts::default(),
            to: &to,
            requirement: None,
            manifest: None,
            store: &store,
        };
        let out = composer::rewrite(lock, &c).unwrap().unwrap().text;
        let v: serde_json::Value =
            serde_json::from_str(&out).unwrap_or_else(|e| panic!("{h:?}: {e}\n{out}"));
        assert_eq!(v["packages"][0]["description"].as_str().unwrap(), h);
        assert_eq!(v["packages"].as_array().unwrap().len(), 1);
        assert!(v["packages"][0].get("evil").is_none());
    }
}
