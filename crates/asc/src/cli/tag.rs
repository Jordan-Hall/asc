//! `tag` (SPEC §11.2): lightweight and annotated tags, `-s`/`-u` signing through git's signing
//! program, `-d`, `-l` with patterns and filters, and `-n` annotation lines. An annotated tag is
//! a `TagRecord` stored as `ObjKey::Tag` with its git id in the git map; the ref holds the tag.
//! Every ref change goes through `Ctx::update_local_refs`.

use std::ffi::OsString;
use std::io::{Read, Write as _};
use std::process::{ExitCode, Stdio};

use asc_core::codec::Enveloped as _;
use asc_core::git::{EXTRA_GIT_GPGSIG, ObjectKind, export_tag, object_oid, render_tag};
use asc_core::id::{ChangesetId, GitOid, RefTarget, TagId};
use asc_core::object::{SortedMap, TagRecord};
use asc_core::path::RefName;
use asc_core::sig::Signature;
use asc_engine::derive::GitMapRow;
use asc_state::state::git_map;
use asc_store::codec::encode_stored;
use asc_store::key::ObjKey;
use asc_store::objects::Objects as _;
use asc_store::refs::{Expect, RefTxn, RefUpdate};

use super::commit::message::{self, Cleanup};
use super::common::{CliError, Ctx, Result, StagingAfterMove, WsCtx};
use super::output::Output;
use super::pager::PagerWant;
use super::ref_filter::{self, Filters, Format, Query, Quote, Request, SortKey};
use super::refs_plumbing::args::{Cursor, Word, object_oid_of, resolve_oid, resolve_target};
use super::restore::actor;
use super::switch::{abbrev_len, ensure_mapped, fatal, peel, ref_name, ref_target};
use crate::ext;
use crate::native::NativeSpec;

#[derive(clap::Parser, Debug)]
#[command(name = "tag", disable_help_flag = true, disable_version_flag = true)]
pub struct Args {
    /// Every word before `--`, options included: the command reads its own flags.
    #[arg(allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
    /// The words after `--`.
    #[arg(last = true)]
    pub after: Vec<OsString>,
}

impl Args {
    /// The words as typed, `--` restored (clap consumes it).
    pub(crate) fn argv(&self) -> Vec<OsString> {
        let mut all = self.rest.clone();
        if !self.after.is_empty() {
            all.push(OsString::from("--"));
            all.extend(self.after.iter().cloned());
        }
        all
    }
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "tag",
    flags: &[
        "-a", "--annotate", "-s", "--sign", "-u", "--local-user", "-f", "--force", "-d", "--delete", "-n", "-l",
        "--list", "--sort", "--format", "--contains", "--no-contains", "--merged", "--no-merged", "--points-at",
        "-i", "--ignore-case", "-m", "--message", "-F", "--file", "-e", "--edit", "--cleanup", "--color",
        "--no-color",
    ],
    placeholders: ref_filter::ATOMS,
}];

/// `git tag -h` of git 2.55, as printed after a usage error.
const USAGE: &str = r#"usage: git tag [-a | -s | -u <key-id>] [-f] [-m <msg> | -F <file>] [-e]
               [(--trailer <token>[(=|:)<value>])...]
               <tagname> [<commit> | <object>]
   or: git tag -d <tagname>...
   or: git tag [-n[<num>]] -l [--contains <commit>] [--no-contains <commit>]
               [--points-at <object>] [--column[=<options>] | --no-column]
               [--create-reflog] [--sort=<key>] [--format=<format>]
               [--merged <commit>] [--no-merged <commit>] [<pattern>...]
   or: git tag -v [--format=<format>] <tagname>...

    -l, --list            list tag names
    -n[<n>]               print <n> lines of each tag message
    -d, --delete          delete tags
    -v, --verify          verify tags

Tag creation options
    -a, --[no-]annotate   annotated tag, needs a message
    -m, --message <message>
                          tag message
    -F, --[no-]file <file>
                          read message from file
    --[no-]trailer <trailer>
                          add custom trailer(s)
    -e, --[no-]edit       force edit of tag message
    -s, --[no-]sign       annotated and GPG-signed tag
    --[no-]cleanup <mode> how to strip spaces and #comments from message
    -u, --[no-]local-user <key-id>
                          use another key to sign the tag
    -f, --[no-]force      replace the tag if exists
    --[no-]create-reflog  create a reflog

