//! Argv redaction (SPEC §20.3): the stored command line never holds a secret.

/// Most arguments kept; the rest are dropped.
pub const MAX_ARGS: usize = 256;
/// Longest stored argument, in bytes; longer ones are cut on a character boundary.
pub const MAX_ARG_LEN: usize = 1024;
/// Longest stored line, in bytes.
const MAX_LINE: usize = 16 * 1024;

/// Substrings that mark an option, header or query parameter name as secret.
const SECRET_PARTS: &[&str] = &[
    "password",
    "passwd",
    "pass",
    "token",
    "secret",
    "key",
    "auth",
    "authorization",
    "cookie",
    "session",
    "sig",
    "signature",
    "credential",
    "bearer",
    "apikey",
    "api-key",
];

/// Whether `name` (an option, header or query parameter name) looks secret.
///
/// Deny by default: any name containing one of [`SECRET_PARTS`] counts. Known harmless
/// words that merely contain a part (`author`, `signoff`) are removed first, and `--no-*`
/// negations never take a value.
fn secret_name(name: &str) -> bool {
    let lower = name.trim_start_matches('-').to_ascii_lowercase();
    if lower.starts_with("no-") {
        return false;
    }
    if lower.contains("authorization") {
        return true;
    }
    let cleaned = lower
        .replace("author", "")
        .replace("signoff", "")
        .replace("sign-off", "");
    SECRET_PARTS.iter().any(|p| cleaned.contains(p))
}

/// `Name: value` becomes `Name: ***`; a value without a colon becomes `***`.
fn redact_header(h: &str) -> String {
    match h.split_once(':') {
        Some((n, _)) => format!("{}: ***", n.trim_end()),
        None => "***".to_owned(),
    }
}

/// Whether `arg` is itself a header line naming a secret header.
fn secret_header_line(arg: &str) -> bool {
    arg.split_once(':').is_some_and(|(n, _)| {
        !n.is_empty()
            && !n.starts_with('-')
            && n.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            && secret_name(n)
    })
}

/// Redacts `argv` and joins it into one shell-quoted line, bounded in size.
///
/// Credentials are denied by default: URL userinfo (`scheme://user:pass@`) becomes
/// `scheme://***@`, secret-named query parameters keep only their name, the value of every
/// `-c key=value` becomes `***`, header values (`-H`, `--header`, `Authorization: ...`,
/// `http.extraHeader`) become `***`, and the value of any option whose name contains a
/// secret word (`--token`, `--api-key`, `--password`, ...) is dropped.
pub fn redact_argv(argv: &[String]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut it = argv.iter().take(MAX_ARGS);
    while let Some(arg) = it.next() {
        if arg == "-c" {
            out.push(arg.clone());
            if let Some(v) = it.next() {
                out.push(config_value(v));
            }
        } else if let Some(rest) = arg.strip_prefix("-c").filter(|r| r.contains('=')) {
            out.push(format!("-c{}", config_value(rest)));
        } else if arg == "-H" || arg.eq_ignore_ascii_case("--header") {
            out.push(arg.clone());
            if let Some(v) = it.next() {
                out.push(redact_header(v));
            }
        } else if let Some(h) = arg
            .strip_prefix("-H")
            .filter(|h| !h.is_empty() && !h.starts_with('-'))
        {
            out.push(format!("-H{}", redact_header(h)));
        } else if arg.starts_with('-') && !arg.contains('=') && secret_name(arg) {
            out.push(arg.clone());
            it.next();
        } else if let Some((name, value)) = arg.split_once('=').filter(|_| arg.starts_with('-')) {
            if name.eq_ignore_ascii_case("--header") {
                out.push(format!("{name}={}", redact_header(value)));
            } else if secret_name(name) {
                out.push(name.to_owned());
            } else {
                out.push(strip_url_credentials(arg));
            }
        } else if let Some((name, _)) = arg
            .split_once('=')
            .filter(|(n, _)| n.to_ascii_lowercase().contains("extraheader"))
        {
            out.push(format!("{name}=***"));
        } else if secret_header_line(arg) {
            out.push(redact_header(arg));
        } else {
            out.push(strip_url_credentials(arg));
        }
    }
    let mut line = String::new();
    for a in out {
        let a = quote(&truncate(&a, MAX_ARG_LEN));
        if line.len() + a.len() + 1 > MAX_LINE {
            break;
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&a);
    }
    line
}

fn config_value(kv: &str) -> String {
    match kv.split_once('=') {
        Some((k, _)) => format!("{}=***", strip_url_credentials(k)),
        None => kv.to_owned(),
    }
}

