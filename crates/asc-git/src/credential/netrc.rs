//! `~/.netrc` (`_netrc` on Windows) for the client credential resolver (SPEC §15.6.1, step 4).
//!
//! Git leaves netrc to libcurl and `git credential fill` ignores it, so asc reads the file
//! itself. The grammar is the one curl reads: whitespace separated tokens, `machine <host>`
//! or `default` starting an entry, `login`, `password` and `account` filling it, `macdef`
//! bodies (which end at a blank line) skipped, `#` starting a comment, and double quotes
//! with backslash escapes for a value that holds whitespace. The parser is lenient: it
//! never fails and never panics, and a file that ends in the middle of an entry keeps the
//! entries it completed. Secrets never reach `Debug`.

use std::fmt;
use std::path::{Path, PathBuf};

/// Largest netrc file read; a larger file is ignored as malformed.
pub const MAX_BYTES: u64 = 1 << 20;

const SPACE: [char; 6] = [' ', '\t', '\r', '\n', '\u{b}', '\u{c}'];

/// One `machine` or `default` entry.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Entry {
    /// The host, lowercased; `None` for the `default` entry.
    pub machine: Option<String>,
    pub login: Option<String>,
    pub password: Option<String>,
}

impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Entry")
            .field("machine", &self.machine)
            .field("login", &self.login.as_ref().map(|_| "<redacted>"))
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// A parsed netrc file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Netrc {
    entries: Vec<Entry>,
}

/// The login and password a lookup found. Either may be empty, never both.
#[derive(Clone, PartialEq, Eq)]
pub struct Login {
    pub login: String,
    pub password: String,
}

impl fmt::Debug for Login {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Login")
            .field("login", &"<redacted>")
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Splits netrc text into tokens.
struct Tokens<'a> {
    rest: &'a str,
}

impl<'a> Tokens<'a> {
    fn new(text: &'a str) -> Self {
        Self { rest: text }
    }

    fn next_token(&mut self) -> Option<String> {
        loop {
            self.rest = self.rest.trim_start_matches(SPACE);
            let first = self.rest.chars().next()?;
            if first == '#' {
                self.rest = self.rest.find('\n').map_or("", |i| &self.rest[i..]);
                continue;
            }
            if first == '"' {
                return Some(self.quoted());
            }
            let end = self.rest.find(SPACE).unwrap_or(self.rest.len());
            let token = self.rest[..end].to_owned();
            self.rest = &self.rest[end..];
            return Some(token);
        }
    }

    /// A double-quoted token; an unterminated quote runs to the end of the line.
    fn quoted(&mut self) -> String {
        let mut out = String::new();
        let mut chars = self.rest.char_indices().skip(1);
        let mut consumed = self.rest.len();
        while let Some((i, c)) = chars.next() {
            match c {
                '"' => {
                    consumed = i + 1;
                    break;
                }
                '\n' => {
                    consumed = i;
                    break;
                }
                '\\' => match chars.next() {
                    Some((_, 'n')) => out.push('\n'),
                    Some((_, 'r')) => out.push('\r'),
                    Some((_, 't')) => out.push('\t'),
                    Some((_, other)) => out.push(other),
                    None => {}
                },
                other => out.push(other),
            }
        }
        self.rest = &self.rest[consumed..];
        out
    }

    /// Drops the rest of the `macdef` line and everything up to and including the next blank
    /// line, which ends the macro body.
    fn skip_macro(&mut self) {
        let mut consumed = 0;
        for (n, line) in self.rest.split_inclusive('\n').enumerate() {
            consumed += line.len();
            if n > 0 && line.trim().is_empty() {
                break;
            }
        }
        self.rest = &self.rest[consumed..];
    }
}

