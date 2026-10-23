//! The interactive rebase todo list (SPEC §4.4, U27d): git's todo grammar (`pick reword edit
//! squash fixup drop exec break label reset merge update-ref noop`), the text the editor sees, the
//! `--autosquash` and `--exec` rearrangements, the squash message files and the status text of a
//! rebase in progress.
//!
//! Everything here is plain text work with no repository access, so it can be tested without a
//! workspace. The caller resolves commit names (the todo editor may leave abbreviations or refs
//! in a line) and passes the answers in. A todo file is untrusted input: line length and line
//! count are capped, and every error is reported with git's wording and the line number.

use std::collections::HashMap;
use std::path::Path;

use asc_core::id::GitOid;

use super::commit::message;
use super::common::{CliError, Ctx, Result};
use super::seqstate;

/// Most todo lines accepted; a longer list is refused instead of truncated.
pub const MAX_TODO: usize = 100_000;
/// Longest todo line accepted.
pub const MAX_LINE: usize = 64 * 1024;

// ---------------------------------------------------------------- the model

/// A todo command. `Comment` stands for a comment or blank line, which git keeps in the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Pick,
    Reword,
    Edit,
    Squash,
    Fixup,
    Exec,
    Break,
    Drop,
    Label,
    Reset,
    Merge,
    UpdateRef,
    Noop,
    Comment,
}

impl Op {
    /// The command as git writes it.
    pub fn word(self) -> &'static str {
        match self {
            Self::Pick => "pick",
            Self::Reword => "reword",
            Self::Edit => "edit",
            Self::Squash => "squash",
            Self::Fixup => "fixup",
            Self::Exec => "exec",
            Self::Break => "break",
            Self::Drop => "drop",
            Self::Label => "label",
            Self::Reset => "reset",
            Self::Merge => "merge",
            Self::UpdateRef => "update-ref",
            Self::Noop => "noop",
            Self::Comment => "",
        }
    }

    fn from_word(word: &str) -> Option<Self> {
        Some(match word {
            "pick" | "p" => Self::Pick,
            "reword" | "r" => Self::Reword,
            "edit" | "e" => Self::Edit,
            "squash" | "s" => Self::Squash,
            "fixup" | "f" => Self::Fixup,
            "exec" | "x" => Self::Exec,
            "break" | "b" => Self::Break,
            "drop" | "d" => Self::Drop,
            "label" | "l" => Self::Label,
            "reset" | "t" => Self::Reset,
            "merge" | "m" => Self::Merge,
            "update-ref" | "u" => Self::UpdateRef,
            "noop" => Self::Noop,
            _ => return None,
        })
    }

    /// Whether the command names a commit as its first argument.
    pub fn has_commit(self) -> bool {
        matches!(
            self,
            Self::Pick | Self::Reword | Self::Edit | Self::Squash | Self::Fixup | Self::Drop
        )
    }

    /// `squash` and `fixup`: the commands that fold into the previous commit.
    pub fn is_fixup(self) -> bool {
        matches!(self, Self::Squash | Self::Fixup)
    }

    /// Whether running the command writes a commit (what `--exec` follows).
    pub fn makes_commit(self) -> bool {
        matches!(
            self,
            Self::Pick | Self::Reword | Self::Edit | Self::Squash | Self::Fixup | Self::Merge
        )
    }
}

/// The `-C` / `-c` flag of `fixup` and `merge`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keep {
    No,
    /// `-C`: use that commit's message as it is.
    Message,
    /// `-c`: use that commit's message and open the editor on it.
    Edit,
}

impl Keep {
    fn flag(self) -> &'static str {
        match self {
            Self::No => "",
            Self::Message => "-C",
            Self::Edit => "-c",
        }
    }
}

/// One todo line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub op: Op,
    pub keep: Keep,
    /// The commit of `pick`, `reword`, `edit`, `squash`, `fixup`, `drop`; the `-C`/`-c` commit of
    /// `merge`.
    pub oid: Option<GitOid>,
    /// Everything after the commit: the `# subject` comment of a pick, the command of `exec`, the
    /// label of `label`/`reset`/`merge`, the ref of `update-ref`; the whole line of a comment.
    pub arg: String,
}

impl Item {
    pub fn new(op: Op, oid: Option<GitOid>, arg: impl Into<String>) -> Self {
        Self {
            op,
            keep: Keep::No,
            oid,
            arg: arg.into(),
        }
    }

    pub fn comment(text: impl Into<String>) -> Self {
        Self::new(Op::Comment, None, text)
    }

    pub fn blank() -> Self {
        Self::comment("")
    }

    /// Whether the line counts as a command (everything but comments and blank lines).
    pub fn counts(&self) -> bool {
        self.op != Op::Comment
    }

    /// The line without its newline; `short` abbreviates commit ids (the editor sees short ids,
    /// the state files full ones).
    pub fn render(&self, short: &dyn Fn(&GitOid) -> String) -> String {
        let mut out = String::from(self.op.word());
        if self.op == Op::Comment {
            return self.arg.clone();
        }
        if self.keep != Keep::No {
            out.push(' ');
            out.push_str(self.keep.flag());
        }
        if let Some(oid) = &self.oid {
            out.push(' ');
            out.push_str(&short(oid));
        }
        if !self.arg.is_empty() {
            out.push(' ');
            out.push_str(&self.arg);
        }
        out
    }
}

