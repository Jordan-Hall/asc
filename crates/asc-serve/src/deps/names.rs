//! The grammar of package names, checked before a name becomes part of a registry URL.
//!
//! A name comes from a manifest or lockfile in a repository, so it is never trusted: it is ASCII, within
//! the length the registry allows, and made of the characters the registry allows, or it is refused with
//! a reason. Nothing that builds a path from a name slices it by byte offset before this has passed.

use super::model::Ecosystem;

fn part(s: &str, max: usize, extra: &str, what: &str) -> Result<(), String> {
    if s.is_empty() || s.len() > max {
        return Err(format!("{what} must be 1 to {max} characters"));
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || extra.as_bytes().contains(&b))
    {
        return Err(format!("{what} holds a character a registry name cannot"));
    }
    if s.starts_with('.') || s.contains("..") {
        return Err(format!("{what} is not a valid name"));
    }
    Ok(())
}

/// Whether `name` is a name `eco`'s registry could have.
pub fn check(eco: Ecosystem, name: &str) -> Result<(), String> {
    match eco {
        Ecosystem::CratesIo => part(name, 64, "_-", "a crate name"),
        Ecosystem::Npm => {
            if name.len() > 214 {
                return Err("an npm name must be at most 214 characters".into());
            }
            match name.strip_prefix('@') {
                Some(rest) => {
                    let (scope, pkg) = rest
                        .split_once('/')
                        .ok_or("an npm scope is written @scope/name")?;
                    part(scope, 214, "._~-", "an npm scope")?;
                    part(pkg, 214, "._~-", "an npm name")
                }
                None => part(name, 214, "._~-", "an npm name"),
            }
        }
        Ecosystem::PyPi => part(name, 214, "._-", "a PyPI name"),
        Ecosystem::Go => {
            if name.is_empty() || name.len() > 512 {
                return Err("a Go module path must be 1 to 512 characters".into());
            }
            for seg in name.split('/') {
                part(seg, 255, "._~-", "an element of a Go module path")?;
            }
            Ok(())
        }
        Ecosystem::Maven => {
            let (g, a) = name
                .split_once(':')
                .ok_or("a Maven name is group:artifact")?;
            if a.contains(':') {
                return Err("a Maven name is group:artifact".into());
            }
            part(g, 128, "._-", "a Maven group")?;
            part(a, 128, "._-", "a Maven artifact")
        }
        Ecosystem::NuGet => part(name, 100, "._-", "a NuGet id"),
        Ecosystem::Packagist => {
            let (v, p) = name
                .split_once('/')
                .ok_or("a Packagist name is vendor/package")?;
            part(v, 100, "._-", "a Packagist vendor")?;
            part(p, 100, "._-", "a Packagist package")
        }
        Ecosystem::RubyGems => part(name, 100, "._-", "a gem name"),
        Ecosystem::Hex => part(name, 64, "_", "a Hex name"),
        Ecosystem::Pub => part(name, 64, "_", "a pub name"),
        Ecosystem::SwiftUrl | Ecosystem::CocoaPods => {
            Err("the ecosystem has no registry API".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_names_pass() {
        for (eco, n) in [
            (Ecosystem::CratesIo, "serde_json"),
            (Ecosystem::CratesIo, "a"),
            (Ecosystem::Npm, "lodash"),
            (Ecosystem::Npm, "@babel/core"),
            (Ecosystem::Npm, "left-pad"),
            (Ecosystem::PyPi, "Django"),
            (Ecosystem::PyPi, "zope.interface"),
            (Ecosystem::Go, "github.com/Azure/go-sdk"),
            (Ecosystem::Go, "gopkg.in/yaml.v3"),
            (Ecosystem::Maven, "org.apache.commons:commons-lang3"),
            (Ecosystem::NuGet, "Newtonsoft.Json"),
            (Ecosystem::Packagist, "symfony/console"),
            (Ecosystem::RubyGems, "activerecord-import"),
            (Ecosystem::Hex, "phoenix_html"),
            (Ecosystem::Pub, "flutter_bloc"),
        ] {
            assert_eq!(check(eco, n), Ok(()), "{eco} {n}");
        }
    }

    #[test]
    fn unicode_empty_overlong_and_path_like_names_are_refused_in_every_ecosystem() {
        let long = "a".repeat(300);
        for eco in [
            Ecosystem::CratesIo,
            Ecosystem::Npm,
            Ecosystem::PyPi,
            Ecosystem::Go,
            Ecosystem::Maven,
            Ecosystem::NuGet,
            Ecosystem::Packagist,
            Ecosystem::RubyGems,
            Ecosystem::Hex,
            Ecosystem::Pub,
        ] {
            let wrap = |n: &str| match eco {
                Ecosystem::Maven => format!("{n}:{n}"),
                Ecosystem::Packagist => format!("{n}/{n}"),
                _ => n.to_owned(),
            };
            for bad in [
                "",
                "é",
                "日本語",
                "ab\u{e9}cd",
                "a\u{0}b",
                "a b",
                "a?b",
                "a#b",
                "a%2Fb",
                "..",
                "a..b",
                "a\\b",
                &long,
            ] {
                assert!(check(eco, &wrap(bad)).is_err(), "{eco} accepted {bad:?}");
            }
        }
        assert!(check(Ecosystem::CratesIo, &"a".repeat(65)).is_err());
        assert!(check(Ecosystem::CratesIo, "a/b").is_err());
        assert!(check(Ecosystem::Npm, "@scope").is_err());
        assert!(check(Ecosystem::Npm, "@sc/ope/x").is_err());
        assert!(check(Ecosystem::Go, "a//b").is_err());
        assert!(check(Ecosystem::Go, "github.com/a/../b").is_err());
        assert!(check(Ecosystem::Maven, "g:a:v").is_err());
        assert!(check(Ecosystem::Packagist, "vendor").is_err());
        assert!(check(Ecosystem::SwiftUrl, "x").is_err());
    }
}
