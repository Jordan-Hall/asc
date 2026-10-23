//! `range-diff` (SPEC §11.2): compares two ranges of commits. Each commit becomes a text of its
//! author, message and per-file patch (hunk positions removed); identical patches pair first,
//! the rest are paired by the cheapest assignment over the size of the diff between their
//! texts, and each pair prints as `n: old S m: new subject`, with the diff of the two texts
//! under it when they differ. `--creation-factor`, `--no-patch`, `--left-only`,
//! `--right-only` and `--[no-]dual-color` (no colour is drawn) are served.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write as _;
use std::process::ExitCode;

use asc_core::id::{GitOid, TreeId};
use asc_engine::diff::line::{self, Algorithm};
use asc_engine::diff::{
    DiffCtx, DiffOptions, NoAttributes, RenameOptions, Status, TreeChange, write_patch,
};

use super::commit::message::subject;
use super::common::{CliEngine, CliError, Ctx, Result, WsCtx};
use super::conflict;
use super::log::opts::Opts;
use super::log::revs;
use super::log::walk::{Hist, Walker};
use crate::native::NativeSpec;

/// Most commits in one range; the pairing is cubic in the total.
const MAX_COMMITS: usize = 600;
/// Largest patch text of one commit.
const MAX_PATCH_BYTES: usize = 64 << 20;
/// What pairing a patch with an already paired one costs.
const COST_MAX: i64 = 1 << 16;
const DEFAULT_CREATION_FACTOR: i64 = 60;
/// Longest function-context text (git's `struct func_line` buffer).
const FUNC_MAX: usize = 80;

const USAGE: &str = r####"usage: git range-diff [<options>] <old-base>..<old-tip> <new-base>..<new-tip>
   or: git range-diff [<options>] <old-tip>...<new-tip>
   or: git range-diff [<options>] <base> <old-tip> <new-tip>

    --[no-]creation-factor <n>
                          percentage by which creation is weighted
    --no-dual-color       use simple diff colors
    --dual-color          opposite of --no-dual-color
    --[no-]notes[=<notes>]
                          passed to 'git log'
    --[no-]diff-merges <style>
                          passed to 'git log'
    --[no-]max-memory <size>
                          maximum memory for cost matrix (default 4G)
    --[no-]remerge-diff   passed to 'git log'
    --[no-]left-only      only emit output related to the first range
    --[no-]right-only     only emit output related to the second range

Diff output format options
    -p, --patch           generate patch
    -s, --no-patch        suppress diff output
    -u                    generate patch
    -U, --unified[=<n>]   generate diffs with <n> lines context
    -W, --[no-]function-context
                          generate diffs with <n> lines context
    --raw                 generate the diff in raw format
    --patch-with-raw      synonym for '-p --raw'
    --patch-with-stat     synonym for '-p --stat'
    --numstat             machine friendly --stat
    --shortstat           output only the last line of --stat
    -X, --dirstat[=<param1>,<param2>...]
                          output the distribution of relative amount of changes for each sub-directory
    --cumulative          synonym for --dirstat=cumulative
    --dirstat-by-file[=<param1>,<param2>...]
                          synonym for --dirstat=files,<param1>,<param2>...
    --check               warn if changes introduce conflict markers or whitespace errors
    --summary             condensed summary such as creations, renames and mode changes
    --name-only           show only names of changed files
    --name-status         show only names and status of changed files
    --stat[=<width>[,<name-width>[,<count>]]]
                          generate diffstat
    --stat-width <width>  generate diffstat with a given width
    --stat-name-width <width>
                          generate diffstat with a given name width
    --stat-graph-width <width>
                          generate diffstat with a given graph width
    --stat-count <count>  generate diffstat with limited lines
    --[no-]compact-summary
                          generate compact summary in diffstat
    --binary              output a binary diff that can be applied
    --[no-]full-index     show full pre- and post-image object names on the "index" lines
    --[no-]color[=<when>] show colored diff
    --ws-error-highlight <kind>
                          highlight whitespace errors in the 'context', 'old' or 'new' lines in the diff
    -z                    do not munge pathnames and use NULs as output field terminators in --raw or --numstat
    --[no-]abbrev[=<n>]   use <n> digits to display object names
    --src-prefix <prefix> show the given source prefix instead of "a/"
    --dst-prefix <prefix> show the given destination prefix instead of "b/"
    --line-prefix <prefix>
                          prepend an additional prefix to every line of output
    --no-prefix           do not show any source or destination prefix
    --default-prefix      use default prefixes a/ and b/
    --inter-hunk-context <n>
                          show context between diff hunks up to the specified number of lines
    --output-indicator-new <char>
                          specify the character to indicate a new line instead of '+'
    --output-indicator-old <char>
                          specify the character to indicate an old line instead of '-'
    --output-indicator-context <char>
                          specify the character to indicate a context instead of ' '