/// Masks URL userinfo and secret query values, and scp-like `user:pass@host:path`.
fn strip_url_credentials(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        // As git does: the authority runs to the first `/`, and the userinfo is
        // everything up to the last `@` within it.
        let end = tail.find('/').unwrap_or(tail.len());
        match tail[..end].rfind('@') {
            Some(at) => {
                out.push_str("***");
                rest = &tail[at..];
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    let masked = if s.contains("://") {
        out
    } else {
        scp_userinfo(&out)
    };
    redact_query(&masked)
}

/// `user:pass@host:path` becomes `***@host:path`; `git@host:path` is kept.
fn scp_userinfo(s: &str) -> String {
    if s.starts_with('-') || s.contains(char::is_whitespace) {
        return s.to_owned();
    }
    match s.rfind('@') {
        Some(at) if s[..at].contains(':') && !s[..at].contains('/') => {
            format!("***{}", &s[at..])
        }
        _ => s.to_owned(),
    }
}

/// Keeps only the name of every secret-named query parameter.
fn redact_query(s: &str) -> String {
    let Some(q) = s.find('?') else {
        return s.to_owned();
    };
    let (head, tail) = s.split_at(q + 1);
    let (query, frag) = match tail.find('#') {
        Some(h) => tail.split_at(h),
        None => (tail, ""),
    };
    let params: Vec<String> = query
        .split('&')
        .map(|p| match p.split_once('=') {
            Some((n, _)) if secret_name(&percent_decode(n)) => format!("{n}=***"),
            _ => p.to_owned(),
        })
        .collect();
    format!("{head}{}{frag}", params.join("&"))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

fn quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,^*".contains(c));
    if plain {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::redact_argv;

    fn r(a: &[&str]) -> String {
        redact_argv(&a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
    }

    fn check(cases: &[(&[&str], &str)]) {
        for (argv, want) in cases {
            assert_eq!(&r(argv), want, "'{argv:?}'");
        }
    }

    #[test]
    fn secret_options() {
        check(&[
            (&["--github-token", "abc", "x"], "--github-token x"),
            (&["--api-key", "abc", "x"], "--api-key x"),
            (&["--API_KEY=abc"], "--API_KEY"),
            (&["--apikey=abc"], "--apikey"),
            (&["--passwd", "p", "x"], "--passwd x"),
            (&["--cookie=a=b"], "--cookie"),
            (&["--session-id", "s", "x"], "--session-id x"),
            (&["--client-secret=s"], "--client-secret"),
            (&["--bearer", "b"], "--bearer"),
            (&["--sig", "b", "x"], "--sig x"),
            (&["--credential-helper=store"], "--credential-helper"),
            (&["--Authorization", "Basic abc", "x"], "--Authorization x"),
        ]);
    }

    #[test]
    fn non_secrets_stay_readable() {
        check(&[
            (&["stash", "--keep-index"], "stash --keep-index"),
            (
                &["commit", "--author", "A <a@b.c>", "-m", "hi"],
                "commit --author 'A <a@b.c>' -m hi",
            ),
            (
                &["commit", "--author=A", "--signoff", "-m", "hi"],
                "commit --author=A --signoff -m hi",
            ),
            (&["--no-auth", "x"], "--no-auth x"),
            (&["log", "--oneline", "-n", "3"], "log --oneline -n 3"),
            (&["https://host:8080/a?b=c"], "'https://host:8080/a?b=c'"),
            (&["git@host:org/repo.git"], "git@host:org/repo.git"),
            (&["https://h/a@b"], "https://h/a@b"),
        ]);
    }

    #[test]
    fn config_and_headers() {
        check(&[
            (&["-c", "a.b=1", "-c", "c.d=2"], "-c a.b=*** -c c.d=***"),
            (&["-chttp.extraHeader=Bearer x"], "-chttp.extraHeader=***"),
            (&["http.extraHeader=Bearer x"], "http.extraHeader=***"),
            (
                &["-H", "Authorization: Bearer x"],
                "-H 'Authorization: ***'",
            ),
            (&["--header", "X-Foo: bar"], "--header 'X-Foo: ***'"),
            (&["--header=Cookie: a=b"], "'--header=Cookie: ***'"),
            (&["Authorization: Basic abc"], "'Authorization: ***'"),
            (&["X-Api-Key: abc"], "'X-Api-Key: ***'"),
        ]);
    }

    #[test]
    fn urls() {
        check(&[
            (&["https://u:p@h/r"], "https://***@h/r"),
            (&["https://u:p?x#y@h/r"], "https://***@h/r"),
            (&["https://u:p@w@h/r"], "https://***@h/r"),
            (&["https://u%40x:p%2Fw@h/r"], "https://***@h/r"),
            (&["https://u:p@h"], "https://***@h"),
            (
                &["https://h/r?token=abc&a=b"],
                "'https://h/r?token=***&a=b'",
            ),
            (
                &["https://h/r?a=b&X-Amz-Signature=zz#f"],
                "'https://h/r?a=b&X-Amz-Signature=***#f'",
            ),
            (
                &["https://h/r?X-Amz-Credential=zz&Key=k"],
                "'https://h/r?X-Amz-Credential=***&Key=***'",
            ),
            (&["https://h/r?%74oken=zz"], "'https://h/r?%74oken=***'"),
            (&["user:pass@host:path"], "***@host:path"),
        ]);
    }
}
