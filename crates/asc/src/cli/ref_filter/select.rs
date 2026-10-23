//! Which refs a command lists: git's pattern rules (`match_name_as_path` for `for-each-ref`,
//! plain `wildmatch` for the `branch`/`tag` short names) and the ref prefixes worth listing.

/// `wildmatch(pattern, text, WM_PATHNAME)`: `*` and `?` stop at `/`, `**` spans directories.
pub fn wildmatch(pattern: &[u8], text: &[u8], pathname: bool, fold: bool) -> bool {
    matches_at(pattern, 0, text, pathname, fold)
}

fn lower(b: u8, fold: bool) -> u8 {
    if fold { b.to_ascii_lowercase() } else { b }
}

fn class_has(name: &[u8], c: u8) -> bool {
    match name {
        b"alpha" => c.is_ascii_alphabetic(),
        b"digit" => c.is_ascii_digit(),
        b"alnum" => c.is_ascii_alphanumeric(),
        b"upper" => c.is_ascii_uppercase(),
        b"lower" => c.is_ascii_lowercase(),
        b"space" => c.is_ascii_whitespace(),
        b"blank" => c == b' ' || c == b'\t',
        b"punct" => c.is_ascii_punctuation(),
        b"xdigit" => c.is_ascii_hexdigit(),
        b"cntrl" => c.is_ascii_control(),
        b"graph" => c.is_ascii_graphic(),
        b"print" => c.is_ascii_graphic() || c == b' ',
        _ => false,
    }
}

/// A bracket expression at the start of `p` (after `[`) against `c`: whether it matches and the
/// length of the expression, or `None` for an unterminated one.
fn bracket(p: &[u8], c: u8, fold: bool) -> Option<(bool, usize)> {
    let mut i = 0;
    let negate = matches!(p.first(), Some(b'!' | b'^'));
    if negate {
        i += 1;
    }
    let mut found = false;
    let mut first = true;
    while i < p.len() {
        if p[i] == b']' && !first {
            return Some((found != negate, i + 1));
        }
        first = false;
        if p[i] == b'[' && p.get(i + 1) == Some(&b':')
            && let Some(end) = p[i + 2..].windows(2).position(|w| w == b":]") {
                found |= class_has(&p[i + 2..i + 2 + end], c);
                i += end + 4;
                continue;
            }
        let mut lo = p[i];
        if lo == b'\\' && i + 1 < p.len() {
            i += 1;
            lo = p[i];
        }
        if p.get(i + 1) == Some(&b'-') && p.get(i + 2).is_some_and(|&h| h != b']') {
            let mut hi = p[i + 2];
            let mut skip = 3;
            if hi == b'\\' && i + 3 < p.len() {
                hi = p[i + 3];
                skip = 4;
            }
            found |= (lower(lo, fold)..=lower(hi, fold)).contains(&lower(c, fold));
            i += skip;
        } else {
            found |= lower(lo, fold) == lower(c, fold);
            i += 1;
        }
    }
    None
}

