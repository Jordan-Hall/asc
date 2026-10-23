//! Fields of the raw bytes of a commit or tag, read the way git's ref-filter reads them
//! (`grab_person`, `find_subpos`, `grab_date`). The object is exported once and every atom reads
//! the same bytes, so what prints equals what `cat-file -p` shows.


use crate::cli::format::date;
use crate::cli::format::mailmap::Mailmap;

/// The signature markers `parse_signature` knows (a line starting with one begins a signature).
const SIGNATURE_MARKERS: [&[u8]; 4] = [
    b"-----BEGIN PGP SIGNATURE-----",
    b"-----BEGIN PGP MESSAGE-----",
    b"-----BEGIN SIGNED MESSAGE-----",
    b"-----BEGIN SSH SIGNATURE-----",
];

fn lines(raw: &[u8]) -> impl Iterator<Item = (usize, &[u8])> {
    let mut at = 0;
    std::iter::from_fn(move || {
        if at >= raw.len() {
            return None;
        }
        let start = at;
        let end = raw[at..].iter().position(|&b| b == b'\n').map_or(raw.len(), |p| at + p + 1);
        at = end;
        Some((start, &raw[start..end]))
    })
}

/// The values of header `name` (`parent`, `tree`, …) of a commit or tag.
pub fn header_values<'a>(raw: &'a [u8], name: &str) -> Vec<&'a [u8]> {
    let mut out = Vec::new();
    for (_, line) in lines(raw) {
        if line == b"\n" {
            break;
        }
        if let Some(rest) = line.strip_prefix(name.as_bytes()).and_then(|r| r.strip_prefix(b" ")) {
            out.push(rest.strip_suffix(b"\n").unwrap_or(rest));
        }
    }
    out
}

/// The value of the first header `name`, or empty.
pub fn header<'a>(raw: &'a [u8], name: &str) -> &'a [u8] {
    header_values(raw, name).first().copied().unwrap_or(b"")
}

/// What follows `who ` in the `who` header (`author`, `committer`, `tagger`), up to the end of the
/// object, as git's `find_wholine` hands it to the field readers; empty when there is none. The
/// readers search this whole tail, not just the line, as git's do.
pub fn person_tail<'a>(raw: &'a [u8], who: &str) -> &'a [u8] {
    for (start, line) in lines(raw) {
        if line == b"\n" {
            break;
        }
        if line.starts_with(who.as_bytes()) && line.get(who.len()) == Some(&b' ') {
            return &raw[start + who.len() + 1..];
        }
    }
    b""
}

/// The first line of `tail`, without its newline.
pub fn line_of(tail: &[u8]) -> &[u8] {
    tail.iter().position(|&b| b == b'\n').map_or(tail, |p| &tail[..p])
}

/// The name before ` <` on the first line; empty when there is none.
pub fn name_of(tail: &[u8]) -> &[u8] {
    let line = line_of(tail);
    line.windows(2).position(|w| w == b" <").map_or(b"", |p| &line[..p])
}

/// The address between the first `<` and the next `>`.
fn address(tail: &[u8]) -> Option<&[u8]> {
    let open = tail.iter().position(|&b| b == b'<')?;
    let close = tail[open..].iter().position(|&b| b == b'>').map(|p| open + p)?;
    Some(&tail[open + 1..close])
}

/// The email as `%(authoremail…)` prints it (`copy_email`).
pub fn email_of(tail: &[u8], trim: bool, localpart: bool, mailmap: Option<&Mailmap>) -> Vec<u8> {
    let Some(open) = tail.iter().position(|&b| b == b'<') else {
        return Vec::new();
    };
    let find = |from: usize, ch: u8| tail[from..].iter().position(|&b| b == ch).map(|p| from + p);
    let (start, end) = if localpart {
        let start = open + 1;
        (start, find(start, b'@').or_else(|| find(start, b'>')))
    } else if trim {
        (open + 1, find(open + 1, b'>'))
    } else {
        (open, find(open, b'>').map(|p| p + 1))
    };
    let Some(end) = end else { return Vec::new() };
    let text = tail[start..end].to_vec();
    match (mailmap, address(tail)) {
        (Some(map), Some(addr)) => {
            let mapped = map.lookup(name_of(tail), addr).1;
            if localpart {
                mapped[..mapped.iter().position(|&b| b == b'@').unwrap_or(mapped.len())].to_vec()
            } else if trim {
                mapped
            } else {
                [b"<".as_slice(), &mapped, b">"].concat()
            }
        }
        _ => text,
    }
}

/// The name, through the mailmap when asked.
pub fn mapped_name(tail: &[u8], mailmap: Option<&Mailmap>) -> Vec<u8> {
    let name = name_of(tail);
    match (mailmap, address(tail)) {
        (Some(map), Some(addr)) => map.lookup(name, addr).0,
        _ => name.to_vec(),
    }
}