impl Netrc {
    /// Parses netrc text. Unknown keywords and values without a keyword are skipped.
    pub fn parse(text: &str) -> Self {
        let mut tokens = Tokens::new(text.strip_prefix('\u{feff}').unwrap_or(text));
        let mut entries: Vec<Entry> = Vec::new();
        let mut current: Option<Entry> = None;
        while let Some(word) = tokens.next_token() {
            match word.as_str() {
                "machine" => {
                    entries.extend(current.take());
                    match tokens.next_token() {
                        Some(host) if !host.is_empty() => {
                            current = Some(Entry {
                                machine: Some(host.to_ascii_lowercase()),
                                ..Entry::default()
                            });
                        }
                        // A `machine` without a host ends the usable part of the file.
                        _ => break,
                    }
                }
                "default" => {
                    entries.extend(current.take());
                    current = Some(Entry::default());
                }
                "login" | "user" => {
                    if let (Some(value), Some(e)) = (tokens.next_token(), current.as_mut()) {
                        e.login.get_or_insert(value);
                    }
                }
                "password" | "passwd" => {
                    if let (Some(value), Some(e)) = (tokens.next_token(), current.as_mut()) {
                        e.password.get_or_insert(value);
                    }
                }
                "account" | "port" => {
                    tokens.next_token();
                }
                "macdef" => {
                    tokens.next_token();
                    tokens.skip_macro();
                }
                _ => {}
            }
        }
        entries.extend(current);
        Self { entries }
    }

    /// Reads and parses `path`; a missing, unreadable, non-UTF-8 or oversized file is `None`.
    pub fn read(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        if !meta.is_file() || meta.len() > MAX_BYTES {
            return None;
        }
        let bytes = std::fs::read(path).ok()?;
        Some(Self::parse(std::str::from_utf8(&bytes).ok()?))
    }

    /// The candidate files under `home`, most preferred first: `_netrc` then `.netrc` on
    /// Windows, `.netrc` elsewhere.
    pub fn candidates(home: &Path) -> Vec<PathBuf> {
        if cfg!(windows) {
            vec![home.join("_netrc"), home.join(".netrc")]
        } else {
            vec![home.join(".netrc")]
        }
    }

    /// Reads the first candidate under `home` that exists.
    pub fn read_home(home: &Path) -> Option<Self> {
        Self::candidates(home).iter().find_map(|p| Self::read(p))
    }

