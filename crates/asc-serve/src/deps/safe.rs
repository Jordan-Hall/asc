//! What may be written into a lockfile.
//!
//! The strings of a lockfile entry come from a registry or a mirror, so a hostile answer is a way to put
//! lines or fields of its choosing into a file the repository trusts. Nothing in [`Facts`] is written
//! before [`check`] has passed it: URLs are https, have no user name, and use a short set of characters
//! that cannot end a string, open a table or start a comment in any of the formats; file names, hashes
//! and timestamps have strict shapes. [`Facts`] fetched through [`super::latest_facts`] also have their
//! hosts checked against the registry they came from. The writers escape what they write for their own
//! format as well, in case a value gets past a check.

use url::Url;

use super::facts::{Facts, is_hex, sri};
use super::ordered::Ordered;

/// The characters a URL written into a lockfile may have. No quotes, backslash, whitespace, `#` (a
/// comment in TOML and YAML, the hash fragment of yarn), brackets, braces or control characters.
fn url_chars(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"-._~:/?@%+=,&;!$*()".contains(&c)
}

/// An https URL with a host, no user name or password, and only the characters above.
pub fn https_url(u: &str) -> bool {
    if u.is_empty() || u.len() > 1024 || !u.bytes().all(url_chars) {
        return false;
    }
    let Ok(p) = Url::parse(u) else {
        return false;
    };
    p.scheme() == "https"
        && p.host_str().is_some_and(|h| !h.is_empty())
        && p.username().is_empty()
        && p.password().is_none()
        && p.fragment().is_none()
}

/// The host of an https URL that passed [`https_url`].
pub fn host_of(u: &str) -> Option<String> {
    Url::parse(u)
        .ok()
        .and_then(|p| p.host_str().map(str::to_ascii_lowercase))
}

/// A file name of a release: letters, digits and `. _ + -`.
pub fn file_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 255
        && !n.starts_with('.')
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
}

/// `2022-07-13T14:00:09Z` or with a fraction of a second.
pub fn timestamp(t: &str) -> bool {
    let b = t.as_bytes();
    let digits =
        |r: std::ops::Range<usize>| b.get(r).is_some_and(|s| s.iter().all(u8::is_ascii_digit));
    if b.len() < 20 || b.len() > 30 {
        return false;
    }
    let shape = digits(0..4)
        && b[4] == b'-'
        && digits(5..7)
        && b[7] == b'-'
        && digits(8..10)
        && b[10] == b'T'
        && digits(11..13)
        && b[13] == b':'
        && digits(14..16)
        && b[16] == b':'
        && digits(17..19);
    if !shape {
        return false;
    }
    match &t[19..] {
        "Z" => true,
        rest => {
            rest.starts_with('.')
                && rest.ends_with('Z')
                && rest.len() >= 3
                && rest[1..rest.len() - 1].bytes().all(|d| d.is_ascii_digit())
        }
    }
}

fn reference(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= 128
        && r.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
        && !r.contains("..")
}

/// `h1:` and 32 bytes of base64.
pub fn h1(s: &str) -> bool {
    use base64::Engine as _;
    s.strip_prefix("h1:").is_some_and(|b| {
        b.len() == 44
            && base64::engine::general_purpose::STANDARD
                .decode(b)
                .is_ok_and(|v| v.len() == 32)
    })
}

fn composer_entry(e: &Ordered) -> Result<(), String> {
    fn walk(v: &Ordered, depth: usize, budget: &mut usize) -> Result<(), String> {
        if depth > 16 {
            return Err("the package entry is nested too deeply".into());
        }
        *budget = budget
            .checked_sub(1)
            .ok_or("the package entry is too large")?;
        match v {
            Ordered::Arr(a) => a.iter().try_for_each(|x| walk(x, depth + 1, budget)),
            Ordered::Obj(m) => m.iter().try_for_each(|(k, x)| {
                if k.len() > 512 {
                    return Err("a key of the package entry is too long".to_owned());
                }
                walk(x, depth + 1, budget)
            }),
            Ordered::Str(s) if s.len() > 16 * 1024 => {
                Err("a string of the package entry is too long".into())
            }
            _ => Ok(()),
        }
    }
    let mut budget = 20_000usize;
    walk(e, 0, &mut budget)?;
    for part in ["source", "dist"] {
        let Some(p) = e.get(part) else { continue };
        if let Some(u) = p.get("url").and_then(Ordered::as_str)
            && !https_url(u)
        {
            return Err(format!(
                "the {part} URL of the package is not a plain https URL"
            ));
        }
        if let Some(r) = p.get("reference").and_then(Ordered::as_str)
            && !reference(r)
        {
            return Err(format!(
                "the {part} reference of the package is not a plain reference"
            ));
        }
        if let Some(s) = p.get("shasum").and_then(Ordered::as_str)
            && !(s.is_empty() || is_hex(s, 40))
        {
            return Err("the dist shasum of the package is not 40 hex digits".into());
        }
        if let Some(t) = p.get("type").and_then(Ordered::as_str)
            && !t.bytes().all(|b| b.is_ascii_lowercase())
        {
            return Err(format!(
                "the {part} type of the package is not a plain word"
            ));
        }
    }
    for k in ["name", "version"] {
        let s = e.get(k).and_then(Ordered::as_str).unwrap_or("");
        if s.is_empty()
            || !s
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b'+'))
        {
            return Err(format!("the {k} of the package is not a plain name"));
        }
    }
    Ok(())
}