fn matches_at(p: &[u8], at: usize, t: &[u8], pathname: bool, fold: bool) -> bool {
    let (mut pi, mut ti) = (at, 0);
    while pi < p.len() {
        match p[pi] {
            b'*' => {
                let start = pi;
                while p.get(pi) == Some(&b'*') {
                    pi += 1;
                }
                let stars = pi - start;
                let boundary_before = start == 0 || p[start - 1] == b'/';
                let boundary_after = pi == p.len() || p[pi] == b'/';
                let spans = !pathname || (stars >= 2 && boundary_before && boundary_after);
                if stars >= 2 && boundary_before && boundary_after && pi < p.len() {
                    // `**/` also matches no directory at all.
                    if matches_at(&p[pi + 1..], 0, &t[ti..], pathname, fold) {
                        return true;
                    }
                }
                let rest = &p[pi..];
                if rest.is_empty() {
                    return spans || !t[ti..].contains(&b'/');
                }
                for k in ti..=t.len() {
                    if matches_at(rest, 0, &t[k..], pathname, fold) {
                        return true;
                    }
                    if k < t.len() && !spans && t[k] == b'/' {
                        return false;
                    }
                }
                return false;
            }
            b'?' => {
                if ti >= t.len() || (pathname && t[ti] == b'/') {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
            b'[' => {
                let Some(&c) = t.get(ti) else { return false };
                match bracket(&p[pi + 1..], c, fold) {
                    Some((true, len)) if !(pathname && c == b'/') => {
                        pi += 1 + len;
                        ti += 1;
                    }
                    Some(_) => return false,
                    // An unterminated bracket is an ordinary `[`.
                    None => {
                        if lower(c, fold) != b'[' {
                            return false;
                        }
                        pi += 1;
                        ti += 1;
                    }
                }
            }
            b'\\' if pi + 1 < p.len() => {
                if t.get(ti).map(|&c| lower(c, fold)) != Some(lower(p[pi + 1], fold)) {
                    return false;
                }
                pi += 2;
                ti += 1;
            }
            c => {
                if t.get(ti).map(|&x| lower(x, fold)) != Some(lower(c, fold)) {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
        }
    }
    ti == t.len()
}

/// `match_name_as_path`: a pattern matches a name it equals, names below it (a prefix up to a
/// slash, or ending in one), or by `wildmatch`.
pub fn match_as_path(patterns: &[String], name: &str, fold: bool) -> bool {
    let n = name.as_bytes();
    patterns.iter().any(|p| {
        let pb = p.as_bytes();
        if pb.is_empty() {
            return false;
        }
        let prefix = n.len() >= pb.len() && &n[..pb.len()] == pb;
        if prefix && (n.len() == pb.len() || n[pb.len()] == b'/' || pb[pb.len() - 1] == b'/') {
            return true;
        }
        wildmatch(pb, n, true, fold)
    })
}

/// `branch`/`tag` style: plain `wildmatch` against the name.
pub fn match_plain(patterns: &[String], name: &str, fold: bool) -> bool {
    patterns.iter().any(|p| wildmatch(p.as_bytes(), name.as_bytes(), false, fold))
}

/// The ref prefixes to list for these patterns: each pattern's literal head cut at its last `/`;
/// `refs/` when nothing narrower is known.
pub fn prefixes(patterns: &[String]) -> Vec<String> {
    if patterns.is_empty() {
        return vec!["refs/".to_owned()];
    }
    let mut out: Vec<String> = Vec::new();
    for p in patterns {
        let literal = p.find(['*', '?', '[', '\\']).map_or(p.as_str(), |at| &p[..at]);
        let cut = literal.rfind('/').map_or("", |at| &literal[..=at]);
        let prefix = if cut.starts_with("refs/") { cut.to_owned() } else { "refs/".to_owned() };
        if !out.iter().any(|existing| prefix.starts_with(existing.as_str())) {
            out.retain(|existing| !existing.starts_with(&prefix));
            out.push(prefix);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wm(p: &str, t: &str) -> bool {
        wildmatch(p.as_bytes(), t.as_bytes(), true, false)
    }

    #[test]
    fn wildmatch_path_rules() {
        assert!(wm("refs/heads/*", "refs/heads/master"));
        assert!(!wm("refs/heads/*", "refs/heads/a/b"));
        assert!(wm("refs/heads/**", "refs/heads/a/b"));
        assert!(wm("refs/**/b", "refs/b"));
        assert!(wm("refs/**/b", "refs/x/y/b"));
        assert!(wm("refs/tags/v?", "refs/tags/v1"));
        assert!(!wm("refs/tags/v?", "refs/tags/v10"));
        assert!(wm("refs/tags/v[0-9]", "refs/tags/v7"));
        assert!(!wm("refs/tags/v[!0-9]", "refs/tags/v7"));
        assert!(wm("a\\*b", "a*b"));
        assert!(!wm("refs/t*", "refs/tags/v1"));
    }

    #[test]
    fn as_path_matches_prefixes_up_to_a_slash() {
        let one = |p: &str| vec![p.to_owned()];
        assert!(match_as_path(&one("refs/heads"), "refs/heads/master", false));
        assert!(match_as_path(&one("refs/heads/"), "refs/heads/master", false));
        assert!(!match_as_path(&one("refs/head"), "refs/heads/master", false));
        assert!(!match_as_path(&one("REFS/HEADS"), "refs/heads/master", true));
        assert!(match_as_path(&one("REFS/heads/*"), "refs/heads/master", true));
        assert!(match_as_path(&one("refs/heads/master"), "refs/heads/master", false));
    }

    #[test]
    fn listing_prefixes() {
        assert_eq!(prefixes(&[]), ["refs/"]);
        assert_eq!(prefixes(&["refs/heads/ma*".into()]), ["refs/heads/"]);
        assert_eq!(prefixes(&["refs/heads/a".into(), "refs/tags/v*".into()]), ["refs/heads/", "refs/tags/"]);
        assert_eq!(prefixes(&["master".into()]), ["refs/"]);
        assert_eq!(prefixes(&["refs/heads/x".into(), "refs/*".into()]), ["refs/"]);
    }
}
