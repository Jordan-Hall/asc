//! Argv redaction (SPEC §20.3): the stored command line never holds a secret.

/// Most arguments kept; the rest are dropped.
pub const MAX_ARGS: usize = 256;
/// Longest stored argument, in bytes; longer ones are cut on a character boundary.
pub const MAX_ARG_LEN: usize = 1024;
/// Longest stored line, in bytes.
const MAX_LINE: usize = 16 * 1024;

/// Options whose value is dropped.
fn secret_option(name: &str) -> bool {
    name == "--password" || name == "--token" || name.starts_with("--auth")
}

/// Redacts `argv` and joins it into one shell-quoted line, bounded in size.
///
/// URL credentials (`scheme://user:pass@`) become `scheme://***@`, the value of every
/// `-c key=value` becomes `***`, and the value of any `--password`, `--token` or `--auth*`
/// option is dropped.
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
        } else if secret_option(arg) {
            out.push(arg.clone());
            it.next();
        } else if let Some((name, _)) = arg.split_once('=').filter(|(n, _)| secret_option(n)) {
            out.push(name.to_owned());
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

/// Replaces `user[:pass]@` after every `://` with `***@`.
fn strip_url_credentials(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail.find(['/', '?', '#']).unwrap_or(tail.len());
        match tail[..end].rfind('@') {
            Some(at) => {
                out.push_str("***");
                rest = &tail[at..];
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    out
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
