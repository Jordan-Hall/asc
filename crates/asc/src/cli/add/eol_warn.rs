//! git's line-ending round-trip warning (`convert.c`, `crlf_to_git` and
//! `check_global_conv_flags_eol` under the default `core.safecrlf=warn`). When git reads a work
//! tree file into the index it checks whether a checkout would give the same line endings back,
//! and warns when it would not:
//!
//! ```text
//! warning: in the working copy of 'a.txt', LF will be replaced by CRLF the next time Git touches it
//! ```
//!
//! The decision follows git's `crlf_action`: the `text` (else `crlf`) and `eol` attributes, then
//! `core.autocrlf` and `core.eol`, with the platform's native line ending (CRLF on Windows) when
//! neither says. asc stores the bytes its own normalisation gives; this only repeats git's notice.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use asc_core::path::RepoPath;
pub use asc_wc::eol::Stats;
use asc_wc::eol::Smudge;
use asc_wc::status::Eol;

use super::attrs::{EolAttr, PathAttrs};
use crate::cli::common::Ctx;

/// The stats of the file at `path`, read in pieces.
pub fn file_stats(path: &Path) -> io::Result<Stats> {
    let mut s = Stats::default();
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(s.finish());
        }
        s.feed(&buf[..n]);
    }
}

/// How git converts a path: `auto` for `text=auto` (or `core.autocrlf` without a `text`
/// attribute), `crlf` when a checkout writes CRLF.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Action {
    auto: bool,
    crlf: bool,
}

/// The configuration git's line-ending conversion reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EolConfig {
    /// `core.autocrlf`: `Some(true)` true, `Some(false)` input, `None` false or unset.
    autocrlf: Option<bool>,
    /// Whether text a checkout writes gets CRLF (git's `text_eol_is_crlf`).
    text_crlf: bool,
}

impl EolConfig {
    /// `core.autocrlf` false and LF output: only the attributes convert.
    pub const NONE: Self = Self {
        autocrlf: None,
        text_crlf: false,
    };

    pub fn new(ctx: &Ctx) -> Self {
        let autocrlf = match ctx.config.get("core.autocrlf").as_deref() {
            Some(v) if v.eq_ignore_ascii_case("input") => Some(false),
            _ => ctx.config.bool("core.autocrlf").filter(|t| *t),
        };
        let text_crlf = match autocrlf {
            Some(t) => t,
            None => match ctx.config.get("core.eol").as_deref() {
                Some(v) if v.eq_ignore_ascii_case("crlf") => true,
                Some(v) if v.eq_ignore_ascii_case("lf") => false,
                _ => cfg!(windows),
            },
        };
        Self {
            autocrlf,
            text_crlf,
        }
    }

    /// git's `convert_attrs`; `None` where nothing is converted.
    fn action(&self, a: &PathAttrs) -> Option<Action> {
        let auto = match a.text {
            Some(Eol::Keep) => return None,
            Some(Eol::Text) => false,
            Some(Eol::Auto) => true,
            None if a.eol_attr.is_some() => false,
            None => {
                return self.autocrlf.map(|crlf| Action { auto: true, crlf });
            }
        };
        let crlf = match a.eol_attr {
            Some(e) => e == EolAttr::Crlf,
            None => self.text_crlf,
        };
        Some(Action { auto, crlf })
    }

    /// `a` with what is stored ([`PathAttrs::eol`]) and what a checkout writes
    /// ([`PathAttrs::smudge`]) worked out from its attributes and this configuration.
    pub fn resolve(&self, mut a: PathAttrs) -> PathAttrs {
        (a.eol, a.smudge) = match self.action(&a) {
            None => (Eol::Keep, Smudge::None),
            Some(Action { auto: true, crlf }) => {
                (Eol::Auto, if crlf { Smudge::AutoCrlf } else { Smudge::None })
            }
            Some(Action { auto: false, crlf }) => {
                (Eol::Text, if crlf { Smudge::Crlf } else { Smudge::None })
            }
        };
        a
    }

    /// Whether git converts `a`'s line endings at all; when not there is never a notice.
    pub fn converts(&self, a: &PathAttrs) -> bool {
        self.action(a).is_some()
    }

    /// Whether [`Self::warning`] needs to know if the staged content is text with CRLFs.
    pub fn needs_index(&self, a: &PathAttrs, s: &Stats) -> bool {
        self.action(a).is_some_and(|x| x.auto) && s.crlf > 0
    }

