//! The command reference, `docs/CLI.md` embedded at build time, as help text: `asc <command>
//! --help`, `asc <command> -h` and `asc help <command>` for a command git does not have print the
//! reference rows of that command, and `asc --help` ends with the groups of the reference. The file
//! is the single source; a test checks that every asc-only verb has a row.

/// The reference as written.
pub const CLI_MD: &str = include_str!("../../../../docs/CLI.md");

/// The width the descriptions are wrapped to.
const WIDTH: usize = 96;

/// One row of a reference table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The group (section heading without its number).
    pub group: String,
    /// The full names the row covers, e.g. `["agent token list", "agent token show"]`.
    pub names: Vec<String>,
    /// What the command does (empty for the plumbing table, which has none).
    pub what: String,
    /// One example, as typed.
    pub example: String,
}

/// The cells of a table line, split on `|` that is not escaped as `\|`.
fn cells(line: &str) -> Vec<String> {
    let inner = line.trim().trim_start_matches('|').trim_end_matches('|');
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cur.push('|');
                chars.next();
            }
            '|' => out.push(std::mem::take(&mut cur).trim().to_owned()),
            _ => cur.push(c),
        }
    }
    out.push(cur.trim().to_owned());
    out
}

/// Markdown to plain text: code spans lose their backticks, links keep their text.
fn plain(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut rest = md;
    while let Some(i) = rest.find('[') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after
            .find("](")
            .and_then(|j| after[j..].find(')').map(|k| (j, j + k)))
        {
            Some((j, end)) => {
                out.push_str(&after[..j]);
                rest = &after[end + 1..];
            }
            None => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.replace('`', "")
}

/// The names in a command cell: every code span; a one-word name after a longer first one takes
/// the first one's leading words (`agent token list`, `show` is `agent token show`).
fn names(cell: &str) -> Vec<String> {
    let spans: Vec<&str> = cell.split('`').skip(1).step_by(2).collect();
    let Some(first) = spans.first() else {
        return Vec::new();
    };
    let words: Vec<&str> = first.split_whitespace().collect();
    let prefix = words[..words.len().saturating_sub(1)].join(" ");
    spans
        .iter()
        .enumerate()
        .map(|(i, s)| {
            if i > 0 && !prefix.is_empty() && !s.contains(' ') {
                format!("{prefix} {s}")
            } else {
                (*s).to_owned()
            }
        })
        .collect()
}

/// Every row of every table of the reference.
pub fn rows() -> Vec<Row> {
    rows_of(CLI_MD)
}

fn rows_of(md: &str) -> Vec<Row> {
    let mut group = String::new();
    let mut out = Vec::new();
    for line in md.lines() {
        if let Some(h) = line.strip_prefix("## ") {
            group = match h.split_once(". ") {
                Some((n, t)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => t,
                _ => h,
            }
            .to_owned();
            continue;
        }
        if !line.starts_with("| `") {
            continue;
        }
        let c = cells(line);
        let names = names(&c[0]);
        if names.is_empty() {
            continue;
        }
        let (what, example) = match c.len() {
            2 => (String::new(), plain(&c[1])),
            n if n >= 3 => (plain(&c[1]), plain(&c[2])),
            _ => continue,
        };
        out.push(Row {
            group: group.clone(),
            names,
            what,
            example,
        });
    }
    out
}

fn first_word(name: &str) -> &str {
    name.split_whitespace().next().unwrap_or_default()
}

/// The rows of a command: `words` is the verb and the words typed after it (`["agent", "token"]`).
/// The rows narrow to the ones under the longest run of leading words that names some; when none
/// does, every row of the verb is kept. None when the reference has no row for the verb.
pub fn rows_for(words: &[&str]) -> Option<Vec<Row>> {
    rows_for_in(&rows(), words)
}

fn rows_for_in(all: &[Row], words: &[&str]) -> Option<Vec<Row>> {
    let verb = *words.first()?;
    let of_verb: Vec<Row> = all
        .iter()
        .filter(|r| r.names.iter().any(|n| first_word(n) == verb))
        .cloned()
        .collect();
    if of_verb.is_empty() {
        return None;
    }
    // The longest run of leading words some row is under: `agent show 7` narrows to `agent show`.
    for k in (2..=words.len()).rev() {
        let want = words[..k].join(" ");
        let under = format!("{want} ");
        let narrow: Vec<Row> = of_verb
            .iter()
            .filter(|r| r.names.iter().any(|n| *n == want || n.starts_with(&under)))
            .cloned()
            .collect();
        if !narrow.is_empty() {
            return Some(narrow);
        }
    }
    Some(of_verb)
}

/// `text` wrapped to [`WIDTH`], each line indented by `indent` spaces.
fn wrap(text: &str, indent: usize) -> String {
    let pad = " ".repeat(indent);
    let mut out = String::new();
    let mut line = String::new();
    for w in text.split_whitespace() {
        if !line.is_empty() && indent + line.len() + 1 + w.len() > WIDTH {
            out.push_str(&pad);
            out.push_str(&line);
            out.push('\n');
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(w);
    }
    if !line.is_empty() {
        out.push_str(&pad);
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// The help text of a command, or None when the reference has no row for it.
pub fn help_for(words: &[&str]) -> Option<String> {
    rows_for(words).map(|rows| render(&rows))
}

fn render(rows: &[Row]) -> String {
    let mut out = String::new();
    for r in rows {
        let names: Vec<String> = r.names.iter().map(|n| format!("asc {n}")).collect();
        out.push_str(&names.join(", "));
        out.push('\n');
        if !r.what.is_empty() {
            out.push_str(&wrap(&r.what, 4));
        }
        out.push_str(&wrap(&format!("Example: {}", r.example), 4));
        out.push('\n');
    }
    out.push_str("The whole reference is docs/CLI.md; asc help git maps git commands to asc.\n");
    out
}

/// The groups of the reference with their commands, for the end of `asc --help`.
pub fn groups() -> String {
    let mut seen: Vec<(String, Vec<String>)> = Vec::new();
    for r in rows() {
        let at = match seen.iter().position(|(g, _)| *g == r.group) {
            Some(i) => i,
            None => {
                seen.push((r.group.clone(), Vec::new()));
                seen.len() - 1
            }
        };
        for n in &r.names {
            let v = first_word(n).to_owned();
            if !seen[at].1.contains(&v) {
                seen[at].1.push(v);
            }
        }
    }
    let mut out = String::from("\nasc commands by group (asc <command> --help for one):\n");
    for (g, verbs) in seen {
        out.push_str("  ");
        out.push_str(&g);
        out.push_str(":\n");
        out.push_str(&wrap(&verbs.join(", "), 6));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_keep_escaped_pipes() {
        assert_eq!(
            cells(r"| `init` | a \| b | `x` |"),
            vec!["`init`", "a | b", "`x`"]
        );
    }

    #[test]
    fn names_take_the_leading_words_of_the_first() {
        assert_eq!(
            names("`agent token list`, `show`, `revoke`"),
            vec!["agent token list", "agent token show", "agent token revoke"]
        );
        assert_eq!(
            names("`sparse-checkout`, `sparse`"),
            vec!["sparse-checkout", "sparse"]
        );
        assert_eq!(
            names("`daemon start`, `stop`"),
            vec!["daemon start", "daemon stop"]
        );
    }

    #[test]
    fn plain_drops_code_spans_and_link_targets() {
        assert_eq!(
            plain("see [`CONFIG.md`](CONFIG.md) and `asc x`"),
            "see CONFIG.md and asc x"
        );
        assert_eq!(plain("a [b c"), "a [b c");
    }

    const SAMPLE: &str = "\
## 1. Set up

| Command | What it does | Example |
|---|---|---|
| `auth status` | Names the source. | `asc auth status https://h` |
| `auth login` | Stores a token. | `asc auth login --host https://h` |
| `doctor` | Checks the install. | `asc doctor` |

## 11. Plumbing

| Command | Example |
|---|---|
| `show-ref` | `asc show-ref --heads` |
";

    #[test]
    fn rows_parse_both_table_shapes() {
        let rows = rows_of(SAMPLE);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].group, "Set up");
        assert_eq!(rows[0].names, vec!["auth status"]);
        assert_eq!(rows[0].what, "Names the source.");
        assert_eq!(rows[0].example, "asc auth status https://h");
        assert_eq!(rows[3].group, "Plumbing");
        assert_eq!(rows[3].what, "");
        assert_eq!(rows[3].example, "asc show-ref --heads");
    }

    #[test]
    fn rows_for_narrows_to_a_subcommand_and_falls_back_to_the_verb() {
        let all = rows_of(SAMPLE);
        assert_eq!(rows_for_in(&all, &["auth"]).map(|r| r.len()), Some(2));
        let login = rows_for_in(&all, &["auth", "login"]).expect("login");
        assert_eq!(login.len(), 1);
        assert_eq!(login[0].names, vec!["auth login"]);
        assert_eq!(
            rows_for_in(&all, &["auth", "nope"]).map(|r| r.len()),
            Some(2)
        );
        assert_eq!(
            rows_for_in(&all, &["auth", "login", "extra"]).map(|r| r.len()),
            Some(1)
        );
        assert_eq!(rows_for_in(&all, &["nope"]), None);
    }

    #[test]
    fn render_names_examples_and_points_at_the_reference() {
        let all = rows_of(SAMPLE);
        let text = render(&rows_for_in(&all, &["auth", "login"]).expect("login"));
        assert!(text.starts_with(
            "asc auth login\n    Stores a token.\n    Example: asc auth login --host https://h\n"
        ));
        assert!(text.ends_with("docs/CLI.md; asc help git maps git commands to asc.\n"));
    }

    #[test]
    fn wrap_keeps_lines_inside_the_width() {
        let long = "word ".repeat(60);
        for l in wrap(&long, 4).lines() {
            assert!(l.len() <= WIDTH, "{l}");
            assert!(l.starts_with("    "));
        }
    }

    #[test]
    fn every_asc_only_verb_has_a_row() {
        let all = rows();
        for verb in crate::dispatch::ASC_ONLY_VERBS {
            assert!(
                rows_for_in(&all, &[verb]).is_some(),
                "docs/CLI.md has no row for asc {verb}"
            );
        }
    }

    #[test]
    fn groups_lists_every_group_of_the_reference() {
        let g = groups();
        assert!(g.contains("  Set up and look after an install:\n"));
        assert!(g.contains("  Plumbing:\n"));
        assert!(g.contains("auth"));
    }
}