/// The seconds and zone after the first `> ` (`grab_date`): the zone is read as the decimal
/// `±HHMM` number `strtol` makes of it.
pub fn when(tail: &[u8]) -> Option<(i64, i16)> {
    let at = tail.windows(2).position(|w| w == b"> ")? + 2;
    let rest = &tail[at..];
    let digits = rest.iter().position(|b| !b.is_ascii_digit()).unwrap_or(rest.len());
    let time = std::str::from_utf8(&rest[..digits]).ok()?.parse::<i64>().ok()?;
    let zone = &rest[digits..];
    let zone = zone.strip_prefix(b" ").unwrap_or(zone);
    let (sign, zone) = match zone.first() {
        Some(b'-') => (-1, &zone[1..]),
        Some(b'+') => (1, &zone[1..]),
        _ => (1, zone),
    };
    let len = zone.iter().position(|b| !b.is_ascii_digit()).unwrap_or(zone.len());
    let hhmm: i64 = std::str::from_utf8(&zone[..len]).ok().and_then(|z| z.parse().ok()).unwrap_or(0);
    Some((time, (sign * (hhmm / 100 * 60 + hhmm % 100)) as i16))
}

/// The date text and its timestamp (0 for none), formatted by `mode`.
pub fn date_of(tail: &[u8], mode: &date::Mode, now: i64) -> (Vec<u8>, i64) {
    match when(tail) {
        Some((time, tz)) => (mode.render(time, tz, now).into_bytes(), time),
        None => (Vec::new(), 0),
    }
}


/// Where the parts of the message sit in the raw bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// First line of the message (after the headers and the blank lines that follow them).
    pub sub: usize,
    /// End of the subject paragraph.
    pub sub_end: usize,
    /// First line of the body.
    pub body: usize,
    /// Start of the signature block, or the end of the object.
    pub sig: usize,
    pub end: usize,
}

fn signature_start(raw: &[u8]) -> usize {
    let mut start = raw.len();
    for (at, line) in lines(raw) {
        if SIGNATURE_MARKERS.iter().any(|m| line.starts_with(m)) {
            start = at;
        }
    }
    start
}

/// `find_subpos`.
pub fn layout(raw: &[u8]) -> Layout {
    let end = raw.len();
    let sig = signature_start(raw);
    let mut at = 0;
    for (start, line) in lines(raw) {
        if line == b"\n" {
            break;
        }
        at = start + line.len();
    }
    while at < end && raw[at] == b'\n' {
        at += 1;
    }
    let sub = at;
    let para = raw[sub..].windows(2).position(|w| w == b"\n\n").map(|p| sub + p).filter(|&p| p < sig);
    match para {
        Some(p) => {
            let mut body = p;
            while body < end && raw[body] == b'\n' {
                body += 1;
            }
            Layout { sub, sub_end: p, body, sig, end }
        }
        None => {
            let mut sub_end = sig.max(sub);
            while sub_end > sub && raw[sub_end - 1] == b'\n' {
                sub_end -= 1;
            }
            Layout { sub, sub_end, body: sig.max(sub), sig, end }
        }
    }
}

/// The subject paragraph on one line.
pub fn subject(raw: &[u8], l: &Layout) -> Vec<u8> {
    raw[l.sub..l.sub_end].iter().map(|&b| if b == b'\n' { b' ' } else { b }).collect()
}

/// The first `lines` lines of `text`, continuation lines indented four spaces.
pub fn first_lines(text: &[u8], count: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut at = 0;
    for i in 0..count {
        if at >= text.len() {
            break;
        }
        if i > 0 {
            out.extend_from_slice(b"\n    ");
        }
        match text[at..].iter().position(|&b| b == b'\n') {
            Some(p) => {
                out.extend_from_slice(&text[at..at + p]);
                at += p + 1;
            }
            None => {
                out.extend_from_slice(&text[at..]);
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &[u8] = b"tree 1\nparent 2\nauthor Ann A <ann@x.org> 1600000000 +0130\n\nfirst\nsecond\n\nbody 1\nbody 2\n";

    #[test]
    fn headers_and_people() {
        assert_eq!(header_values(COMMIT, "parent"), [b"2".as_slice()]);
        let line = person_tail(COMMIT, "author");
        assert_eq!(name_of(line), b"Ann A");
        assert_eq!(email_of(line, false, false, None), b"<ann@x.org>");
        assert_eq!(email_of(line, true, false, None), b"ann@x.org");
        assert_eq!(email_of(line, true, true, None), b"ann");
        assert_eq!(when(line), Some((1_600_000_000, 90)));
        assert_eq!(person_tail(COMMIT, "tagger"), b"");
    }

    #[test]
    fn message_parts() {
        let l = layout(COMMIT);
        assert_eq!(subject(COMMIT, &l), b"first second");
        assert_eq!(&COMMIT[l.body..l.sig], b"body 1\nbody 2\n");
        assert_eq!(first_lines(&COMMIT[l.sub..], 2), b"first\n    second");
    }

    #[test]
    fn signature_splits_off_the_end() {
        let tag = b"object 1\n\nsubj\n\nbody\n-----BEGIN PGP SIGNATURE-----\n\nAB\n-----END PGP SIGNATURE-----\n";
        let l = layout(tag);
        assert_eq!(&tag[l.body..l.sig], b"body\n");
        assert!(tag[l.sig..].starts_with(b"-----BEGIN PGP SIGNATURE-----"));
        let plain = layout(b"object 1\n\nonly subject\n");
        assert_eq!(plain.sub_end - plain.sub, "only subject".len());
    }
}