Tag listing options
    --[no-]column[=<style>]
                          show tag list in columns
    --contains <commit>   print only tags that contain the commit
    --no-contains <commit>
                          print only tags that don't contain the commit
    --merged <commit>     print only tags that are merged
    --no-merged <commit>  print only tags that are not merged
    --[no-]omit-empty     do not output a newline after empty formatted refs
    --[no-]sort <key>     field name to sort on
    --[no-]points-at <object>
                          print only tags of the object
    --[no-]format <format>
                          format to use for the output
    --[no-]color[=<when>] respect format colors
    -i, --[no-]ignore-case
                          sorting and filtering are case insensitive
"#;

/// Lines of the tag message `tag -n` shows when the number is omitted.
const DEFAULT_LINES: usize = 1;

#[derive(Default)]
struct Opts {
    annotate: bool,
    sign: bool,
    key: Option<String>,
    force: bool,
    delete: bool,
    list: bool,
    lines: Option<usize>,
    sorts: Vec<String>,
    format: Option<String>,
    contains: Vec<String>,
    no_contains: Vec<String>,
    merged: Vec<String>,
    no_merged: Vec<String>,
    points_at: Vec<String>,
    ignore_case: bool,
    messages: Vec<String>,
    files: Vec<String>,
    edit: bool,
    cleanup: Option<String>,
    color: Option<bool>,
    words: Vec<String>,
}

fn parse_color(value: &str) -> Result<Option<bool>> {
    Ok(match value {
        "always" | "true" | "yes" | "on" | "1" | "" => Some(true),
        "never" | "false" | "no" | "off" | "0" => Some(false),
        "auto" => None,
        _ => return Err(CliError::Fatal(format!("bad boolean config value '{value}' for 'color'"))),
    })
}

fn parse(args: &[OsString]) -> Result<Opts> {
    let mut cur = Cursor::new(args, USAGE);
    let mut o = Opts::default();
    while let Some(word) = cur.next() {
        match word {
            Word::End => o.words.extend(cur.rest()),
            Word::Operand(w) => o.words.push(w),
            Word::Short(flags) => {
                let mut chars = flags.chars();
                while let Some(c) = chars.next() {
                    let take = |name: &str, chars: &mut std::str::Chars<'_>, cur: &mut Cursor| {
                        let rest: String = chars.by_ref().collect();
                        if rest.is_empty() { cur.value(name, None) } else { Ok(rest) }
                    };
                    match c {
                        'a' => o.annotate = true,
                        's' => o.sign = true,
                        'f' => o.force = true,
                        'd' => o.delete = true,
                        'l' => o.list = true,
                        'i' => o.ignore_case = true,
                        'e' => o.edit = true,
                        'u' => {
                            o.key = Some(take("u", &mut chars, &mut cur)?);
                            o.sign = true;
                        }
                        'm' => o.messages.push(take("m", &mut chars, &mut cur)?),
                        'F' => o.files.push(take("F", &mut chars, &mut cur)?),
                        'n' => {
                            let rest: String = chars.by_ref().collect();
                            o.lines = Some(if rest.is_empty() {
                                DEFAULT_LINES
                            } else {
                                rest.parse().map_err(|_| cur.usage_error("switch `n' expects a numerical value"))?
                            });
                        }
                        other => return Err(cur.usage_error(&format!("unknown switch `{other}'"))),
                    }
                }
            }
            Word::Long(name, inline) => match name.as_str() {
                "annotate" => o.annotate = true,
                "sign" => o.sign = true,
                "local-user" => {
                    o.key = Some(cur.value("local-user", inline)?);
                    o.sign = true;
                }
                "force" => o.force = true,
                "delete" => o.delete = true,
                "list" => o.list = true,
                "sort" => o.sorts.push(cur.value("sort", inline)?),
                "format" => o.format = Some(cur.value("format", inline)?),
                "contains" => o.contains.push(cur.value_or(inline, "HEAD")),
                "no-contains" => o.no_contains.push(cur.value_or(inline, "HEAD")),
                "merged" => o.merged.push(cur.value_or(inline, "HEAD")),
                "no-merged" => o.no_merged.push(cur.value_or(inline, "HEAD")),
                "points-at" => o.points_at.push(cur.value_or(inline, "HEAD")),
                "ignore-case" => o.ignore_case = true,
                "message" => o.messages.push(cur.value("message", inline)?),
                "file" => o.files.push(cur.value("file", inline)?),
                "edit" => o.edit = true,
                "cleanup" => o.cleanup = Some(cur.value("cleanup", inline)?),
                "color" => o.color = parse_color(inline.as_deref().unwrap_or("always"))?,
                "no-color" => o.color = Some(false),
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
        }
    }
    Ok(o)
}