fn full(oid: &GitOid) -> String {
    oid.to_string()
}

/// The list as the state files hold it: full ids, one line each.
pub fn render_full(list: &[Item]) -> String {
    let mut out = String::new();
    for item in list {
        out.push_str(&item.render(&full));
        out.push('\n');
    }
    out
}

/// The list with the ids shortened by `short`.
pub fn render_short(list: &[Item], short: &dyn Fn(&GitOid) -> String) -> String {
    let mut out = String::new();
    for item in list {
        out.push_str(&item.render(short));
        out.push('\n');
    }
    out
}

/// The number of commands in the list (comments and blank lines do not count).
pub fn count(list: &[Item]) -> usize {
    list.iter().filter(|i| i.counts()).count()
}

/// The next command after position `at` (comments skipped), as git's `peek_command`.
pub fn peek(list: &[Item], from: usize) -> Option<&Item> {
    list.iter().skip(from).find(|i| i.counts())
}

// ---------------------------------------------------------------- parsing

/// A line split into its parts, the commit still a name.
#[derive(Debug)]
struct Lexed {
    op: Op,
    keep: Keep,
    token: Option<String>,
    arg: String,
}

enum LexError {
    /// Plain message.
    Msg(String),
    /// `break`/`noop` with an argument; git prints the rest of the buffer from the argument on.
    NoArgs { op: Op, offset: usize },
}

fn blank_or_comment(line: &str, comment: u8) -> bool {
    let t = line.trim_start_matches([' ', '\t']);
    t.is_empty() || t.as_bytes()[0] == comment
}

fn first_word(text: &str) -> (&str, &str) {
    let end = text.find([' ', '\t']).unwrap_or(text.len());
    (&text[..end], text[end..].trim_start_matches([' ', '\t']))
}

fn lex(line: &str, comment: u8) -> std::result::Result<Lexed, LexError> {
    let bol = line.trim_start_matches([' ', '\t']);
    let (word, mut rest) = first_word(bol);
    let Some(op) = Op::from_word(word) else {
        return Err(LexError::Msg(format!("invalid command '{word}'")));
    };
    let offset = line.len() - rest.len();
    let mut keep = Keep::No;
    let mut token = None;
    let missing = || LexError::Msg(format!("missing arguments for {}", op.word()));
    match op {
        Op::Noop | Op::Break => {
            if !rest.is_empty() {
                return Err(LexError::NoArgs { op, offset });
            }
        }
        Op::Exec | Op::Label | Op::Reset | Op::UpdateRef => {
            if rest.is_empty() {
                return Err(missing());
            }
        }
        Op::Merge => {
            for (flag, k) in [("-C", Keep::Message), ("-c", Keep::Edit)] {
                if let Some(after) = rest.strip_prefix(flag)
                    && after.starts_with([' ', '\t'])
                {
                    let (name, tail) = first_word(after.trim_start_matches([' ', '\t']));
                    if name.is_empty() {
                        return Err(missing());
                    }
                    keep = k;
                    token = Some(name.to_owned());
                    rest = tail;
                    break;
                }
            }
            if rest.is_empty() {
                return Err(missing());
            }
        }
        Op::Comment => unreachable!("comments are lexed by the caller"),
        _ => {
            if op == Op::Fixup {
                for (flag, k) in [("-C", Keep::Message), ("-c", Keep::Edit)] {
                    if let Some(after) = rest.strip_prefix(flag)
                        && after.starts_with([' ', '\t'])
                    {
                        keep = k;
                        rest = after.trim_start_matches([' ', '\t']);
                        break;
                    }
                }
            }
            if rest.is_empty() || rest.as_bytes()[0] == comment {
                return Err(missing());
            }
            let (name, tail) = first_word(rest);
            token = Some(name.to_owned());
            rest = tail;
        }
    }
    Ok(Lexed {
        op,
        keep,
        token,
        arg: rest.trim_end_matches(['\r']).to_owned(),
    })
}

/// The commit names a todo text refers to, in order, so the caller can resolve them.
pub fn tokens(text: &str, comment: u8) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines().take(MAX_TODO + 1) {
        if line.len() > MAX_LINE || blank_or_comment(line, comment) {
            continue;
        }
        if let Ok(Lexed { token: Some(t), .. }) = lex(line, comment) {
            out.push(t);
        }
    }
    out
}