    /// git's notice for a work tree file with `s` (`index_crlf`: the staged content is text with
    /// CRLFs), as the middle of "in the working copy of '…', … the next time Git touches it".
    pub fn warning(&self, a: &PathAttrs, s: &Stats, index_crlf: bool) -> Option<&'static str> {
        let action = self.action(a)?;
        if action.auto && s.is_binary() {
            return None;
        }
        let mut new = *s;
        // What `git add` stores: CRLF becomes LF, except under auto when the index has CRLFs.
        if s.crlf > 0 && !(action.auto && index_crlf) {
            new.lone_lf += new.crlf;
            new.crlf = 0;
        }
        // What `git checkout` writes back (`will_convert_lf_to_crlf`).
        let to_crlf = action.crlf
            && new.lone_lf > 0
            && !(action.auto && (new.lone_cr > 0 || new.crlf > 0 || new.is_binary()));
        if to_crlf {
            new.crlf += new.lone_lf;
            new.lone_lf = 0;
        }
        if s.crlf > 0 && new.crlf == 0 {
            Some("CRLF will be replaced by LF")
        } else if s.lone_lf > 0 && new.lone_lf == 0 {
            Some("LF will be replaced by CRLF")
        } else {
            None
        }
    }
}

/// Prints git's notice for `path`.
pub fn print(path: &RepoPath, what: &str) {
    eprintln!(
        "warning: in the working copy of '{}', {what} the next time Git touches it",
        String::from_utf8_lossy(path.as_bytes())
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(text: Option<Eol>, eol_attr: Option<EolAttr>) -> PathAttrs {
        PathAttrs {
            text,
            eol_attr,
            ..PathAttrs::NONE
        }
    }

    const fn cfg(autocrlf: Option<bool>, text_crlf: bool) -> EolConfig {
        EolConfig {
            autocrlf,
            text_crlf,
        }
    }

    const LF: &str = "LF will be replaced by CRLF";
    const CRLF: &str = "CRLF will be replaced by LF";

    #[test]
    fn text_auto_on_a_crlf_platform_warns_for_lone_lfs() {
        let auto = attrs(Some(Eol::Auto), None);
        let lf = Stats::of(b"a\nb\n");
        assert_eq!(cfg(None, true).warning(&auto, &lf, false), Some(LF));
        assert_eq!(cfg(None, false).warning(&auto, &lf, false), None);
        // autocrlf=input never writes CRLF. A mixed file is stored with LFs, so a CRLF checkout
        // turns its lone LFs into CRLFs and an LF one drops its CRLFs.
        assert_eq!(cfg(Some(false), false).warning(&auto, &lf, false), None);
        let mixed = Stats::of(b"a\r\nb\n");
        assert_eq!(cfg(None, true).warning(&auto, &mixed, false), Some(LF));
        assert_eq!(cfg(None, false).warning(&auto, &mixed, false), Some(CRLF));
        assert_eq!(cfg(None, true).warning(&auto, &Stats::of(b"\0\n"), false), None);
    }

    #[test]
    fn crlf_that_a_checkout_would_not_restore_warns() {
        let text_lf = attrs(Some(Eol::Text), Some(EolAttr::Lf));
        let crlf = Stats::of(b"a\r\nb\r\n");
        assert_eq!(cfg(None, true).warning(&text_lf, &crlf, false), Some(CRLF));
        let text_crlf = attrs(None, Some(EolAttr::Crlf));
        assert_eq!(cfg(None, false).warning(&text_crlf, &crlf, false), None);
        // Under auto, CRLFs already in the index are kept, so nothing changes.
        let auto = attrs(Some(Eol::Auto), Some(EolAttr::Lf));
        let c = cfg(None, false);
        assert!(c.needs_index(&auto, &crlf));
        assert_eq!(c.warning(&auto, &crlf, true), None);
        assert_eq!(c.warning(&auto, &crlf, false), Some(CRLF));
    }

    #[test]
    fn without_attributes_only_autocrlf_converts() {
        let none = PathAttrs::NONE;
        let lf = Stats::of(b"a\n");
        assert_eq!(cfg(None, true).warning(&none, &lf, false), None);
        assert_eq!(cfg(Some(true), true).warning(&none, &lf, false), Some(LF));
        assert_eq!(
            cfg(Some(false), false).warning(&none, &Stats::of(b"a\r\n"), false),
            Some(CRLF)
        );
        let binary = attrs(Some(Eol::Keep), None);
        assert_eq!(cfg(Some(true), true).warning(&binary, &lf, false), None);
    }
}