    /// The entries, in file order.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The credential for `host`. A `machine` entry matches case-insensitively; when `login`
    /// is given only an entry with that login (or none) matches and the answer carries it.
    /// The `default` entry answers only when no `machine` entry names the host.
    pub fn lookup(&self, host: &str, login: Option<&str>) -> Option<Login> {
        let host = host.to_ascii_lowercase();
        let named = self
            .entries
            .iter()
            .any(|e| e.machine.as_deref() == Some(host.as_str()));
        let entry = self
            .entries
            .iter()
            .filter(|e| match &e.machine {
                Some(m) => named && *m == host,
                None => !named,
            })
            .find(|e| login.is_none_or(|l| e.login.as_deref().is_none_or(|el| el == l)))?;
        let found = Login {
            login: entry
                .login
                .clone()
                .or_else(|| login.map(str::to_owned))
                .unwrap_or_default(),
            password: entry.password.clone().unwrap_or_default(),
        };
        (!found.login.is_empty() || !found.password.is_empty()).then_some(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(text: &str, host: &str) -> Option<(String, String)> {
        Netrc::parse(text)
            .lookup(host, None)
            .map(|l| (l.login, l.password))
    }

    fn pair(a: &str, b: &str) -> Option<(String, String)> {
        Some((a.to_owned(), b.to_owned()))
    }

    #[test]
    fn machine_entries_one_line_and_many() {
        let text = "machine a.example login alice password s3cret\nmachine b.example\n  login bob\n  password hunter2\n";
        assert_eq!(find(text, "a.example"), pair("alice", "s3cret"));
        assert_eq!(find(text, "B.EXAMPLE"), pair("bob", "hunter2"));
        assert_eq!(find(text, "c.example"), None);
    }

    #[test]
    fn default_entry_only_without_machine() {
        let text = "machine a.example login alice password one\ndefault login anon password two\n";
        assert_eq!(find(text, "a.example"), pair("alice", "one"));
        assert_eq!(find(text, "other.example"), pair("anon", "two"));
    }

    #[test]
    fn default_may_come_first() {
        let text = "default login anon password two\nmachine a.example login alice password one\n";
        assert_eq!(find(text, "a.example"), pair("alice", "one"));
        assert_eq!(find(text, "z.example"), pair("anon", "two"));
    }

    #[test]
    fn comments_macdef_and_unknown_words() {
        let text = "# a comment\nmachine a.example # trailing\n login alice\nmacdef init\n machine evil login x password y\n\n password pw\nbogus word\n";
        assert_eq!(Netrc::parse(text).entries().len(), 1);
        assert_eq!(find(text, "a.example"), pair("alice", "pw"));
        assert_eq!(find(text, "evil"), None);
    }

    #[test]
    fn quoted_values_hold_spaces_and_escapes() {
        let text = "machine a.example login \"a b\" password \"p\\\"w\\\\x\"\n";
        assert_eq!(find(text, "a.example"), pair("a b", "p\"w\\x"));
    }

    #[test]
    fn login_filter_picks_the_matching_entry() {
        let text = "machine a.example login alice password one\nmachine a.example login bob password two\n";
        let n = Netrc::parse(text);
        assert_eq!(n.lookup("a.example", Some("bob")).unwrap().password, "two");
        assert!(n.lookup("a.example", Some("carol")).is_none());
    }

    #[test]
    fn login_filter_accepts_an_entry_without_login() {
        let n = Netrc::parse("machine a.example password tok\n");
        let l = n.lookup("a.example", Some("me")).unwrap();
        assert_eq!((l.login.as_str(), l.password.as_str()), ("me", "tok"));
    }

    #[test]
    fn malformed_files_never_panic() {
        for text in [
            "",
            "machine",
            "machine a.example login",
            "machine a.example password",
            "login alice password x",
            "machine \"unterminated",
            "macdef",
            "machine a.example\u{0}login x",
            "default",
            "\"",
            "# only a comment",
        ] {
            let _ = Netrc::parse(text);
        }
        // Entries completed before the damage survive it.
        assert_eq!(
            find("machine a.example login u password p\nmachine", "a.example"),
            pair("u", "p")
        );
        assert_eq!(
            find("\u{feff}machine a.example login u password p", "a.example"),
            pair("u", "p")
        );
        assert_eq!(find("machine a.example login", "a.example"), None);
        assert_eq!(find("login alice password x", "a.example"), None);
    }

    #[test]
    fn debug_redacts() {
        let n = Netrc::parse("machine a.example login alice password hunter2\n");
        let text = format!("{n:?} {:?}", n.lookup("a.example", None));
        assert!(
            !text.contains("alice") && !text.contains("hunter2"),
            "{text}"
        );
    }

    #[test]
    fn reads_files_and_ignores_missing_or_huge_ones() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Netrc::read_home(dir.path()).is_none());
        let name = if cfg!(windows) { "_netrc" } else { ".netrc" };
        std::fs::write(
            dir.path().join(name),
            "machine a.example login u password p\n",
        )
        .unwrap();
        assert!(
            Netrc::read_home(dir.path())
                .unwrap()
                .lookup("a.example", None)
                .is_some()
        );
        std::fs::write(dir.path().join(name), vec![b'a'; (MAX_BYTES + 1) as usize]).unwrap();
        assert!(Netrc::read_home(dir.path()).is_none());
        std::fs::write(dir.path().join(name), [0xff, 0xfe, 0x00]).unwrap();
        assert!(Netrc::read_home(dir.path()).is_none());
    }
}
