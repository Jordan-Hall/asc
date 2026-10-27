//! `go.mod` and `go.sum`.

use super::Parsed;
use crate::deps::model::{Dep, Ecosystem};
use crate::deps::version::{Scheme, Version};

const ECO: Ecosystem = Ecosystem::Go;

fn unquote(s: &str) -> &str {
    let s = s.trim();
    for q in ['"', '`'] {
        if let Some(r) = s.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            return r;
        }
    }
    s
}

fn is_version(v: &str) -> bool {
    v.starts_with('v') && Version::parse(Scheme::Semver, v).is_some()
}

/// A replacement target that is a directory, not a module.
fn is_local(path: &str) -> bool {
    path.starts_with('.') || path.starts_with('/') || path.contains('\\') || path.contains(':')
}

struct Replace {
    old: String,
    old_version: Option<String>,
    new: String,
    new_version: Option<String>,
}

struct Require {
    path: String,
    version: String,
    indirect: bool,
}

pub(super) fn go_mod(text: &str, p: &mut Parsed) -> Result<(), String> {
    let path = p.path.clone();
    let mut requires: Vec<Require> = Vec::new();
    let mut replaces: Vec<Replace> = Vec::new();
    let mut block: Option<String> = None;
    let mut saw_module = false;
    for raw in text.lines() {
        let (code, comment) = raw.split_once("//").unwrap_or((raw, ""));
        let indirect = comment.trim().starts_with("indirect");
        let code = code.trim();
        if code.is_empty() {
            continue;
        }
        let (word, rest) = match &block {
            Some(b) => {
                if code == ")" {
                    block = None;
                    continue;
                }
                (b.clone(), code)
            }
            None => {
                let (kw, rest) = code.split_once(char::is_whitespace).unwrap_or((code, ""));
                let rest = rest.trim();
                if rest == "(" {
                    block = Some(kw.to_owned());
                    continue;
                }
                (kw.to_owned(), rest)
            }
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        match word.as_str() {
            "module" => saw_module = true,
            "require" => {
                if let [m, v, ..] = fields[..] {
                    requires.push(Require {
                        path: unquote(m).to_owned(),
                        version: v.to_owned(),
                        indirect,
                    });
                }
            }
            "replace" => {
                let Some(arrow) = fields.iter().position(|f| *f == "=>") else {
                    continue;
                };
                let (left, right) = (&fields[..arrow], &fields[arrow + 1..]);
                if left.is_empty() || right.is_empty() {
                    continue;
                }
                replaces.push(Replace {
                    old: unquote(left[0]).to_owned(),
                    old_version: left.get(1).map(|v| (*v).to_owned()),
                    new: unquote(right[0]).to_owned(),
                    new_version: right.get(1).map(|v| (*v).to_owned()),
                });
            }
            _ => {}
        }
    }
    if !saw_module {
        return Err("not a go.mod: it has no module directive".into());
    }
    let mut left_out = 0usize;
    for r in requires {
        let rep = replaces
            .iter()
            .find(|x| x.old == r.path && x.old_version.as_ref().is_none_or(|v| *v == r.version));
        let (name, version) = match rep {
            Some(x) if is_local(&x.new) => {
                left_out += 1;
                continue;
            }
            Some(x) => match &x.new_version {
                Some(v) => (x.new.clone(), v.clone()),
                None => {
                    left_out += 1;
                    continue;
                }
            },
            None => (r.path, r.version),
        };
        if !is_version(&version) {
            left_out += 1;
            continue;
        }
        let mut d = Dep::lock(ECO, &name, &version, &path);
        d.direct = !r.indirect;
        p.pin(d)?;
    }
    if left_out > 0 {
        p.note(format!(
            "{left_out} requirements are replaced by a directory or have no version and are not checked"
        ));
    }
    Ok(())
}

pub(super) fn go_sum(text: &str, p: &mut Parsed) -> Result<(), String> {
    let path = p.path.clone();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        let [module, version, hash, ..] = f[..] else {
            if f.is_empty() {
                continue;
            }
            return Err("not a go.sum: a line has fewer than three fields".into());
        };
        // `module version/go.mod h1:` hashes the go.mod only; the module itself has no `/go.mod`.
        if version.ends_with("/go.mod") || !hash.starts_with("h1:") || !is_version(version) {
            continue;
        }
        p.pin(Dep::lock(ECO, module, version, &path))?;
    }
    Ok(())
}