/// Parses a todo text. `known` maps every name [`tokens`] returned to its commit (absent: not a
/// commit). On failure the errors are git's lines without the `error: ` prefix, in order.
pub fn parse(
    text: &str,
    comment: u8,
    known: &HashMap<String, GitOid>,
    check_fixups: bool,
) -> std::result::Result<Vec<Item>, Vec<String>> {
    let mut items = Vec::new();
    let mut errors = Vec::new();
    let mut start = 0usize;
    // git refuses a `squash`/`fixup` that has no commit before it to fold into.
    let mut seen_nonfixup = false;
    for (n, raw) in text.split_inclusive('\n').enumerate() {
        let line_start = start;
        start += raw.len();
        let line = raw.trim_end_matches(['\n', '\r']);
        if items.len() >= MAX_TODO || line.len() > MAX_LINE {
            errors.push("the todo list is too large".to_owned());
            return Err(errors);
        }
        if line.trim_matches([' ', '\t']).is_empty() {
            items.push(Item::blank());
            continue;
        }
        if blank_or_comment(line, comment) {
            items.push(Item::comment(line));
            continue;
        }
        match lex(line, comment) {
            Ok(lx) => {
                let oid = match &lx.token {
                    None => None,
                    Some(t) => match known.get(t) {
                        Some(o) => Some(*o),
                        None => {
                            errors.push(format!("could not parse '{t}'"));
                            errors.push(format!("invalid line {}: {line}", n + 1));
                            continue;
                        }
                    },
                };
                if lx.op.is_fixup() {
                    if check_fixups && !seen_nonfixup {
                        errors.push(format!(
                            "cannot '{}' without a previous commit",
                            lx.op.word()
                        ));
                        continue;
                    }
                } else if !matches!(lx.op, Op::Noop | Op::Drop) {
                    seen_nonfixup = true;
                }
                items.push(Item {
                    op: lx.op,
                    keep: lx.keep,
                    oid,
                    arg: lx.arg,
                });
            }
            Err(e) => {
                match e {
                    LexError::Msg(m) => errors.push(m),
                    LexError::NoArgs { op, offset } => {
                        let tail = text.get(line_start + offset..).unwrap_or_default();
                        errors.push(format!("{} does not accept arguments: '{tail}'", op.word()));
                    }
                }
                errors.push(format!("invalid line {}: {line}", n + 1));
            }
        }
    }
    if errors.is_empty() {
        Ok(items)
    } else {
        Err(errors)
    }
}