Diff rename options
    -B, --break-rewrites[=<n>[/<m>]]
                          break complete rewrite changes into pairs of delete and create
    -M, --find-renames[=<n>]
                          detect renames
    -D, --irreversible-delete
                          omit the preimage for deletes
    -C, --find-copies[=<n>]
                          detect copies
    --[no-]find-copies-harder
                          use unmodified files as source to find copies
    --no-renames          disable rename detection
    --[no-]rename-empty   use empty blobs as rename source
    --[no-]follow         continue listing the history of a file beyond renames
    -l <n>                prevent rename/copy detection if the number of rename/copy targets exceeds given limit

Diff algorithm options
    --minimal             produce the smallest possible diff
    -w, --ignore-all-space
                          ignore whitespace when comparing lines
    -b, --ignore-space-change
                          ignore changes in amount of whitespace
    --ignore-space-at-eol ignore changes in whitespace at EOL
    --ignore-cr-at-eol    ignore carrier-return at the end of line
    --ignore-blank-lines  ignore changes whose lines are all blank
    -I, --[no-]ignore-matching-lines <regex>
                          ignore changes whose all lines match <regex>
    --[no-]indent-heuristic
                          heuristic to shift diff hunk boundaries for easy reading
    --patience            generate diff using the "patience diff" algorithm
    --histogram           generate diff using the "histogram diff" algorithm
    --diff-algorithm <algorithm>
                          choose a diff algorithm
    --anchored <text>     generate diff using the "anchored diff" algorithm
    --word-diff[=<mode>]  show word diff, using <mode> to delimit changed words
    --word-diff-regex <regex>
                          use <regex> to decide what a word is
    --color-words[=<regex>]
                          equivalent to --word-diff=color --word-diff-regex=<regex>
    --[no-]color-moved[=<mode>]
                          moved lines of code are colored differently
    --[no-]color-moved-ws <mode>
                          how white spaces are ignored in --color-moved

Other diff options
    --[no-]relative[=<prefix>]
                          when run from subdir, exclude changes outside and show relative paths
    -a, --[no-]text       treat all files as text
    -R                    swap two inputs, reverse the diff
    --[no-]exit-code      exit with 1 if there were differences, 0 otherwise
    --[no-]quiet          disable all output of the program
    --[no-]ext-diff       allow an external diff helper to be executed
    --[no-]textconv       run external text conversion filters when comparing binary files
    --ignore-submodules[=<when>]
                          ignore changes to submodules in the diff generation
    --submodule[=<format>]
                          specify how differences in submodules are shown
    --ita-invisible-in-index
                          hide 'git add -N' entries from the index
    --ita-visible-in-index
                          treat 'git add -N' entries as real in the index
    -S <string>           look for differences that change the number of occurrences of the specified string
    -G <regex>            look for differences that change the number of occurrences of the specified regex
    --pickaxe-all         show all changes in the changeset with -S or -G
    --pickaxe-regex       treat <string> in -S as extended POSIX regular expression
    -O <file>             control the order in which files appear in the output
    --rotate-to <path>    show the change in the specified path first
    --skip-to <path>      skip the output to the specified path
    --find-object <object-id>
                          look for differences that change the number of occurrences of the specified object
    --diff-filter [(A|C|D|M|R|T|U|X|B)...[*]]
                          select files by diff type
    --max-depth <depth>   maximum tree depth to recurse
    --output <file>       output to a specific file

"####;