async fn commits(ctx: &Ctx, ws: &WsCtx, words: &[String], fatal_exit: bool) -> Result<Vec<ChangesetId>> {
    let mut out = Vec::new();
    for w in words {
        let found = match resolve_target(ctx, ws, w).await? {
            Some(t) => Some(peel(ws, t).await?),
            None => None,
        };
        out.push(found.ok_or_else(|| {
            if fatal_exit {
                CliError::Fatal(format!("malformed object name {w}"))
            } else {
                CliError::Raw(format!("error: malformed object name {w}"), 129)
            }
        })?);
    }
    Ok(out)
}

// ------------------------------------------------------------------------------------ listing

async fn list(ctx: &Ctx, ws: &WsCtx, o: &Opts) -> Result<ExitCode> {
    if o.format.is_some() && o.lines.is_some() {
        return Err(CliError::Fatal("options '--format' and '-n' cannot be used together".into()));
    }
    let patterns: Vec<String> = if o.words.is_empty() {
        vec!["refs/tags/*".to_owned()]
    } else {
        o.words.iter().map(|p| format!("refs/tags/{p}")).collect()
    };
    let text = match (&o.format, o.lines) {
        (Some(f), _) => f.clone(),
        (None, Some(n)) => format!("%(align:15)%(refname:lstrip=2)%(end) %(contents:lines={n})"),
        (None, None) => "%(refname:lstrip=2)".to_owned(),
    };
    let format = Format::parse(text.as_bytes(), Quote::None).map_err(ref_filter::to_cli)?;
    let sorts: Vec<String> = if o.sorts.is_empty() { ctx.config.get_all("tag.sort") } else { o.sorts.clone() };
    let sorts = sorts
        .iter()
        .map(|s| ref_filter::parse_sort_key(s).map_err(ref_filter::to_cli))
        .collect::<Result<Vec<SortKey>>>()?;
    let mut points_at = Vec::new();
    for w in &o.points_at {
        points_at.push(
            resolve_oid(ctx, ws, w)
                .await?
                .ok_or_else(|| CliError::Raw(format!("error: malformed object name '{w}'"), 129))?,
        );
    }
    let filters = Filters {
        points_at,
        merged: commits(ctx, ws, &o.merged, true).await?,
        no_merged: commits(ctx, ws, &o.no_merged, true).await?,
        contains: commits(ctx, ws, &o.contains, false).await?,
        no_contains: commits(ctx, ws, &o.no_contains, false).await?,
    };
    let mut out = Output::open(ctx, "tag", PagerWant::Auto)?;
    let color = o.color.unwrap_or_else(|| out.color());
    let req = Request {
        query: Query {
            patterns,
            excludes: Vec::new(),
            ignore_case: o.ignore_case,
            as_path: false,
            include_root: false,
            filters,
        },
        format: &format,
        sorts,
        color,
    };
    let (items, env) = ref_filter::list(ctx, ws, &req).await?;
    let result = (|| -> Result<()> {
        for item in &items {
            let line = format.render(item, &env).map_err(ref_filter::to_cli)?;
            out.write_all(&line)?;
            out.write_all(b"\n")?;
        }
        Ok(())
    })();
    match result {
        Err(CliError::Io(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        other => other?,
    }
    out.finish()?;
    Ok(ExitCode::SUCCESS)
}

// ------------------------------------------------------------------------------------ delete

fn txn(updates: Vec<RefUpdate>) -> RefTxn {
    RefTxn { updates, actor: actor(), reason: String::new(), op_id: ext::OpId::new().0 }
}

async fn delete(ctx: &Ctx, ws: &WsCtx, o: &Opts) -> Result<ExitCode> {
    let mut status = 0u8;
    let mut found = Vec::new();
    for name in &o.words {
        match ref_target(ws, &format!("refs/tags/{name}")).await? {
            Some(t) => found.push((name, t)),
            None => {
                eprintln!("error: tag '{name}' not found.");
                status = 1;
            }
        }
    }
    for (name, target) in found {
        let shown = object_oid_of(ws, target).await?.to_string();
        let u = RefUpdate {
            name: ref_name(&format!("refs/tags/{name}"))?,
            expect: Expect::Value(target),
            new: None,
            expires_at: None,
        };
        ctx.update_local_refs(txn(vec![u]), "tag: Deleted", StagingAfterMove::Keep).await?;
        println!("Deleted tag '{name}' (was {})", &shown[..abbrev_len(ctx)]);
    }
    Ok(ExitCode::from(status))
}

// ------------------------------------------------------------------------------------ create

const UNKNOWN: &str = "Committer identity unknown\n\n*** Please tell me who you are.\n\nRun\n\n  git config --global user.email \"you@example.com\"\n  git config --global user.name \"Your Name\"\n\nto set your account's default identity.\nOmit --global to set the identity only in this repository.\n\nfatal: unable to auto-detect email address";

/// The tagger: the committer identity, as git reads it.
fn tagger(ctx: &Ctx) -> Result<Signature> {
    let env = &ctx.env.committer;
    let name = env
        .name
        .clone()
        .or_else(|| ctx.config.get("committer.name"))
        .or_else(|| ctx.config.get("user.name"));
    let email = env
        .email
        .clone()
        .or_else(|| ctx.config.get("committer.email"))
        .or_else(|| ctx.config.get("user.email"))
        .or_else(|| std::env::var("EMAIL").ok().filter(|e| !e.is_empty()));
    let Some(email) = email.filter(|e| !e.is_empty()) else {
        return Err(CliError::Raw(UNKNOWN.to_owned(), 128));
    };
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        return Err(CliError::Fatal(format!("empty ident name (for <{email}>) not allowed")));
    };
    let time = match env.date.as_deref() {
        Some(d) => gix_date::parse(d, None).map_err(|_| CliError::Fatal(format!("invalid date format: {d}")))?,
        None => gix_date::Time::now_local_or_utc(),
    };
    let tz = i16::try_from(time.offset / 60).unwrap_or(0);
    Signature::new(name.into_bytes(), email.into_bytes(), time.seconds, tz).map_err(fatal)
}

/// The message from `-m`, `-F` and the editor, cleaned.
async fn message_of(ctx: &Ctx, ws: &WsCtx, o: &Opts, name: &str) -> Result<Vec<u8>> {
    let mut text: Vec<u8> = Vec::new();
    for m in &o.messages {
        if !text.is_empty() {
            text.push(b'\n');
        }
        text.extend_from_slice(m.as_bytes());
        text.push(b'\n');
    }
    for f in &o.files {
        if !text.is_empty() {
            text.push(b'\n');
        }
        let mut bytes = Vec::new();
        if f == "-" {
            std::io::stdin().read_to_end(&mut bytes)?;
        } else {
            bytes = std::fs::read(f).map_err(|e| CliError::Fatal(format!("could not open or read '{f}': {e}")))?;
        }
        text.extend_from_slice(&bytes);
        if !text.ends_with(b"\n") {
            text.push(b'\n');
        }
    }
    let given = !o.messages.is_empty() || !o.files.is_empty();
    let comment = message::comment_char(ctx);
    let mode = match &o.cleanup {
        Some(c) => Cleanup::parse(c).ok_or_else(|| CliError::Fatal(format!("Invalid cleanup mode {c}")))?,
        None => Cleanup::Default,
    };
    let edit = o.edit || !given;
    if edit {
        let file = ws.projection.git_dir().join("TAG_EDITMSG");
        let c = comment as char;
        let mut content = text.clone();
        content.extend_from_slice(
            format!("\n{c}\n{c} Write a message for tag:\n{c}   {name}\n{c} Lines starting with '{c}' will be ignored.\n")
                .as_bytes(),
        );
        std::fs::write(&file, content)?;
        if !message::edit(ctx, &file).await? {
            return Err(CliError::Fatal("There was a problem with the editor".into()));
        }
        let edited = std::fs::read(&file)?;
        let cleaned = message::cleanup(&edited, mode, true, comment);
        if cleaned.is_empty() {
            return Err(CliError::Fatal("no tag message?".into()));
        }
        return Ok(cleaned);
    }
    Ok(message::cleanup(&text, mode, false, comment))
}

/// Signs `payload` with git's signing program (`gpg.program`, default `gpg`).
async fn sign(ctx: &Ctx, payload: &[u8], key: Option<&str>, who: &str) -> Result<Vec<u8>> {
    let program = ctx.config.get("gpg.program").unwrap_or_else(|| "gpg".to_owned());
    let key = key
        .map(str::to_owned)
        .or_else(|| ctx.config.get("user.signingkey"))
        .unwrap_or_else(|| who.to_owned());
    let mut cmd = ctx.child(ctx.shell())?;
    cmd.args(["-c", "exec \"$@\"", "sh"])
        .arg(&program)
        .args(["--status-fd=2", "-bsau"])
        .arg(&key)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let payload = payload.to_vec();
    let out = tokio::task::spawn_blocking(move || -> std::io::Result<std::process::Output> {
        let mut child = cmd.spawn()?;
        if let Some(mut pipe) = child.stdin.take() {
            let _ = pipe.write_all(&payload);
        }
        child.wait_with_output()
    })
    .await
    .map_err(fatal)??;
    let mut sig = out.stdout;
    while sig.last() == Some(&b'\n') {
        sig.pop();
    }
    // The program must say it made a signature (`--status-fd`), as git insists.
    let status = String::from_utf8_lossy(&out.stderr).into_owned();
    let created = status.starts_with("[GNUPG:] SIG_CREATED ") || status.contains("\n[GNUPG:] SIG_CREATED ");
    if !out.status.success() || sig.is_empty() || !created {
        let mut text = String::from("error: gpg failed to sign the data");
        if !status.trim().is_empty() {
            text.push_str(&format!(":\n{}", status.trim_end()));
        }
        text.push_str("\nerror: unable to sign the tag");
        return Err(CliError::Raw(text, 128));
    }
    sig.push(b'\n');
    Ok(sig)
}

/// Stores an annotated tag of `target` and returns the ref value and the tag's git id.
async fn annotate(ctx: &Ctx, ws: &WsCtx, o: &Opts, name: &str, target: ChangesetId) -> Result<(RefTarget, GitOid)> {
    let message = message_of(ctx, ws, o, name).await?;
    let who = tagger(ctx)?;
    let target_git = ws.engine.git_commit_of(target).await.map_err(fatal)?;
    let mut record = TagRecord {
        target,
        name: name.as_bytes().to_vec(),
        tagger: Some(who.clone()),
        message,
        extra: SortedMap::new(Vec::new()).map_err(fatal)?,
    };
    let sign_it = o.sign || ctx.config.bool("tag.gpgSign") == Some(true);
    if sign_it {
        let payload = render_tag(&record, target_git);
        let display = format!(
            "{} <{}>",
            String::from_utf8_lossy(who.name()),
            String::from_utf8_lossy(who.email())
        );
        let sig = sign(ctx, &payload, o.key.as_deref(), &display).await?;
        record.extra = SortedMap::new(vec![(EXTRA_GIT_GPGSIG.to_vec(), sig)]).map_err(fatal)?;
    }
    let raw = export_tag(&record, target_git).map_err(fatal)?;
    let envelope = record.to_envelope();
    let id = TagId::of(&envelope);
    ws.objects.put(&ObjKey::Tag(id), encode_stored(&envelope)).await?;
    let git = object_oid(ObjectKind::Tag, &raw);
    git_map::insert_many(&ws.db, ws.repo.key, &[GitMapRow { git, kind: ObjectKind::Tag, asc: *id.as_bytes() }]).await?;
    Ok((RefTarget::Tag(id), git))
}

async fn create(ctx: &Ctx, ws: &WsCtx, o: &Opts) -> Result<ExitCode> {
    let (name, object) = match o.words.as_slice() {
        [name] => (name, "HEAD"),
        [name, object] => (name, object.as_str()),
        _ => return Err(CliError::Usage(USAGE.to_owned())),
    };
    let full = format!("refs/tags/{name}");
    if name.starts_with('-') || RefName::new(full.clone().into_bytes()).is_err() {
        return Err(CliError::Fatal(format!("'{name}' is not a valid tag name.")));
    }
    let existing = ref_target(ws, &full).await?;
    if existing.is_some() && !o.force {
        return Err(CliError::Fatal(format!("tag '{name}' already exists")));
    }
    let target = if object == "HEAD" {
        ws.head_commit().await?.map(RefTarget::Changeset)
    } else {
        resolve_target(ctx, ws, object).await?
    };
    let target = target.ok_or_else(|| CliError::Fatal(format!("Failed to resolve '{object}' as a valid ref.")))?;
    let annotated = o.annotate
        || o.sign
        || !o.messages.is_empty()
        || !o.files.is_empty()
        || o.edit
        || ctx.config.bool("tag.gpgSign") == Some(true);
    ensure_mapped(ws, peel(ws, target).await?).await?;
    let value = if annotated {
        let RefTarget::Changeset(commit) = target else {
            return Err(CliError::Fatal("asc: tagging a tag object is not supported".into()));
        };
        annotate(ctx, ws, o, name, commit).await?.0
    } else {
        target
    };
    let was = match existing {
        Some(old) if old != value => Some(object_oid_of(ws, old).await?.to_string()),
        _ => None,
    };
    let u = RefUpdate {
        name: ref_name(&full)?,
        expect: existing.map_or(Expect::Absent, Expect::Value),
        new: Some(value),
        expires_at: None,
    };
    ctx.update_local_refs(txn(vec![u]), &format!("tag: tagging {name}"), StagingAfterMove::Keep).await?;
    if let Some(old) = was {
        println!("Updated tag '{name}' (was {})", &old[..abbrev_len(ctx)]);
    }
    Ok(ExitCode::SUCCESS)
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let ws = ctx.ws()?;
    let o = parse(&a.argv())?;
    let filtered = !(o.contains.is_empty()
        && o.no_contains.is_empty()
        && o.merged.is_empty()
        && o.no_merged.is_empty()
        && o.points_at.is_empty());
    let creating = o.annotate || o.sign || !o.messages.is_empty() || !o.files.is_empty() || o.edit || o.force;
    if o.delete {
        if o.list || creating {
            return Err(CliError::Fatal("options '-d' and the listing and creating options are incompatible".into()));
        }
        return delete(ctx, ws, &o).await;
    }
    if o.list || o.lines.is_some() || filtered || (o.words.is_empty() && !creating) {
        if creating && !o.list {
            return Err(CliError::Usage(USAGE.to_owned()));
        }
        return list(ctx, ws, &o).await;
    }
    if o.words.is_empty() {
        return Err(CliError::Usage(USAGE.to_owned()));
    }
    create(ctx, ws, &o).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(items: &[&str]) -> Opts {
        parse(&items.iter().map(OsString::from).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn n_takes_only_an_attached_number() {
        assert_eq!(opts(&["-n"]).lines, Some(1));
        assert_eq!(opts(&["-n5", "-l"]).lines, Some(5));
        let o = opts(&["-n", "v1*"]);
        assert_eq!((o.lines, o.words.as_slice()), (Some(1), ["v1*".to_owned()].as_slice()));
        assert!(parse(&[OsString::from("-nx")]).is_err());
    }

    #[test]
    fn messages_repeat_and_clusters_split() {
        let o = opts(&["-fa", "-m", "one", "-mtwo", "v1"]);
        assert!(o.force && o.annotate);
        assert_eq!(o.messages, ["one", "two"]);
        assert_eq!(o.words, ["v1"]);
        let o = opts(&["-u", "KEY", "v1"]);
        assert!(o.sign);
        assert_eq!(o.key.as_deref(), Some("KEY"));
    }

    #[test]
    fn filters_default_to_head() {
        let o = opts(&["--no-merged=x", "--points-at"]);
        assert_eq!(o.no_merged, ["x"]);
        assert_eq!(o.points_at, ["HEAD"]);
        assert_eq!(opts(&["--contains", "v1"]).contains, ["v1"]);
    }
}