/// git's `stripspace --strip-comments`: comment lines dropped, trailing blanks off every line,
/// runs of blank lines collapsed, leading and trailing blank lines dropped.
pub fn strip_comments(text: &str, comment: u8) -> String {
    let mut out = String::new();
    let mut pending_blank = false;
    for line in text.lines() {
        if !line.is_empty() && line.as_bytes()[0] == comment {
            continue;
        }
        let line = line.trim_end();
        if line.is_empty() {
            pending_blank = !out.is_empty();
            continue;
        }
        if pending_blank {
            out.push('\n');
            pending_blank = false;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Whether a list holds any command.
pub fn is_empty(list: &[Item]) -> bool {
    count(list) == 0
}

// ---------------------------------------------------------------- generating the list

/// One commit of the range to replay.
pub struct Commit {
    pub oid: GitOid,
    pub subject: String,
    /// The commit changes nothing against its parent.
    pub empty: bool,
}

/// The `pick` line of a commit: `pick <oid> # <subject>` with git's ` # empty` mark.
pub fn pick_item(c: &Commit, comment: u8) -> Item {
    let cc = comment as char;
    let mut arg = format!("{cc} {}", c.subject);
    if c.empty {
        arg.push_str(&format!(" {cc} empty"));
    }
    Item::new(Op::Pick, Some(c.oid), arg)
}

/// Inserts the `--exec` commands after every group of commit-writing commands (git's
/// `todo_list_add_exec_commands`).
pub fn add_exec(list: Vec<Item>, cmds: &[String]) -> Vec<Item> {
    if cmds.is_empty() {
        return list;
    }
    let mut out = Vec::with_capacity(list.len() * (cmds.len() + 1));
    let mut insert = false;
    let push_execs = |out: &mut Vec<Item>| {
        for c in cmds {
            out.push(Item::new(Op::Exec, None, c.clone()));
        }
    };
    for item in list {
        if insert && !item.op.is_fixup() {
            push_execs(&mut out);
            insert = false;
        }
        insert |= item.op.makes_commit();
        out.push(item);
    }
    if insert {
        push_execs(&mut out);
    }
    out
}

/// `fixup!`, `squash!` or `amend!` at the start of a subject: the kind and what follows.
fn fixupish(subject: &str) -> Option<&str> {
    ["fixup! ", "squash! ", "amend! "]
        .iter()
        .find_map(|p| subject.strip_prefix(p))
}

/// The commit a `fixup!`/`squash!`/`amend!` subject names, with git's three lookups: the whole
/// subject of an earlier pick, a commit name (when the text has no blank), or a prefix of an
/// earlier subject.
fn fixup_target(
    rest: &str,
    upto: usize,
    subjects: &[Option<String>],
    by_title: &HashMap<String, usize>,
    by_name: &HashMap<GitOid, usize>,
    names: &HashMap<String, GitOid>,
) -> Option<usize> {
    if let Some(&i) = by_title.get(rest) {
        return Some(i);
    }
    if !rest.contains(' ')
        && let Some(oid) = names.get(rest)
        && let Some(&i) = by_name.get(oid)
    {
        return Some(i);
    }
    (0..upto).find(|&i| subjects[i].as_deref().is_some_and(|s| s.starts_with(rest)))
}

/// The names `--autosquash` may have to resolve: the text after `fixup! ` of every subject that
/// has no blank in it.
pub fn autosquash_names(subjects: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for s in subjects {
        let mut rest = s.as_str();
        let Some(r) = fixupish(rest) else { continue };
        rest = r;
        while let Some(r) = fixupish(rest.trim_start()) {
            rest = r;
        }
        let rest = rest.trim_start();
        if !rest.is_empty() && !rest.contains(' ') {
            out.push(rest.to_owned());
        }
    }
    out
}

/// `--autosquash` (git's `todo_list_rearrange_squash`): every `fixup!`/`squash!`/`amend!` commit
/// moves behind the commit it names and becomes a `fixup`, `squash` or `fixup -C`. `subjects[i]`
/// is the subject of the commit of `list[i]` (`None` for other commands); `names` resolves the
/// names from [`autosquash_names`].
pub fn rearrange_squash(
    list: Vec<Item>,
    subjects: &[Option<String>],
    names: &HashMap<String, GitOid>,
) -> Vec<Item> {
    let n = list.len();
    let mut list = list;
    let mut next: Vec<Option<usize>> = vec![None; n];
    let mut tail: Vec<Option<usize>> = vec![None; n];
    let mut by_title: HashMap<String, usize> = HashMap::new();
    let mut by_name: HashMap<GitOid, usize> = HashMap::new();
    for i in 0..n {
        let Some(subject) = subjects[i].as_deref() else {
            continue;
        };
        if list[i].op != Op::Pick {
            continue;
        }
        if let Some(oid) = list[i].oid {
            by_name.insert(oid, i);
        }
        let mut target = None;
        if let Some(mut rest) = fixupish(subject) {
            while let Some(r) = fixupish(rest.trim_start()) {
                rest = r;
            }
            let rest = rest.trim_start();
            target = fixup_target(rest, i, subjects, &by_title, &by_name, names);
        }
        match target {
            Some(i2) => {
                if subject.starts_with("fixup!") {
                    list[i].op = Op::Fixup;
                } else if subject.starts_with("amend!") {
                    list[i].op = Op::Fixup;
                    list[i].keep = Keep::Message;
                } else {
                    list[i].op = Op::Squash;
                }
                match tail[i2] {
                    None => {
                        next[i] = next[i2];
                        next[i2] = Some(i);
                    }
                    Some(t) => {
                        next[i] = next[t];
                        next[t] = Some(i);
                    }
                }
                tail[i2] = Some(i);
            }
            None => {
                by_title.entry(subject.to_owned()).or_insert(i);
            }
        }
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        if list[i].op.is_fixup() {
            continue;
        }
        let mut cur = Some(i);
        while let Some(c) = cur {
            out.push(list[c].clone());
            cur = next[c];
        }
    }
    out
}

/// The text the editor opens: the list with short ids, then git's help block.
pub fn editor_text(
    list: &[Item],
    short: &dyn Fn(&GitOid) -> String,
    header: &str,
    comment: u8,
) -> String {
    let mut out = render_short(list, short);
    out.push('\n');
    out.push_str(&help_block(header, comment));
    out
}

/// The `# Rebase a..b onto c (N commands)` line and the command help.
pub fn help_block(header: &str, comment: u8) -> String {
    let mut out = format!("{header}\n#\n{TODO_HELP}");
    if comment != b'#' {
        let c = (comment as char).to_string();
        out = out
            .lines()
            .map(|l| {
                if let Some(r) = l.strip_prefix('#') {
                    format!("{c}{r}\n")
                } else {
                    format!("{l}\n")
                }
            })
            .collect();
    }
    out
}

/// `# Rebase <a>..<b> onto <c> (<n> command(s))`.
pub fn rebase_header(
    upstream: &str,
    orig: &str,
    onto: &str,
    commands: usize,
    comment: u8,
) -> String {
    format!(
        "{} Rebase {upstream}..{orig} onto {onto} ({commands} command{})",
        comment as char,
        if commands == 1 { "" } else { "s" },
    )
}

/// git's help block under the todo list.
pub const TODO_HELP: &str = "# Commands:
# p, pick <commit> = use commit
# r, reword <commit> = use commit, but edit the commit message
# e, edit <commit> = use commit, but stop for amending
# s, squash <commit> = use commit, but meld into previous commit
# f, fixup [-C | -c] <commit> = like \"squash\" but keep only the previous
#                    commit's log message, unless -C is used, in which case
#                    keep only this commit's message; -c is same as -C but
#                    opens the editor
# x, exec <command> = run command (the rest of the line) using shell
# b, break = stop here (continue rebase later with 'git rebase --continue')
# d, drop <commit> = remove commit
# l, label <label> = label current HEAD with a name
# t, reset <label> = reset HEAD to a label
# m, merge [-C <commit> | -c <commit>] <label> [# <oneline>]
#         create a merge commit using the original merge commit's
#         message (or the oneline, if no original merge commit was
#         specified); use -c <commit> to reword the commit message
# u, update-ref <ref> = track a placeholder for the <ref> to be updated
#                       to this position in the new commits. The <ref> is
#                       updated at the end of the rebase
#
# These lines can be re-ordered; they are executed from top to bottom.
#
# If you remove a line here THAT COMMIT WILL BE LOST.
#
# However, if you remove everything, the rebase will be aborted.
#
";

// ---------------------------------------------------------------- the editor

/// The todo editor: `GIT_SEQUENCE_EDITOR`, `sequence.editor`, then the commit editor.
pub fn sequence_editor(ctx: &Ctx) -> String {
    std::env::var("GIT_SEQUENCE_EDITOR")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| ctx.config.get("sequence.editor").filter(|v| !v.is_empty()))
        .unwrap_or_else(|| message::editor(ctx))
}

/// Runs `editor` on `file` as git does (`sh -c '<editor> "$@"' <editor> <file>`, spawned through
/// `asc::child::command()`). `Ok(false)` means the editor failed, after git's message for it.
pub async fn run_editor(ctx: &Ctx, editor: &str, file: &Path) -> Result<bool> {
    if editor == ":" {
        return Ok(true);
    }
    let mut cmd = ctx.child(ctx.shell())?;
    cmd.arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg(editor)
        .arg(file);
    let status = tokio::task::spawn_blocking(move || cmd.status())
        .await
        .map_err(|e| CliError::Fatal(e.to_string()))??;
    if !status.success() {
        eprintln!("error: there was a problem with the editor '{editor}'");
    }
    Ok(status.success())
}

/// Runs the todo editor on `file`.
pub async fn edit_todo(ctx: &Ctx, file: &Path) -> Result<bool> {
    run_editor(ctx, &sequence_editor(ctx), file).await
}

/// The text `rebase --edit-todo` opens: the commands left (`body`, short ids), and the help block
/// ending with the reminder of how to continue.
pub fn edit_todo_text(body: &str, tool: &str, comment: u8) -> String {
    let c = comment as char;
    let mut out = with_newline(body);
    out.push_str(&format!("{c}\n"));
    let help = TODO_HELP.replace(
        "# However, if you remove everything, the rebase will be aborted.\n#\n",
        &format!(
            "# You are editing the todo file of an ongoing interactive rebase.\n# To continue rebase after editing, run:\n#     {tool} rebase --continue\n#\n"
        ),
    );
    for line in help.split_inclusive('\n') {
        match line.strip_prefix('#') {
            Some(rest) if comment != b'#' => {
                out.push(c);
                out.push_str(rest);
            }
            _ => out.push_str(line),
        }
    }
    out
}

/// `git-rebase-todo.backup`: the generated list with full ids and the help block.
pub fn backup_text(list: &[Item], header: &str, comment: u8) -> String {
    format!("{}\n{}", render_full(list), help_block(header, comment))
}

/// The `update-ref` lines (git's `todo_list_add_update_ref_commands`): behind a commit that a
/// branch points at, once the commit and the `exec` lines after it are done. `refs_at` maps a
/// commit to the branch names (full ref names) on it; the returned pairs are the refs the list
/// now tracks with the commit they pointed at.
pub fn add_update_refs(
    list: Vec<Item>,
    refs_at: &HashMap<GitOid, Vec<String>>,
) -> (Vec<Item>, Vec<(String, GitOid)>) {
    let mut out = Vec::with_capacity(list.len());
    let mut tracked = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let flush = |out: &mut Vec<Item>, pending: &mut Vec<String>| {
        for r in pending.drain(..) {
            out.push(Item::new(Op::UpdateRef, None, r));
            out.push(Item::blank());
        }
    };
    for item in list {
        if item.oid.is_some() && item.op.has_commit() && !item.op.is_fixup() && item.op != Op::Drop
        {
            flush(&mut out, &mut pending);
        }
        if let (Some(oid), true) = (item.oid, item.op.has_commit() && item.op != Op::Drop) {
            for r in refs_at.get(&oid).into_iter().flatten() {
                pending.push(r.clone());
                tracked.push((r.clone(), oid));
            }
        }
        out.push(item);
    }
    flush(&mut out, &mut pending);
    (out, tracked)
}

// ---------------------------------------------------------------- squash messages

/// Which kind of message a folded commit adds.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Fold {
    Squash,
    Fixup,
}

fn with_newline(body: &str) -> String {
    if body.is_empty() || body.ends_with('\n') {
        body.to_owned()
    } else {
        format!("{body}\n")
    }
}

/// git's `strbuf_add_commented_lines`.
pub fn commented(body: &str, c: char) -> String {
    let mut out = String::new();
    for line in with_newline(body).split_inclusive('\n') {
        out.push(c);
        if !matches!(line.as_bytes().first(), Some(b'\n' | b'\t')) {
            out.push(' ');
        }
        out.push_str(line);
    }
    out
}

fn ordinal_header(n: usize, c: char) -> String {
    format!("{c} This is a combination of {n} commits.")
}

/// The squash message when the first commit gets folded: the header, the first message, and the
/// folded one.
pub fn start_squash(head_body: &str, folded: &str, fold: Fold, c: char) -> String {
    let mut out = format!(
        "{}\n{c} This is the 1st commit message:\n\n{}",
        ordinal_header(2, c),
        with_newline(head_body)
    );
    append_squash(&mut out, 2, folded, fold, c);
    out
}

/// Adds the message of the `n`-th commit to a squash message.
pub fn append_squash(buf: &mut String, n: usize, body: &str, fold: Fold, c: char) {
    buf.push_str(&format!("\n{c} "));
    match fold {
        Fold::Squash => {
            buf.push_str(&format!("This is the commit message #{n}:\n\n"));
            buf.push_str(&with_newline(body));
        }
        Fold::Fixup => {
            buf.push_str(&format!("The commit message #{n} will be skipped:\n\n"));
            buf.push_str(&commented(body, c));
        }
    }
}

/// The squash message with its first line replaced by the header for `n` commits.
pub fn renumber(buf: &str, n: usize, c: char) -> String {
    let rest = if buf.starts_with(c) {
        buf.find('\n').map_or("", |i| &buf[i..])
    } else {
        buf
    };
    format!("{}{rest}", ordinal_header(n, c))
}

/// The lines of `current-fixups`: `squash <oid>` / `fixup <oid>` / `fixup -C <oid>`.
pub fn fixup_line(op: Op, keep: Keep, oid: &GitOid) -> String {
    if keep == Keep::No {
        format!("{} {oid}", op.word())
    } else {
        format!("{} {} {oid}", op.word(), keep.flag())
    }
}

/// Whether a `current-fixups` text has a `squash` line.
pub fn seen_squash(fixups: &str) -> bool {
    fixups.lines().any(|l| l.starts_with("squash"))
}

/// The message of a `squash!` commit without its `squash! <subject>` line: that line only names
/// the commit it folds into.
pub fn without_squash_subject(message: &str) -> &str {
    if message.starts_with("squash!") {
        let rest = message.find('\n').map_or("", |i| &message[i + 1..]);
        rest.trim_start_matches('\n')
    } else {
        message
    }
}

/// The message of an `amend!` commit without its subject line, as `fixup -C` uses it.
pub fn amend_body(message: &str) -> &str {
    if message.starts_with("amend!") {
        match message.find("\n\n") {
            Some(i) => message[i + 2..].trim_start_matches('\n'),
            None => "",
        }
    } else {
        message
    }
}

// ---------------------------------------------------------------- rebase state for status

/// What `git status` and the commit template show about a rebase in progress.
pub struct RebaseInfo {
    pub interactive: bool,
    pub branch: Option<String>,
    pub onto: Option<String>,
    /// The abbreviated lines of `done` and of the todo, comments and blanks left out.
    pub done: Vec<String>,
    pub todo: Vec<String>,
    pub merge_msg: bool,
    pub gitdir: std::path::PathBuf,
}

fn abbreviate_line(line: &str, abbrev: usize) -> String {
    let mut parts = line.splitn(3, ' ');
    let (Some(cmd), Some(id)) = (parts.next(), parts.next()) else {
        return line.to_owned();
    };
    if [
        "exec",
        "x",
        "label",
        "l",
        "reset",
        "t",
        "merge",
        "m",
        "update-ref",
        "u",
    ]
    .contains(&cmd)
    {
        return line.to_owned();
    }
    if id.len() == 40 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
        let rest = parts.next().map(|r| format!(" {r}")).unwrap_or_default();
        return format!("{cmd} {}{rest}", &id[..abbrev.min(40)]);
    }
    line.to_owned()
}

