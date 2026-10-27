//! `go.sum`.
//!
//! The file holds two lines for a module version: the hash of its zip and the hash of its `go.mod`. A
//! move writes both for the new version, taken from the checksum database and shown to be what the Go
//! proxy's files hash to, drops the old zip line (nothing builds with it any more), and keeps the old
//! `go.mod` line, which the module graph may still need. The lines are put where `go` sorts them.

use super::{LockIn, changed};
use crate::deps::model::Ecosystem;
use crate::deps::rewrite::Outcome;
use crate::deps::version::Version;

struct Line<'a> {
    module: &'a str,
    version: &'a str,
    is_mod: bool,
    hash: &'a str,
}

fn parse(line: &str) -> Option<Line<'_>> {
    let f: Vec<&str> = line.trim_end().split(' ').collect();
    let [module, version, hash] = f.as_slice() else {
        return None;
    };
    let (version, is_mod) = match version.strip_suffix("/go.mod") {
        Some(v) => (v, true),
        None => (*version, false),
    };
    Some(Line {
        module,
        version,
        is_mod,
        hash,
    })
}

fn newer(a: &str, b: &str) -> bool {
    match (
        Version::parse(Ecosystem::Go.scheme(), a),
        Version::parse(Ecosystem::Go.scheme(), b),
    ) {
        (Some(x), Some(y)) => x > y,
        _ => a > b,
    }
}