#[derive(clap::Parser, Debug)]
#[command(
    name = "range-diff",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "range-diff",
    flags: &[
        "--creation-factor",
        "--no-creation-factor",
        "--dual-color",
        "--no-dual-color",
        "--left-only",
        "--no-left-only",
        "--right-only",
        "--no-right-only",
        "-s",
        "--no-patch",
    ],
    placeholders: &[],
}];

fn fatal(text: impl Into<String>) -> CliError {
    CliError::Fatal(text.into())
}

struct Options {
    creation_factor: i64,
    no_patch: bool,
    left_only: bool,
    right_only: bool,
    words: Vec<String>,
}

fn parse(args: &[OsString]) -> Result<Options> {
    let mut o = Options {
        creation_factor: DEFAULT_CREATION_FACTOR,
        no_patch: false,
        left_only: false,
        right_only: false,
        words: Vec::new(),
    };
    let mut it = args.iter().map(|a| a.to_string_lossy().into_owned());
    let mut done = false;
    while let Some(arg) = it.next() {
        if done || !arg.starts_with('-') || arg == "-" {
            o.words.push(arg);
            continue;
        }
        if arg == "--" {
            done = true;
            continue;
        }
        let (name, inline) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(n, v)| (n, Some(v.to_owned())));
        match name {
            "-s" | "--no-patch" => o.no_patch = true,
            "--dual-color" | "--no-dual-color" => {}
            "--left-only" => o.left_only = true,
            "--no-left-only" => o.left_only = false,
            "--right-only" => o.right_only = true,
            "--no-right-only" => o.right_only = false,
            "--creation-factor" => {
                let v = inline.or_else(|| it.next()).ok_or_else(|| {
                    CliError::Raw(
                        "error: option `creation-factor' requires a value".into(),
                        129,
                    )
                })?;
                o.creation_factor = v.parse::<i64>().map_err(|_| {
                    CliError::Raw(
                        "error: option `creation-factor' expects an integer value with an optional k/m/g suffix".into(),
                        129,
                    )
                })?;
            }
            "--no-creation-factor" => o.creation_factor = DEFAULT_CREATION_FACTOR,
            other => {
                return Err(CliError::Usage(format!(
                    "error: unknown option `{}'\n{}",
                    other.trim_start_matches('-'),
                    USAGE.trim_end()
                )));
            }
        }
    }
    if o.left_only && o.right_only {
        return Err(CliError::Raw(
            "error: options '--left-only' and '--right-only' cannot be used together".into(),
            255,
        ));
    }
    Ok(o)
}

// ---------------------------------------------------------------- patch text

/// One file of a commit's patch: its heading and the hunk lines, as `range-diff` writes them.
pub(super) struct FileSection {
    /// `name`, `name (new)`, `old => new`, with a mode change appended.
    pub heading: String,
    /// The text of the hunks: `@@ name: function` lines and the body lines, newline ended.
    pub body: Vec<u8>,
}

fn heading_of(c: &TreeChange) -> String {
    let mut s = match c.status {
        Status::Added => format!("{} (new)", c.path),
        Status::Deleted => format!("{} (deleted)", c.path),
        Status::Renamed(_) | Status::Copied(_) => format!("{} => {}", c.old_path, c.path),
        _ => c.path.to_string(),
    };
    if !matches!(c.status, Status::Added | Status::Deleted)
        && c.old.is_some()
        && c.new.is_some()
        && c.old_mode() != c.new_mode()
    {
        s.push_str(&format!(
            " (mode change {:06o} => {:06o})",
            c.old_mode(),
            c.new_mode()
        ));
    }
    s
}