fn status_lines(dir: &Path, name: &str, abbrev: usize) -> Vec<String> {
    let Ok(Some(bytes)) = seqstate::read(dir, name) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&bytes)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .take(MAX_TODO)
        .map(|l| abbreviate_line(l, abbrev))
        .collect()
}

/// Reads the state of a rebase in progress; `None` when there is none.
pub fn read_info(gitdir: &Path, abbrev: usize) -> Option<RebaseInfo> {
    let dir = gitdir.join("rebase-merge");
    if !dir.is_dir() {
        return None;
    }
    let text = |name: &str| {
        seqstate::read(&dir, name)
            .ok()
            .flatten()
            .map(|b| String::from_utf8_lossy(&b).trim().to_owned())
    };
    let interactive = seqstate::exists(&dir, "interactive");
    let branch = text("head-name").map(|b| b.strip_prefix("refs/heads/").unwrap_or(&b).to_owned());
    let onto = text("onto").map(|o| o.chars().take(abbrev).collect());
    Some(RebaseInfo {
        interactive,
        branch,
        onto,
        done: if interactive {
            status_lines(&dir, "done", abbrev)
        } else {
            Vec::new()
        },
        todo: if interactive {
            status_lines(&dir, "git-rebase-todo", abbrev)
        } else {
            Vec::new()
        },
        merge_msg: gitdir.join("MERGE_MSG").exists(),
        gitdir: gitdir.to_owned(),
    })
}

