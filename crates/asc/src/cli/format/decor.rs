//! Ref decorations: `%d`, `%D`, `%(decorate…)` and the `(HEAD -> master)` suffix of `log`.

use super::color;

/// What a decorated ref is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefKind {
    Head,
    Local,
    Remote,
    Tag,
    Stash,
}

/// One ref pointing at a commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecorRef {
    pub kind: RefKind,
    /// The full ref name (`refs/heads/master`), or `HEAD`.
    pub name: String,
}

/// The refs at one commit, in git's order: `HEAD`, then the others by descending name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Decor {
    pub refs: Vec<DecorRef>,
    /// The full name of the branch `HEAD` is attached to, when `HEAD` is at this commit.
    pub head_branch: Option<String>,
}

/// How decorations are written.
#[derive(Clone, Debug)]
pub struct Style {
    /// Full ref names (`--decorate=full`) instead of short ones.
    pub full: bool,
    pub color: bool,
    pub prefix: Vec<u8>,
    pub suffix: Vec<u8>,
    pub separator: Vec<u8>,
    pub pointer: Vec<u8>,
    pub tag: Vec<u8>,
}

impl Style {
    /// `%d`, `--decorate`: ` (a, b)`.
    pub fn standard(full: bool, color: bool) -> Self {
        Self {
            full,
            color,
            prefix: b" (".to_vec(),
            suffix: b")".to_vec(),
            separator: b", ".to_vec(),
            pointer: b" -> ".to_vec(),
            tag: b"tag: ".to_vec(),
        }
    }
}

fn short_name(name: &str) -> &str {
    ["refs/heads/", "refs/remotes/", "refs/tags/"]
        .iter()
        .find_map(|p| name.strip_prefix(p))
        .unwrap_or(name)
}

fn kind_color(kind: RefKind) -> &'static str {
    match kind {
        RefKind::Head => "bold cyan",
        RefKind::Local => "bold green",
        RefKind::Remote => "bold red",
        RefKind::Tag => "bold yellow",
        RefKind::Stash => "bold magenta",
    }
}

impl Decor {
    pub fn is_empty(&self) -> bool {
        self.refs.is_empty()
    }

    /// Writes the decoration list; nothing when there are no refs.
    pub fn write(&self, style: &Style, out: &mut Vec<u8>) {
        if self.refs.is_empty() {
            return;
        }
        let paint = |out: &mut Vec<u8>, spec: &str| {
            if style.color {
                out.extend_from_slice(color::sgr(spec).as_bytes());
            }
        };
        let reset = |out: &mut Vec<u8>| {
            if style.color {
                out.extend_from_slice(color::RESET.as_bytes());
            }
        };
        let name_of = |r: &DecorRef| {
            if style.full {
                r.name.clone()
            } else {
                short_name(&r.name).to_owned()
            }
        };
        let attached = self
            .head_branch
            .as_deref()
            .filter(|_| self.refs.iter().any(|r| r.kind == RefKind::Head));
        let mut lead: &[u8] = &style.prefix;
        for r in &self.refs {
            if attached.is_some_and(|b| r.kind == RefKind::Local && r.name == b) {
                continue;
            }
            paint(out, "yellow");
            out.extend_from_slice(lead);
            reset(out);
            lead = &style.separator;
            paint(out, kind_color(r.kind));
            if r.kind == RefKind::Tag {
                out.extend_from_slice(&style.tag);
                reset(out);
                paint(out, kind_color(r.kind));
            }
            out.extend_from_slice(name_of(r).as_bytes());
            reset(out);
            if let (RefKind::Head, Some(branch)) = (r.kind, attached) {
                paint(out, "yellow");
                out.extend_from_slice(&style.pointer);
                reset(out);
                paint(out, kind_color(RefKind::Local));
                let shown = if style.full {
                    branch
                } else {
                    short_name(branch)
                };
                out.extend_from_slice(shown.as_bytes());
                reset(out);
            }
        }
        paint(out, "yellow");
        out.extend_from_slice(&style.suffix);
        reset(out);
    }

    /// The decoration as a string, for tests and `--oneline`.
    pub fn text(&self, style: &Style) -> Vec<u8> {
        let mut out = Vec::new();
        self.write(style, &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(kind: RefKind, name: &str) -> DecorRef {
        DecorRef {
            kind,
            name: name.into(),
        }
    }

    fn sample() -> Decor {
        Decor {
            refs: vec![
                r(RefKind::Head, "HEAD"),
                r(RefKind::Tag, "refs/tags/v1"),
                r(RefKind::Remote, "refs/remotes/origin/master"),
                r(RefKind::Local, "refs/heads/master"),
            ],
            head_branch: Some("refs/heads/master".into()),
        }
    }

    #[test]
    fn head_arrow_replaces_the_branch() {
        let s = String::from_utf8(sample().text(&Style::standard(false, false))).unwrap();
        assert_eq!(s, " (HEAD -> master, tag: v1, origin/master)");
        let s = String::from_utf8(sample().text(&Style::standard(true, false))).unwrap();
        assert_eq!(
            s,
            " (HEAD -> refs/heads/master, tag: refs/tags/v1, refs/remotes/origin/master)"
        );
    }

    #[test]
    fn detached_head_lists_every_branch() {
        let mut d = sample();
        d.head_branch = None;
        let s = String::from_utf8(d.text(&Style::standard(false, false))).unwrap();
        assert_eq!(s, " (HEAD, tag: v1, origin/master, master)");
    }

    #[test]
    fn empty_is_empty() {
        assert!(
            Decor::default()
                .text(&Style::standard(false, false))
                .is_empty()
        );
    }
}