/// The patch of `old` -> `new` split by file, rename detection on and no path prefixes, like
/// `git log -p --no-prefix`.
pub(super) async fn file_sections(
    ws: &WsCtx,
    old: TreeId,
    new: TreeId,
    abbrev: usize,
    renames: bool,
) -> Result<Vec<FileSection>> {
    let opts = DiffOptions {
        renames: renames.then(RenameOptions::default),
        no_prefix: true,
        abbrev,
        ..DiffOptions::default()
    };
    let changes = ws
        .engine
        .diff_trees(old, new, None, &opts)
        .await
        .map_err(|e| fatal(e.to_string()))?;
    let ctx = DiffCtx {
        opts: &opts,
        content: &ws.engine,
        attrs: &NoAttributes,
    };
    let mut out = Vec::with_capacity(changes.len());
    let mut total = 0usize;
    for c in &changes {
        let mut raw: Vec<u8> = Vec::new();
        write_patch(&mut raw, &ctx, std::slice::from_ref(c))
            .await
            .map_err(|e| fatal(e.to_string()))?;
        total += raw.len();
        if total > MAX_PATCH_BYTES {
            return Err(fatal("asc: range-diff: a patch is too large"));
        }
        let mut body: Vec<u8> = Vec::new();
        let mut in_hunks = false;
        for l in raw.split_inclusive(|&b| b == b'\n') {
            if let Some(rest) = l.strip_prefix(b"@@ ") {
                in_hunks = true;
                // "-a,b +c,d @@ function": the function text follows the second "@@".
                let func = find_sub(rest, b" @@").map(|at| &rest[at + 3..]);
                body.extend_from_slice(b"@@");
                if let Some(f) = func {
                    let f = f.strip_prefix(b" ").unwrap_or(f);
                    let f = f.strip_suffix(b"\n").unwrap_or(f);
                    if !f.is_empty() {
                        body.extend_from_slice(format!(" {}: ", c.path).as_bytes());
                        body.extend_from_slice(f);
                    }
                }
                body.push(b'\n');
            } else if in_hunks || l.starts_with(b"Binary files ") {
                body.extend_from_slice(l);
                if !l.ends_with(b"\n") {
                    body.push(b'\n');
                }
            }
        }
        out.push(FileSection {
            heading: heading_of(c),
            body,
        });
    }
    Ok(out)
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A commit of a range with the text that is compared.
struct Patch {
    git: GitOid,
    subject: String,
    /// The whole text: metadata, message and files.
    text: Vec<u8>,
    /// Where the file part begins in `text`.
    diff_offset: usize,
    /// Lines of the file part.
    diff_lines: usize,
    matching: Option<usize>,
}

impl Patch {
    fn diff(&self) -> &[u8] {
        &self.text[self.diff_offset..]
    }
}

async fn patches_of(ctx: &Ctx, ws: &WsCtx, range: &str, abbrev: usize) -> Result<Vec<Patch>> {
    let mut o = Opts::parse(&["--no-merges".into(), OsString::from(range)])?;
    o.reverse = false;
    let hist = Hist::select(ws);
    let sel = revs::select(ctx, hist.api(ws), &o).await?;
    let mut walker = Walker::new(ws, &hist, &sel, &o).await?;
    let mut entries = Vec::new();
    while let Some(e) = walker.next().await? {
        if entries.len() >= MAX_COMMITS {
            return Err(fatal(format!(
                "asc: range-diff: more than {MAX_COMMITS} commits in a range"
            )));
        }
        entries.push(e);
    }
    // Oldest first.
    entries.reverse();
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let parent_root = match e.cs.parents.first() {
            Some(&p) => ws.engine.get_changeset(p).await?.root,
            None => CliEngine::empty_tree_id(),
        };
        let sections = file_sections(ws, parent_root, e.cs.root, abbrev, true).await?;
        let mut text: Vec<u8> = Vec::new();
        text.extend_from_slice(b" ## Metadata ##\nAuthor: ");
        text.extend_from_slice(e.cs.author.name());
        text.extend_from_slice(b" <");
        text.extend_from_slice(e.cs.author.email());
        text.extend_from_slice(b">\n\n ## Commit message ##\n");
        let msg = &e.cs.message;
        let end = msg
            .iter()
            .rposition(|b| !b.is_ascii_whitespace())
            .map_or(0, |p| p + 1);
        for l in msg[..end].split(|&b| b == b'\n') {
            let t = l.trim_ascii_end();
            if !t.is_empty() {
                text.extend_from_slice(b"    ");
                text.extend_from_slice(t);
            }
            text.push(b'\n');
        }
        let mut diff_offset = text.len();
        let mut first = true;
        for s in &sections {
            text.push(b'\n');
            if first {
                diff_offset = text.len();
                first = false;
            }
            text.extend_from_slice(format!(" ## {} ##\n", s.heading).as_bytes());
            text.extend_from_slice(&s.body);
        }
        let diff_lines = text[diff_offset..].iter().filter(|&&b| b == b'\n').count();
        out.push(Patch {
            git: conflict::git_id(ws, e.id).await?,
            subject: subject(&e.cs.message),
            text,
            diff_offset,
            diff_lines,
            matching: None,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------- pairing

/// Lines a diff of `a` against `b` prints with three lines of context: a header per hunk, the
/// lines of the hunk on the old side and the added lines.
fn diff_size(a: &[u8], b: &[u8]) -> i64 {
    let d = line::diff(a, b, Algorithm::Myers, false);
    d.groups(3, 0)
        .iter()
        .map(|g| {
            let added: usize = d.hunks[g.hunks.clone()].iter().map(|h| h.after.len()).sum();
            (1 + g.old.len() + added) as i64
        })
        .sum()
}

/// The assignment of rows to columns of the square `cost` matrix that minimises the total
/// (the Hungarian algorithm with potentials).
fn assign(n: usize, cost: &[i64]) -> Vec<usize> {
    const INF: i64 = i64::MAX / 4;
    let mut u = vec![0i64; n + 1];
    let mut v = vec![0i64; n + 1];
    let mut p = vec![0usize; n + 1];
    let mut way = vec![0usize; n + 1];
    for i in 1..=n {
        p[0] = i;
        let mut j0 = 0usize;
        let mut minv = vec![INF; n + 1];
        let mut used = vec![false; n + 1];
        loop {
            used[j0] = true;
            let i0 = p[j0];
            let (mut delta, mut j1) = (INF, 0usize);
            for j in 1..=n {
                if used[j] {
                    continue;
                }
                let cur = cost[(i0 - 1) * n + (j - 1)] - u[i0] - v[j];
                if cur < minv[j] {
                    minv[j] = cur;
                    way[j] = j0;
                }
                if minv[j] < delta {
                    delta = minv[j];
                    j1 = j;
                }
            }
            for j in 0..=n {
                if used[j] {
                    u[p[j]] += delta;
                    v[j] -= delta;
                } else {
                    minv[j] -= delta;
                }
            }
            j0 = j1;
            if p[j0] == 0 {
                break;
            }
        }
        loop {
            let j1 = way[j0];
            p[j0] = p[j1];
            j0 = j1;
            if j0 == 0 {
                break;
            }
        }
    }
    let mut row_to_col = vec![0usize; n];
    for j in 1..=n {
        if p[j] > 0 {
            row_to_col[p[j] - 1] = j - 1;
        }
    }
    row_to_col
}

fn correspond(a: &mut [Patch], b: &mut [Patch], creation_factor: i64) {
    // Identical file parts pair first, in order.
    let mut by_diff: HashMap<&[u8], Vec<usize>> = HashMap::new();
    for (i, p) in a.iter().enumerate() {
        by_diff.entry(p.diff()).or_default().push(i);
    }
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for (j, q) in b.iter().enumerate() {
        if let Some(list) = by_diff.get_mut(q.diff())
            && !list.is_empty()
        {
            let i = list.remove(0);
            pairs.push((i, j));
        }
    }
    for (i, j) in pairs {
        a[i].matching = Some(j);
        b[j].matching = Some(i);
    }

    let (na, nb) = (a.len(), b.len());
    let n = na + nb;
    if n == 0 {
        return;
    }
    let mut cost = vec![0i64; n * n];
    for i in 0..na {
        for j in 0..nb {
            cost[i * n + j] = if a[i].matching == Some(j) {
                0
            } else if a[i].matching.is_some() || b[j].matching.is_some() {
                COST_MAX
            } else {
                diff_size(a[i].diff(), b[j].diff())
            };
        }
        let c = if a[i].matching.is_none() {
            a[i].diff_lines as i64 * creation_factor / 100
        } else {
            COST_MAX
        };
        for j in nb..n {
            cost[i * n + j] = c;
        }
    }
    for j in 0..nb {
        let c = if b[j].matching.is_none() {
            b[j].diff_lines as i64 * creation_factor / 100
        } else {
            COST_MAX
        };
        for i in na..n {
            cost[i * n + j] = c;
        }
    }
    let row_to_col = assign(n, &cost);
    for i in 0..na {
        let j = row_to_col[i];
        if j < nb {
            a[i].matching = Some(j);
            b[j].matching = Some(i);
        }
    }
}

// ---------------------------------------------------------------- output

fn pair_header(
    a: Option<(usize, &Patch)>,
    b: Option<(usize, &Patch)>,
    width: usize,
    status: char,
) -> String {
    let dashes = "-".repeat(7);
    let short = |p: &Patch| p.git.to_string()[..7].to_owned();
    let mut s = String::new();
    match a {
        Some((i, p)) => s.push_str(&format!("{:>width$}:  {} ", i + 1, short(p))),
        None => s.push_str(&format!("{:>width$}:  {dashes} ", "-")),
    }
    s.push(status);
    match b {
        Some((j, p)) => s.push_str(&format!(" {:>width$}:  {}", j + 1, short(p))),
        None => s.push_str(&format!(" {:>width$}:  {dashes}", "-")),
    }
    let subj = a.or(b).map(|(_, p)| p.subject.as_str()).unwrap_or_default();
    s.push(' ');
    s.push_str(subj);
    s.push('\n');
    s
}

/// The function-context text of a line of a patch text: a file heading or a hunk heading.
fn funcline(line: &[u8]) -> Option<&[u8]> {
    let l = line.strip_suffix(b"\n").unwrap_or(line);
    let text = if let Some(inner) = l.strip_prefix(b" ## ").and_then(|r| r.strip_suffix(b" ##")) {
        inner
    } else if let Some(rest) = l.strip_prefix(b"@@ ") {
        rest
    } else if l.len() > 1 && l[1..].starts_with(b"@@ ") {
        &l[4..]
    } else {
        return None;
    };
    let mut len = text.len().min(FUNC_MAX);
    while len > 0 && text[len - 1].is_ascii_whitespace() {
        len -= 1;
    }
    Some(&text[..len])
}

/// The diff of two patch texts, every line indented by four spaces.
fn patch_diff(a: &[u8], b: &[u8]) -> Vec<u8> {
    let d = line::diff(a, b, Algorithm::Myers, true);
    let mut out: Vec<u8> = Vec::new();
    let mut origin: isize = -1;
    let mut current: Option<&[u8]> = None;
    let emit = |out: &mut Vec<u8>, c: u8, l: &[u8]| {
        out.extend_from_slice(b"    ");
        out.push(c);
        out.extend_from_slice(l);
        if !l.ends_with(b"\n") {
            out.extend_from_slice(b"\n\\ No newline at end of file\n");
        }
    };
    for g in d.groups(3, 0) {
        let start = g.old.start;
        let mut l = start as isize - 1;
        while l != origin && l >= 0 {
            if let Some(f) = funcline(d.old[l as usize]) {
                current = Some(f);
                break;
            }
            l -= 1;
        }
        origin = start as isize - 1;
        out.extend_from_slice(b"    @@");
        if let Some(f) = current {
            out.push(b' ');
            out.extend_from_slice(f);
        }
        out.push(b'\n');
        let mut oi = g.old.start;
        for h in &d.hunks[g.hunks.clone()] {
            let (b0, b1) = (h.before.start as usize, h.before.end as usize);
            let (a0, a1) = (h.after.start as usize, h.after.end as usize);
            while oi < b0 {
                emit(&mut out, b' ', d.old[oi]);
                oi += 1;
            }
            for l in &d.old[b0..b1] {
                emit(&mut out, b'-', l);
            }
            for l in &d.new[a0..a1] {
                emit(&mut out, b'+', l);
            }
            oi = b1;
        }
        while oi < g.old.end {
            emit(&mut out, b' ', d.old[oi]);
            oi += 1;
        }
    }
    out
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let o = parse(&a.rest)?;
    let ws = ctx.ws()?;
    let (old_range, new_range) = match o.words.as_slice() {
        [one] if one.contains("...") => {
            let (r1, r2) = one.split_once("...").unwrap_or_default();
            if r1.is_empty() || r2.is_empty() {
                return Err(CliError::Raw(
                    format!("fatal: need two commit ranges\n\n{}\n", USAGE.trim_end()),
                    129,
                ));
            }
            (format!("{r2}..{r1}"), format!("{r1}..{r2}"))
        }
        [x, y] if x.contains("..") && y.contains("..") => (x.clone(), y.clone()),
        [base, old, new] => (format!("{base}..{old}"), format!("{base}..{new}")),
        _ => {
            return Err(CliError::Raw(
                format!("fatal: need two commit ranges\n\n{}\n", USAGE.trim_end()),
                129,
            ));
        }
    };
    for range in [&old_range, &new_range] {
        for side in range
            .replace("...", "..")
            .split("..")
            .filter(|p| !p.is_empty())
        {
            let word = side.trim_start_matches('^');
            if super::revparse::commit_of(ctx, word).await?.is_none() {
                return Err(CliError::Raw(format!("fatal: bad revision '{word}'"), 128));
            }
        }
    }
    let abbrev = ctx
        .config
        .get("core.abbrev")
        .and_then(|v| v.parse::<usize>().ok())
        .map_or(7, |n| n.clamp(4, 40));
    let mut left = patches_of(ctx, ws, &old_range, abbrev).await?;
    let mut right = patches_of(ctx, ws, &new_range, abbrev).await?;
    correspond(&mut left, &mut right, o.creation_factor);

    let width = left.len().max(right.len()).to_string().len();
    let mut out: Vec<u8> = Vec::new();
    let mut shown = vec![false; left.len()];
    let (mut i, mut j) = (0usize, 0usize);
    while i < left.len() || j < right.len() {
        while i < left.len() && shown[i] {
            i += 1;
        }
        if i < left.len() && left[i].matching.is_none() {
            if !o.right_only {
                out.extend_from_slice(
                    pair_header(Some((i, &left[i])), None, width, '<').as_bytes(),
                );
            }
            i += 1;
            continue;
        }
        while j < right.len() && right[j].matching.is_none() {
            if !o.left_only {
                out.extend_from_slice(
                    pair_header(None, Some((j, &right[j])), width, '>').as_bytes(),
                );
            }
            j += 1;
        }
        if j < right.len() {
            let Some(k) = right[j].matching else { break };
            let (pa, pb) = (&left[k], &right[j]);
            let same = pa.text == pb.text;
            let status = if same { '=' } else { '!' };
            out.extend_from_slice(
                pair_header(Some((k, pa)), Some((j, pb)), width, status).as_bytes(),
            );
            if !same && !o.no_patch {
                out.extend_from_slice(&patch_diff(&pa.text, &pb.text));
            }
            shown[k] = true;
            j += 1;
        }
    }
    let mut stdout = std::io::stdout().lock();
    match stdout.write_all(&out).and_then(|()| stdout.flush()) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        other => other?,
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assignment_minimises_cost() {
        // Rows pick columns 1, 0.
        let cost = [5, 1, 1, 5];
        assert_eq!(assign(2, &cost), vec![1, 0]);
    }

    #[test]
    fn funclines_come_from_headings() {
        assert_eq!(funcline(b" ## f (new) ##\n"), Some(&b"f (new)"[..]));
        assert_eq!(funcline(b"@@ f: line 21\n"), Some(&b"f: line 21"[..]));
        assert_eq!(funcline(b"@@\n"), None);
        assert_eq!(funcline(b" +added\n"), None);
    }

    #[test]
    fn diff_size_counts_headers_and_lines() {
        assert_eq!(diff_size(b"a\nb\n", b"a\nb\n"), 0);
        assert_eq!(diff_size(b"a\nb\n", b"a\nc\n"), 4);
    }

    #[test]
    fn patch_diff_indents_and_headers() {
        let a = b" ## Metadata ##\nx\ny\n";
        let b = b" ## Metadata ##\nx\nz\n";
        let got = String::from_utf8(patch_diff(a, b)).unwrap();
        assert_eq!(
            got,
            "    @@\n      ## Metadata ##\n     x\n    -y\n    +z\n"
        );
    }
}