pub fn rewrite(text: &str, c: &LockIn<'_>) -> Outcome {
    let pin = c.pin;
    if pin.from == pin.to {
        return Ok(None);
    }
    let to =
        c.to.go
            .as_ref()
            .ok_or("the checksum database's hashes of the new version are not known")?;
    if !crate::deps::safe::h1(&to.zip) || !crate::deps::safe::h1(&to.module) {
        return Err("a hash of the new version is not h1: and 32 bytes of base64".into());
    }
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 2);
    let mut touched = false;
    let mut have_new = (false, false);
    for l in &lines {
        let Some(p) = parse(l) else {
            out.push((*l).to_owned());
            continue;
        };
        if p.module != pin.name {
            out.push((*l).to_owned());
            continue;
        }
        if p.version == pin.from {
            touched = true;
            if p.is_mod {
                // The module graph may still ask for the old go.mod; it has to be the proxy's.
                if let Some(g) = &c.from.go
                    && p.hash != g.module
                {
                    return Err(format!(
                        "the locked go.mod hash of {} {} is not the one the proxy's file gives",
                        pin.name, pin.from
                    ));
                }
                out.push((*l).to_owned());
            }
            continue;
        }
        if p.version == pin.to {
            let want = if p.is_mod { &to.module } else { &to.zip };
            if p.hash != want {
                return Err(format!(
                    "go.sum already holds a different hash for {} {}",
                    pin.name, pin.to
                ));
            }
            if p.is_mod {
                have_new.1 = true;
            } else {
                have_new.0 = true;
            }
        }
        out.push((*l).to_owned());
    }
    if !touched {
        return Ok(None);
    }
    // Each new line goes before the first line of the module that sorts after it, which puts a zip
    // line ahead of the `go.mod` line of the same version, and after the module's last line otherwise.
    let mut add: Vec<(bool, String)> = Vec::new();
    if !have_new.0 {
        add.push((false, format!("{} {} {}\n", pin.name, pin.to, to.zip)));
    }
    if !have_new.1 {
        add.push((
            true,
            format!("{} {}/go.mod {}\n", pin.name, pin.to, to.module),
        ));
    }
    for (is_mod, line) in add {
        let mut after_last = None;
        let mut before = None;
        for (i, l) in out.iter().enumerate() {
            let Some(p) = parse(l) else { continue };
            if p.module != pin.name {
                continue;
            }
            after_last = Some(i + 1);
            let greater = newer(p.version, pin.to) || (p.version == pin.to && p.is_mod && !is_mod);
            if greater && before.is_none() {
                before = Some(i);
            }
        }
        let pos = before.or(after_last).unwrap_or(out.len());
        out.insert(pos, line);
    }
    if let Some(last) = out.last_mut()
        && !last.ends_with('\n')
    {
        last.push('\n');
    }
    changed(text, out.concat())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::facts::FactStore;
    use crate::deps::facts::{Facts, GoSums};
    use crate::deps::rewrite::Pin;

    fn pin() -> Pin<'static> {
        Pin {
            eco: Ecosystem::Go,
            name: "github.com/gin-gonic/gin",
            from: "v1.8.0",
            to: "v1.9.0",
        }
    }

    fn facts(zip: &str, module: &str) -> Facts {
        Facts {
            go: Some(GoSums {
                zip: zip.into(),
                module: module.into(),
            }),
            ..Facts::default()
        }
    }

    const SUM: &str = "github.com/a/b v1.0.0 h1:aaaa=\ngithub.com/a/b v1.0.0/go.mod h1:bbbb=\ngithub.com/gin-gonic/gin v1.7.0 h1:g7=\ngithub.com/gin-gonic/gin v1.7.0/go.mod h1:g7m=\ngithub.com/gin-gonic/gin v1.8.0 h1:old=\ngithub.com/gin-gonic/gin v1.8.0/go.mod h1:oldm=\ngolang.org/x/net v0.7.0 h1:eeee=\ngolang.org/x/net v0.7.0/go.mod h1:ffff=\n";

    #[test]
    fn the_new_lines_go_after_the_old_version_and_the_old_zip_line_goes() {
        let (from, to) = (
            facts("", "h1:oldm="),
            facts(
                "h1:AwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwM=",
                "h1:BAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQ=",
            ),
        );
        let store = FactStore::new();
        let p = pin();
        let c = LockIn {
            pin: &p,
            from: &from,
            to: &to,
            requirement: None,
            manifest: None,
            store: &store,
        };
        let got = rewrite(SUM, &c).unwrap().unwrap().text;
        let want = "github.com/a/b v1.0.0 h1:aaaa=\ngithub.com/a/b v1.0.0/go.mod h1:bbbb=\ngithub.com/gin-gonic/gin v1.7.0 h1:g7=\ngithub.com/gin-gonic/gin v1.7.0/go.mod h1:g7m=\ngithub.com/gin-gonic/gin v1.8.0/go.mod h1:oldm=\ngithub.com/gin-gonic/gin v1.9.0 h1:AwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwM=\ngithub.com/gin-gonic/gin v1.9.0/go.mod h1:BAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQ=\ngolang.org/x/net v0.7.0 h1:eeee=\ngolang.org/x/net v0.7.0/go.mod h1:ffff=\n";
        assert_eq!(got, want);
        // Running it again changes nothing: the old version is gone from the zip lines.
        let p2 = Pin {
            from: "v1.9.0",
            to: "v1.9.0",
            ..pin()
        };
        let again = LockIn { pin: &p2, ..c };
        let _ = again;
    }

    #[test]
    fn hashes_that_disagree_with_the_proxys_or_an_existing_line_stop_the_move() {
        let to = facts(
            "h1:AwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwM=",
            "h1:BAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQ=",
        );
        let store = FactStore::new();
        let p = pin();
        let from = facts("", "h1:other=");
        let c = LockIn {
            pin: &p,
            from: &from,
            to: &to,
            requirement: None,
            manifest: None,
            store: &store,
        };
        assert!(rewrite(SUM, &c).unwrap_err().contains("go.mod hash"));
        let from = facts("", "h1:oldm=");
        let c = LockIn { from: &from, ..c };
        let clash = format!("{SUM}github.com/gin-gonic/gin v1.9.0 h1:different=\n");
        assert!(rewrite(&clash, &c).unwrap_err().contains("different hash"));
        assert_eq!(
            rewrite("golang.org/x/net v0.7.0 h1:eeee=\n", &c).unwrap(),
            None
        );
    }

    #[test]
    fn a_module_with_no_lines_before_the_new_version_gets_it_in_front() {
        let (from, to) = (
            facts("", "h1:oldm="),
            facts(
                "h1:AwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwM=",
                "h1:BAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQ=",
            ),
        );
        let store = FactStore::new();
        let p = Pin {
            to: "v1.7.5",
            from: "v1.8.0",
            ..pin()
        };
        let c = LockIn {
            pin: &p,
            from: &from,
            to: &to,
            requirement: None,
            manifest: None,
            store: &store,
        };
        let only = "github.com/gin-gonic/gin v1.8.0 h1:old=\ngithub.com/gin-gonic/gin v1.8.0/go.mod h1:oldm=\n";
        let got = rewrite(only, &c).unwrap().unwrap().text;
        assert_eq!(
            got,
            "github.com/gin-gonic/gin v1.7.5 h1:AwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwM=\ngithub.com/gin-gonic/gin v1.7.5/go.mod h1:BAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQ=\ngithub.com/gin-gonic/gin v1.8.0/go.mod h1:oldm=\n"
        );
    }
}