impl RebaseInfo {
    /// The branch line `git status` prints instead of "HEAD detached".
    pub fn head_line(&self) -> String {
        format!(
            "{} rebase in progress; onto {}",
            if self.interactive { "interactive" } else { "" },
            self.onto.as_deref().unwrap_or("")
        )
        .trim_start()
        .to_owned()
    }

    /// The lines under the branch line: what is done and left, then the state sentence. `tool` is
    /// the program name the hints use.
    pub fn block(&self, unmerged: bool, hints: bool, tool: &str) -> String {
        let mut out = String::new();
        let hint = |out: &mut String, t: &str| {
            if hints {
                out.push_str(&format!("  ({t})\n"));
            }
        };
        if self.interactive {
            const SHOWN: usize = 2;
            if self.done.is_empty() {
                out.push_str("No commands done.\n");
            } else {
                let n = self.done.len();
                if n == 1 {
                    out.push_str("Last command done (1 command done):\n");
                } else {
                    out.push_str(&format!("Last commands done ({n} commands done):\n"));
                }
                for l in &self.done[n.saturating_sub(SHOWN)..] {
                    out.push_str(&format!("   {l}\n"));
                }
                if n > SHOWN {
                    hint(
                        &mut out,
                        &format!(
                            "see more in file {}",
                            self.gitdir
                                .join("rebase-merge")
                                .join("done")
                                .display()
                                .to_string()
                                .replace('\\', "/")
                        ),
                    );
                }
            }
            if self.todo.is_empty() {
                out.push_str("No commands remaining.\n");
            } else {
                let n = self.todo.len();
                if n == 1 {
                    out.push_str("Next command to do (1 remaining command):\n");
                } else {
                    out.push_str(&format!("Next commands to do ({n} remaining commands):\n"));
                }
                for l in self.todo.iter().take(SHOWN) {
                    out.push_str(&format!("   {l}\n"));
                }
                hint(
                    &mut out,
                    &format!("use \"{tool} rebase --edit-todo\" to view and edit"),
                );
            }
        }
        let on = match (&self.branch, &self.onto) {
            (Some(b), Some(o)) if b != "detached HEAD" => Some((b.as_str(), o.as_str())),
            _ => None,
        };
        let sentence = |verb: &str| match on {
            Some((b, o)) => format!("You are currently {verb} branch '{b}' on '{o}'.\n"),
            None => "You are currently rebasing.\n".to_owned(),
        };
        if unmerged {
            out.push_str(&sentence("rebasing"));
            hint(
                &mut out,
                &format!("fix conflicts and then run \"{tool} rebase --continue\""),
            );
            hint(
                &mut out,
                &format!("use \"{tool} rebase --skip\" to skip this patch"),
            );
            hint(
                &mut out,
                &format!("use \"{tool} rebase --abort\" to check out the original branch"),
            );
        } else if !self.interactive || self.merge_msg {
            out.push_str(&sentence("rebasing"));
            hint(
                &mut out,
                &format!("all conflicts fixed: run \"{tool} rebase --continue\""),
            );
        } else {
            match on {
                Some((b, o)) => out.push_str(&format!(
                    "You are currently editing a commit while rebasing branch '{b}' on '{o}'.\n"
                )),
                None => out.push_str("You are currently editing a commit while rebasing.\n"),
            }
            hint(
                &mut out,
                &format!("use \"{tool} commit --amend\" to amend the current commit"),
            );
            hint(
                &mut out,
                &format!(
                    "use \"{tool} rebase --continue\" once you are satisfied with your changes"
                ),
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(c: char) -> GitOid {
        c.to_string().repeat(40).parse().unwrap()
    }

    fn known(pairs: &[(&str, GitOid)]) -> HashMap<String, GitOid> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()
    }

    #[test]
    fn every_command_round_trips_through_the_text() {
        let a = oid('a');
        let text = format!(
            "pick {a} # one\nreword {a}\nedit {a} x\nsquash {a}\nfixup {a}\nfixup -C {a} # c\nfixup -c {a}\ndrop {a}\nexec make test\nbreak\n\nlabel onto\nreset onto\nmerge -C {a} side # m\nmerge side\nupdate-ref refs/heads/x\nnoop\n# note\n"
        );
        let map = known(&[(a.to_string().as_str(), a)]);
        let items = parse(&text, b'#', &map, true).unwrap();
        assert_eq!(render_full(&items), text);
        assert_eq!(count(&items), 16);
    }

    #[test]
    fn single_letters_and_abbreviations_are_the_same_commands() {
        let map = known(&[("abc", oid('a'))]);
        let items = parse(
            "p abc\nr abc\ne abc\ns abc\nf abc\nd abc\nx echo\nb\nl x\nt x\nm x\nu refs/heads/y\n",
            b'#',
            &map,
            true,
        )
        .unwrap();
        let ops: Vec<Op> = items.iter().map(|i| i.op).collect();
        assert_eq!(
            ops,
            [
                Op::Pick,
                Op::Reword,
                Op::Edit,
                Op::Squash,
                Op::Fixup,
                Op::Drop,
                Op::Exec,
                Op::Break,
                Op::Label,
                Op::Reset,
                Op::Merge,
                Op::UpdateRef
            ]
        );
        assert_eq!(items[0].oid, Some(oid('a')));
    }

    #[test]
    fn errors_use_git_s_wording_and_line_numbers() {
        let map = known(&[]);
        let e = parse(
            "bogus x\npick\npick zzz # s\nbreak now\nexec \n",
            b'#',
            &map,
            true,
        )
        .unwrap_err();
        assert_eq!(e[0], "invalid command 'bogus'");
        assert_eq!(e[1], "invalid line 1: bogus x");
        assert_eq!(e[2], "missing arguments for pick");
        assert_eq!(e[3], "invalid line 2: pick");
        assert_eq!(e[4], "could not parse 'zzz'");
        assert_eq!(e[5], "invalid line 3: pick zzz # s");
        assert!(e[6].starts_with("break does not accept arguments: 'now\nexec \n'"));
        assert_eq!(e[7], "invalid line 4: break now");
    }

    #[test]
    fn the_tokens_are_the_commit_names_in_order() {
        let t = tokens(
            "pick abc # x\n# c\nexec y\nmerge -C def side\nfixup -C ghi\n",
            b'#',
        );
        assert_eq!(t, ["abc", "def", "ghi"]);
    }

    #[test]
    fn stripping_comments_collapses_blank_lines() {
        assert_eq!(
            strip_comments("\n# c\npick a\n\n\n# d\n\nexec b  \n\n# e\n", b'#'),
            "pick a\n\nexec b\n"
        );
    }

    #[test]
    fn exec_follows_each_group_of_commits() {
        let p = |c| Item::new(Op::Pick, Some(oid(c)), "");
        let list = vec![
            p('a'),
            Item::new(Op::Fixup, Some(oid('b')), ""),
            p('c'),
            Item::new(Op::Edit, Some(oid('d')), ""),
        ];
        let out = add_exec(list, &["t".to_owned()]);
        let ops: Vec<Op> = out.iter().map(|i| i.op).collect();
        assert_eq!(
            ops,
            [
                Op::Pick,
                Op::Fixup,
                Op::Exec,
                Op::Pick,
                Op::Exec,
                Op::Edit,
                Op::Exec
            ]
        );
    }

    #[test]
    fn autosquash_moves_fixups_behind_their_targets() {
        let p = |c: char| Item::new(Op::Pick, Some(oid(c)), "");
        let list = vec![p('a'), p('b'), p('c'), p('d'), p('e'), p('f')];
        let subjects: Vec<Option<String>> = [
            "one",
            "two",
            "fixup! one",
            "squash! two extra",
            "amend! one",
            "fixup! aaaa",
        ]
        .iter()
        .map(|s| Some((*s).to_owned()))
        .collect();
        let mut names = HashMap::new();
        names.insert("aaaa".to_owned(), oid('a'));
        let out = rearrange_squash(list, &subjects, &names);
        let shape: Vec<(char, Op, Keep)> = out
            .iter()
            .map(|i| {
                (
                    i.oid.unwrap().to_string().chars().next().unwrap(),
                    i.op,
                    i.keep,
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                ('a', Op::Pick, Keep::No),
                ('c', Op::Fixup, Keep::No),
                ('e', Op::Fixup, Keep::Message),
                ('f', Op::Fixup, Keep::No),
                ('b', Op::Pick, Keep::No),
                // "two extra" is not a title or a prefix of "two": the squash stays in place.
                ('d', Op::Pick, Keep::No),
            ]
        );
    }

    #[test]
    fn a_fixup_may_name_its_target_by_a_prefix_of_the_subject() {
        let p = |c: char| Item::new(Op::Pick, Some(oid(c)), "");
        let subjects = vec![
            Some("add the parser".to_owned()),
            Some("other".to_owned()),
            Some("fixup! add the".to_owned()),
        ];
        let out = rearrange_squash(vec![p('a'), p('b'), p('c')], &subjects, &HashMap::new());
        assert_eq!(
            out.iter().map(|i| i.op).collect::<Vec<_>>(),
            [Op::Pick, Op::Fixup, Op::Pick]
        );
    }

    #[test]
    fn the_squash_message_grows_like_git_s() {
        let mut m = start_squash("m1\n", "m2\n", Fold::Squash, '#');
        assert_eq!(
            m,
            "# This is a combination of 2 commits.\n# This is the 1st commit message:\n\nm1\n\n# This is the commit message #2:\n\nm2\n"
        );
        m = renumber(&m, 3, '#');
        append_squash(&mut m, 3, "m3\n\nbody\n", Fold::Fixup, '#');
        assert!(m.starts_with("# This is a combination of 3 commits.\n"));
        assert!(m.ends_with("\n# The commit message #3 will be skipped:\n\n# m3\n#\n# body\n"));
    }

    #[test]
    fn a_squash_commit_loses_its_squash_line() {
        assert_eq!(without_squash_subject("squash! m2\n"), "");
        assert_eq!(without_squash_subject("squash! m2\n\nmore\n"), "more\n");
        assert_eq!(without_squash_subject("plain\n"), "plain\n");
    }

    #[test]
    fn an_amend_commit_loses_its_subject() {
        assert_eq!(amend_body("amend! x\n\nnew text\n"), "new text\n");
        assert_eq!(amend_body("plain\n"), "plain\n");
    }

    #[test]
    fn the_status_block_names_what_is_done_and_what_is_left() {
        let info = RebaseInfo {
            interactive: true,
            branch: Some("side".into()),
            onto: Some("abc1234".into()),
            done: vec![
                "pick 1111111 # a".into(),
                "pick 2222222 # b".into(),
                "edit 3333333 # c".into(),
            ],
            todo: vec!["pick 4444444 # d".into()],
            merge_msg: false,
            gitdir: "/g".into(),
        };
        let text = info.block(false, true, "git");
        assert!(text.starts_with("Last commands done (3 commands done):\n   pick 2222222 # b\n   edit 3333333 # c\n  (see more in file /g/rebase-merge/done)\n"));
        assert!(text.contains("Next command to do (1 remaining command):\n   pick 4444444 # d\n  (use \"git rebase --edit-todo\" to view and edit)\n"));
        assert!(text.ends_with("You are currently editing a commit while rebasing branch 'side' on 'abc1234'.\n  (use \"git commit --amend\" to amend the current commit)\n  (use \"git rebase --continue\" once you are satisfied with your changes)\n"));
        assert_eq!(
            info.head_line(),
            "interactive rebase in progress; onto abc1234"
        );
    }
}