/// Refuses facts whose strings are not safe to write into a lockfile.
pub fn check(f: &Facts) -> Result<(), String> {
    if let Some(t) = &f.tarball
        && !https_url(t)
    {
        return Err("the registry's tarball URL is not a plain https URL".into());
    }
    if let Some(i) = &f.integrity
        && (i.len() > 140 || sri(i).is_none())
    {
        return Err("the registry's integrity is not shaNNN-base64 of the right length".into());
    }
    for (what, v, len) in [("shasum", &f.shasum, 40), ("checksum", &f.sha256, 64)] {
        if let Some(h) = v
            && !is_hex(h, len)
        {
            return Err(format!("the registry's {what} is not {len} hex digits"));
        }
    }
    for file in &f.files {
        if !file_name(&file.filename) {
            return Err(format!(
                "a file name of the release is not plain: {:?}",
                file.filename.chars().take(40).collect::<String>()
            ));
        }
        if !https_url(&file.url) || !file.url.ends_with(&format!("/{}", file.filename)) {
            return Err(format!(
                "the URL of {} is not a plain https URL of that file",
                file.filename
            ));
        }
        if !is_hex(&file.sha256, 64) {
            return Err(format!(
                "the digest of {} is not 64 hex digits",
                file.filename
            ));
        }
        if !(file.upload_time.is_empty() || timestamp(&file.upload_time)) {
            return Err(format!(
                "the upload time of {} is not a timestamp",
                file.filename
            ));
        }
        if file.size > 1 << 40 {
            return Err(format!("the size of {} is not plausible", file.filename));
        }
    }
    if let Some(g) = &f.go
        && (!h1(&g.module) || !(g.zip.is_empty() || h1(&g.zip)))
    {
        return Err("a Go hash is not h1: and 32 bytes of base64".into());
    }
    if let Some(e) = &f.entry {
        composer_entry(e)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::facts::PyFile;

    #[test]
    fn urls_are_plain_https() {
        assert!(https_url(
            "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz"
        ));
        assert!(https_url(
            "https://files.pythonhosted.org/packages/aa/bb/six-1.16.0.tar.gz"
        ));
        for bad in [
            "http://r.example/x.tgz",
            "https://u:p@r.example/x.tgz",
            "https://r.example/x\".tgz",
            "https://r.example/x'.tgz",
            "https://r.example/x y.tgz",
            "https://r.example/x\n.tgz",
            "https://r.example/x\\.tgz",
            "https://r.example/x#frag",
            "https://r.example/x].tgz",
            "https://r.example/x{a}.tgz",
            "https://r.example/x<a>.tgz",
            "https://r.example/\u{e9}.tgz",
            "https://",
            "",
            "ftp://r.example/x",
            "file:///etc/passwd",
        ] {
            assert!(!https_url(bad), "{bad:?}");
        }
        assert!(!https_url(&format!(
            "https://r.example/{}",
            "a".repeat(1100)
        )));
    }

    #[test]
    fn names_hashes_and_times_have_strict_shapes() {
        assert!(file_name("six-1.16.0-py2.py3-none-any.whl"));
        assert!(file_name("a+b_c-1.0.tar.gz"));
        for bad in [
            "",
            ".hidden",
            "a b",
            "a\"b",
            "a\nb",
            "a/b",
            "a#b",
            "a]b",
            "[x]",
            "\u{e9}.whl",
            &"a".repeat(300),
        ] {
            assert!(!file_name(bad), "{bad:?}");
        }
        assert!(timestamp("2022-07-13T14:00:09Z"));
        assert!(timestamp("2022-07-13T14:00:09.552Z"));
        for bad in [
            "",
            "2022-07-13",
            "2022-07-13T14:00:09",
            "2022-07-13T14:00:09.Z",
            "2022-07-13T14:00:09Z\n[x]",
            "2022-07-13 14:00:09Z",
            "x2022-07-13T14:00:09Z",
            "2022-07-13T14:00:09.5\"Z",
        ] {
            assert!(!timestamp(bad), "{bad:?}");
        }
        assert!(h1("h1:FEBLx1zS214owpjy7qsBeixbURkuhQAwrK5UwLGTwt4="));
        assert!(
            !h1("h1:AAAA")
                && !h1("h2:FEBLx1zS214owpjy7qsBeixbURkuhQAwrK5UwLGTwt4=")
                && !h1("h1:FEBLx1zS214owpjy7qsBeixbURkuhQAwrK5UwLGTwt4=\n")
        );
    }

    fn good_file() -> PyFile {
        PyFile {
            filename: "six-1.16.0.tar.gz".into(),
            url: "https://files.pythonhosted.org/packages/aa/six-1.16.0.tar.gz".into(),
            sha256: "a".repeat(64),
            size: 10,
            upload_time: "2022-07-13T14:00:09.552Z".into(),
            sdist: true,
        }
    }

    #[test]
    fn facts_with_a_hostile_string_are_refused_whichever_field_it_is_in() {
        let hostile = [
            "\"
name = \"evil\"",
            "x
[[package]]
name = \"evil\"",
            "x\"]
[evil]",
            "x # comment",
            "&anchor *alias",
            "x\\",
        ];
        let with_file = |f: PyFile| Facts {
            files: vec![f],
            ..Facts::default()
        };
        for h in hostile {
            let tarball = Facts {
                tarball: Some(format!("https://r.example/{h}")),
                ..Facts::default()
            };
            assert!(check(&tarball).is_err(), "tarball {h:?}");
            let name = with_file(PyFile {
                filename: format!("a{h}"),
                ..good_file()
            });
            assert!(check(&name).is_err(), "filename {h:?}");
            let url = with_file(PyFile {
                url: format!("https://r.example/{h}/six-1.16.0.tar.gz"),
                ..good_file()
            });
            assert!(check(&url).is_err(), "url {h:?}");
            let time = with_file(PyFile {
                upload_time: format!("2022-07-13T14:00:09Z{h}"),
                ..good_file()
            });
            assert!(check(&time).is_err(), "time {h:?}");
            let integrity = Facts {
                integrity: Some(format!("sha512-{h}")),
                ..Facts::default()
            };
            assert!(check(&integrity).is_err(), "integrity {h:?}");
            let sum = Facts {
                sha256: Some(h.to_owned()),
                ..Facts::default()
            };
            assert!(check(&sum).is_err(), "checksum {h:?}");
            let sha1 = Facts {
                shasum: Some(h.to_owned()),
                ..Facts::default()
            };
            assert!(check(&sha1).is_err(), "shasum {h:?}");
        }
        // A URL that is not of the file it is listed for.
        let other = with_file(PyFile {
            url: "https://files.pythonhosted.org/packages/aa/other.tar.gz".into(),
            ..good_file()
        });
        assert!(check(&other).is_err());
        assert!(check(&with_file(good_file())).is_ok());
        let real = Facts {
            integrity: Some(format!("sha512-{}", base64_of(64))),
            tarball: Some("https://registry.npmjs.org/x/-/x-1.0.0.tgz".into()),
            ..Facts::default()
        };
        assert!(check(&real).is_ok());
    }

    fn base64_of(n: usize) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(vec![7u8; n])
    }

    #[test]
    fn a_composer_entry_with_a_hostile_url_or_reference_is_refused() {
        use crate::deps::ordered::parse_ordered;
        let entry = |dist_url: &str, reference: &str| {
            parse_ordered(&format!(
                r#"{{"name":"a/b","version":"1.0.0","dist":{{"type":"zip","url":"{dist_url}","reference":"{reference}","shasum":""}}}}"#
            ))
            .unwrap()
        };
        let ok = Facts {
            entry: Some(entry(
                "https://api.github.com/repos/a/b/zipball/abc",
                "abc123",
            )),
            ..Facts::default()
        };
        assert!(check(&ok).is_ok());
        for (u, r) in [
            ("http://api.github.com/x", "abc"),
            ("https://api.github.com/x\\\"", "abc"),
            ("https://api.github.com/x", "abc\\\"\\n[evil]"),
            ("https://api.github.com/x", "../../etc"),
            ("https://u@api.github.com/x", "abc"),
        ] {
            let f = Facts {
                entry: Some(entry(u, r)),
                ..Facts::default()
            };
            assert!(check(&f).is_err(), "{u} {r}");
        }
        let deep = format!("{}1{}", "[".repeat(40), "]".repeat(40));
        let e = parse_ordered(&format!(
            r#"{{"name":"a/b","version":"1.0.0","extra":{deep}}}"#
        ))
        .unwrap();
        let f = Facts {
            entry: Some(e),
            ..Facts::default()
        };
        assert!(check(&f).is_err());
    }
}
