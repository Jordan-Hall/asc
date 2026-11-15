//! `asc signing status|test [--json]`: what this machine signs with (`user.signingkey`, `gpg.format`,
//! `commit.gpgsign`, `tag.gpgsign`, `gpg.ssh.allowedSignersFile`, and the config level of each) and a sign-and-verify
//! check of the key. Nothing here prints key material: a key file shows its path and fingerprint only.
//!
//! `asc signing default [on|off] [--remote R] [--json]`: whether every new change is signed without a flag
//! (`Signing.GetSigningDefaults`, `Signing.SetSigningDefaults`). The value lives on the server with the account, so
//! the web settings page and every client agree; this command keeps a copy in the global config key
//! `asc.signByDefault`. Commands that write changes read [`sign_by_default`]: a set `commit.gpgsign` wins, then
//! `asc.signByDefault`, else off.

use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output, Stdio};
use std::time::{Duration, Instant};

use asc_core::sshsig::{self, KeyFileSigner, PublicKey};
use serde_json::{Value, json};

use asc_rpc::server::wire;
use tonic::transport::Channel;

use super::acct::{bad, json, out};
use super::agent::conn::{Conn, MAX_MESSAGE};
use super::common::{Ctx, Result};
use super::config::ConfigSnapshot;
use crate::native::NativeSpec;
use asc_rpc::services::signing_client::SigningClient;

/// The global config key that keeps the account's signing default.
pub const KEY_SIGN_BY_DEFAULT: &str = "asc.signByDefault";

/// Whether a command that writes a change signs it when no flag says. `commit.gpgsign`, when set at any
/// level, wins; otherwise the account's default as `asc signing default` last saw it.
pub fn sign_by_default(config: &ConfigSnapshot) -> bool {
    config
        .bool("commit.gpgsign")
        .or_else(|| config.bool(KEY_SIGN_BY_DEFAULT))
        .unwrap_or(false)
}

fn client(s: &crate::remote::server::Session) -> SigningClient<Channel> {
    SigningClient::new(s.wire.channel.clone())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE)
}

#[derive(clap::Parser, Debug)]
#[command(
    name = "signing",
    disable_help_flag = true,
    disable_version_flag = true,
    disable_help_subcommand = true
)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Sub,
}

#[derive(clap::Subcommand, Debug)]
pub enum Sub {
    /// Show or set whether new changes are signed by default.
    Default(DefaultArgs),
    /// Show the signing key, format and flags this machine uses.
    Status(JsonArgs),
    /// Sign a fixed buffer with the configured key and verify it.
    Test(JsonArgs),
}

#[derive(clap::Args, Debug)]
pub struct JsonArgs {
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct DefaultArgs {
    /// `on` or `off`; shows the current value when absent.
    value: Option<String>,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    json: bool,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "signing",
    flags: &["--remote", "--json"],
    placeholders: &[],
}];

pub(crate) fn parse_switch(s: &str) -> Result<bool> {
    match s {
        "on" | "true" | "yes" => Ok(true),
        "off" | "false" | "no" => Ok(false),
        _ => Err(bad("signing default", "the value is on or off")),
    }
}

/// Keeps the account's value in the global config, so commands that run offline still follow it.
async fn remember(ctx: &Ctx, on: bool) -> Result<()> {
    let rest: Vec<OsString> = [
        "--global",
        KEY_SIGN_BY_DEFAULT,
        if on { "true" } else { "false" },
    ]
    .map(OsString::from)
    .to_vec();
    super::config::run(ctx, super::config::Args { rest }).await?;
    Ok(())
}

pub async fn run(ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let d = match a.cmd {
        Sub::Default(d) => d,
        Sub::Status(j) => return status(ctx, j.json),
        Sub::Test(j) => return test(ctx, j.json).await,
    };
    let want = d.value.as_deref().map(parse_switch).transpose()?;
    let conn = Conn::open(ctx, d.remote.as_deref()).await?;
    let (on, updated) = match want {
        Some(v) => {
            let msg = wire::SetSigningDefaultsRequest { sign_by_default: v };
            let r = conn
                .call(true, |s, op| async move {
                    Ok(client(&s)
                        .set_signing_defaults(s.wire.request(msg, op))
                        .await?
                        .into_inner())
                })
                .await?;
            (r.sign_by_default, r.updated)
        }
        None => {
            let r = conn
                .call(false, |s, _| async move {
                    Ok(client(&s)
                        .get_signing_defaults(
                            s.wire
                                .request(wire::GetSigningDefaultsRequest::default(), None),
                        )
                        .await?
                        .into_inner())
                })
                .await?;
            (r.sign_by_default, r.updated)
        }
    };
    remember(ctx, on).await?;
    if d.json {
        return json(
            "asc.v1.GetSigningDefaultsResponse",
            &wire::GetSigningDefaultsResponse {
                sign_by_default: on,
                updated,
            },
        );
    }
    let overridden = ctx.config.bool("commit.gpgsign").filter(|g| *g != on);
    out(|out| {
        writeln!(out, "sign by default: {}", if on { "on" } else { "off" })?;
        if let Some(g) = overridden {
            writeln!(
                out,
                "commit.gpgsign is {} here and wins over the account default",
                if g { "true" } else { "false" }
            )?;
        }
        Ok(ExitCode::SUCCESS)
    })
}

/// The bytes `asc signing test` signs.
const FIXED: &[u8] = b"asc signing test\n";
/// The most bytes read from a key file; a larger file is refused.
const MAX_KEY_FILE: u64 = 64 * 1024;
/// How long a signing program may run before it is killed.
const SIGNER_TIMEOUT: Duration = Duration::from_secs(120);

/// The config level a value came from: the last section that sets it decides.
fn level(
    config: &ConfigSnapshot,
    section: &str,
    sub: Option<&str>,
    name: &str,
) -> Option<&'static str> {
    use gix_config::Source;
    let mut found = None;
    for s in config.file().sections() {
        let h = s.header();
        if !h.name().to_string().eq_ignore_ascii_case(section)
            || h.subsection_name().map(|x| x.to_string()).as_deref() != sub
            || s.body().value(name).is_none()
        {
            continue;
        }
        found = Some(match s.meta().source {
            Source::GitInstallation | Source::System => "system",
            Source::Git | Source::User => "global",
            Source::Local | Source::Worktree => "local",
            _ => "command",
        });
    }
    found
}

/// What the configured key is and whether it can sign.
struct KeyInfo {
    /// `none`, `file`, `public-key-file`, `literal`, `agent` or `key-id`.
    source: &'static str,
    path: Option<String>,
    id: Option<String>,
    fingerprint: Option<String>,
    usable: bool,
    problem: Option<String>,
    /// The public key, for an SSH key that signs through the agent.
    public: Option<PublicKey>,
}

impl KeyInfo {
    fn new(source: &'static str) -> Self {
        Self {
            source,
            path: None,
            id: None,
            fingerprint: None,
            usable: false,
            problem: None,
            public: None,
        }
    }

    fn fail(mut self, why: impl Into<String>) -> Self {
        self.usable = false;
        self.problem = Some(why.into());
        self
    }
}

/// The programs `inspect` asks: the keys in ssh-agent, and the fingerprint of a secret OpenPGP key.
struct Probe<'a> {
    agent_keys: &'a dyn Fn() -> std::result::Result<Vec<PublicKey>, String>,
    gpg_fingerprint: &'a dyn Fn(&str) -> std::result::Result<String, String>,
}

fn format_of(config: &ConfigSnapshot) -> String {
    config
        .get("gpg.format")
        .map_or_else(|| "openpgp".to_owned(), |f| f.to_ascii_lowercase())
}

fn gpg_program(config: &ConfigSnapshot) -> String {
    config
        .get("gpg.program")
        .unwrap_or_else(|| "gpg".to_owned())
}

/// `ssh-add -L`, parsed.
fn agent_keys(ctx: &Ctx) -> std::result::Result<Vec<PublicKey>, String> {
    let out = ctx
        .child("ssh-add")
        .map_err(|_| "ssh-add is not available".to_owned())?
        .arg("-L")
        .stdin(Stdio::null())
        .output()
        .map_err(|_| "ssh-add is not available".to_owned())?;
    if !out.status.success() {
        return Err("no ssh-agent holds keys (ssh-add -L failed)".to_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| PublicKey::parse(l).ok())
        .collect())
}

/// The primary fingerprint of the secret key `id`, from `<gpg.program> --list-secret-keys`.
fn gpg_fingerprint(ctx: &Ctx, id: &str) -> std::result::Result<String, String> {
    let program = gpg_program(&ctx.config);
    let missing = || format!("{program} is not available");
    let out = ctx
        .child(&program)
        .map_err(|_| missing())?
        .args(["--batch", "--with-colons", "--list-secret-keys", "--"])
        .arg(id)
        .stdin(Stdio::null())
        .output()
        .map_err(|_| missing())?;
    let none = || format!("{program} has no secret key {id}");
    if !out.status.success() {
        return Err(none());
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            if f.first() == Some(&"fpr") {
                f.get(9).map(|x| (*x).to_owned())
            } else {
                None
            }
        })
        .filter(|f| !f.is_empty())
        .ok_or_else(none)
}

fn agent_info(probe: &Probe, mut info: KeyInfo, key: PublicKey) -> KeyInfo {
    info.fingerprint = Some(key.fingerprint().to_owned());
    match (probe.agent_keys)() {
        Ok(keys) if keys.iter().any(|k| k.fingerprint() == key.fingerprint()) => info.usable = true,
        Ok(_) => info.problem = Some("the key is not loaded in ssh-agent (ssh-add)".into()),
        Err(e) => info.problem = Some(e),
    }
    info.public = Some(key);
    info
}

/// A key given inline: `key::<public key>` (agent) or a bare public key line (literal).
fn literal_key(value: &str) -> Option<(&'static str, &str)> {
    value
        .strip_prefix("key::")
        .map(|k| ("agent", k))
        .or_else(|| {
            ["ssh-", "ecdsa-", "sk-"]
                .iter()
                .any(|p| value.starts_with(p))
                .then_some(("literal", value))
        })
}

/// A Windows network or device path (`\\host\share`, `//host/share`, `\\?\C:\k`).
fn is_unc(value: &str) -> bool {
    let b = value.as_bytes();
    b.len() >= 2 && matches!(b[0], b'\\' | b'/') && matches!(b[1], b'\\' | b'/')
}

/// On Windows a key path from repository config (not trusted) may not name a network share or a
/// device path: reading it would send the user's credentials to a host the repository picked.
fn check_key_path(value: &str, repo_local: bool) -> std::result::Result<(), String> {
    if cfg!(windows) && repo_local && is_unc(value) {
        return Err("the key file path is a network or device path from repository config".into());
    }
    Ok(())
}

/// Reads a key file: a regular file of at most [`MAX_KEY_FILE`] bytes. A FIFO, a device (`/dev/zero`)
/// and a directory are refused before and after the open.
fn read_key_file(path: &Path) -> std::result::Result<String, String> {
    use std::io::Read as _;
    let cannot = |e: std::io::Error| format!("the key file cannot be read: {}", e.kind());
    let regular = "the key file is not a regular file";
    if !std::fs::metadata(path).map_err(cannot)?.is_file() {
        return Err(regular.into());
    }
    let file = std::fs::File::open(path).map_err(cannot)?;
    if !file.metadata().map_err(cannot)?.is_file() {
        return Err(regular.into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_KEY_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(cannot)?;
    if bytes.len() as u64 > MAX_KEY_FILE {
        return Err("the key file is larger than 64 KiB".into());
    }
    String::from_utf8(bytes).map_err(|_| "the file is not an OpenSSH key".to_owned())
}

fn inspect(config: &ConfigSnapshot, probe: &Probe) -> KeyInfo {
    let Some(value) = config.get("user.signingkey").filter(|v| !v.is_empty()) else {
        return KeyInfo::new("none").fail("user.signingkey is not set");
    };
    if format_of(config) != "ssh" {
        let mut info = KeyInfo::new("key-id");
        info.id = Some(value.clone());
        match (probe.gpg_fingerprint)(&value) {
            Ok(fp) => {
                info.fingerprint = Some(fp);
                info.usable = true;
            }
            Err(e) => info.problem = Some(e),
        }
        return info;
    }
    if let Some((source, line)) = literal_key(&value) {
        let info = KeyInfo::new(source);
        return match PublicKey::parse(line) {
            Ok(k) => agent_info(probe, info, k),
            Err(e) => info.fail(format!("the key is not usable: {e}")),
        };
    }
    let path = config
        .path("user.signingkey")
        .unwrap_or_else(|| PathBuf::from(&value));
    let mut info = KeyInfo::new("file");
    info.path = Some(path.display().to_string().replace('\\', "/"));
    let repo_local = level(config, "user", None, "signingkey") == Some("local");
    let text = match check_key_path(&value, repo_local).and_then(|()| read_key_file(&path)) {
        Ok(t) => t,
        Err(e) => return info.fail(e),
    };
    match KeyFileSigner::from_openssh(&text) {
        Ok(signer) => {
            info.fingerprint = Some(signer.public_key().fingerprint().to_owned());
            info.usable = true;
            info
        }
        Err(sshsig::Error::Key(m)) if text.contains("PRIVATE KEY") => info.fail(m),
        Err(_) => match PublicKey::parse(&text) {
            Ok(k) => {
                info.source = "public-key-file";
                agent_info(probe, info, k)
            }
            Err(_) => info.fail("the file is not an OpenSSH key"),
        },
    }
}

fn setting(value: Option<Value>, level: Option<&'static str>) -> Value {
    json!({ "value": value, "level": level })
}

/// The JSON of `asc signing status`.
fn status_json(c: &ConfigSnapshot, k: &KeyInfo) -> Value {
    let flag = |key: &str, section: &str| {
        setting(
            Some(json!(c.bool(key).unwrap_or(false))),
            level(c, section, None, "gpgsign"),
        )
    };
    json!({
        "configured": k.source != "none",
        "key": {
            "source": k.source,
            "path": k.path,
            "id": k.id,
            "fingerprint": k.fingerprint,
            "level": level(c, "user", None, "signingkey"),
            "usable": k.usable,
            "problem": k.problem,
        },
        "format": setting(Some(json!(format_of(c))), level(c, "gpg", None, "format")),
        "commit_sign": flag("commit.gpgsign", "commit"),
        "tag_sign": flag("tag.gpgsign", "tag"),
        "allowed_signers": setting(
            c.get("gpg.ssh.allowedsignersfile").map(|v| json!(v)),
            level(c, "gpg", Some("ssh"), "allowedsignersfile"),
        ),
    })
}

fn at(v: &Value) -> String {
    v["level"]
        .as_str()
        .map(|l| format!(" ({l})"))
        .unwrap_or_default()
}

/// The text of `asc signing status`.
fn status_text(v: &Value, k: &KeyInfo) -> String {
    let what = match k.source {
        "none" => "none".to_owned(),
        "key-id" => format!("key id {}", k.id.as_deref().unwrap_or("")),
        "file" => format!("key file {}", k.path.as_deref().unwrap_or("")),
        "public-key-file" => format!("public key file {}", k.path.as_deref().unwrap_or("")),
        s => format!("{s} public key"),
    };
    let fp = k
        .fingerprint
        .as_deref()
        .map(|f| format!(", {f}"))
        .unwrap_or_default();
    let usable = match (&k.problem, k.usable) {
        (_, true) => "yes".to_owned(),
        (Some(p), _) => format!("no, {p}"),
        _ => "no".to_owned(),
    };
    let on = |x: &Value| if x["value"] == true { "on" } else { "off" };
    let signers = match v["allowed_signers"]["value"].as_str() {
        Some(p) => format!("{p}{}", at(&v["allowed_signers"])),
        None => "not set".to_owned(),
    };
    format!(
        "key: {what}{fp}{}\nusable: {usable}\nformat: {}{}\ncommit signing: {}{}\ntag signing: {}{}\nallowed signers: {signers}\n",
        at(&v["key"]),
        v["format"]["value"].as_str().unwrap_or(""),
        at(&v["format"]),
        on(&v["commit_sign"]),
        at(&v["commit_sign"]),
        on(&v["tag_sign"]),
        at(&v["tag_sign"]),
    )
}

fn inspect_ctx(ctx: &Ctx) -> KeyInfo {
    inspect(
        &ctx.config,
        &Probe {
            agent_keys: &|| agent_keys(ctx),
            gpg_fingerprint: &|id| gpg_fingerprint(ctx, id),
        },
    )
}

fn status(ctx: &Ctx, as_json: bool) -> Result<ExitCode> {
    let k = inspect_ctx(ctx);
    let v = status_json(&ctx.config, &k);
    out(|w| {
        if as_json {
            writeln!(
                w,
                "{}",
                serde_json::to_string_pretty(&v).unwrap_or_default()
            )?;
        } else {
            write!(w, "{}", status_text(&v, &k))?;
        }
        Ok(ExitCode::SUCCESS)
    })
}

/// A fresh scratch directory under the temp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> std::io::Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let p =
            std::env::temp_dir().join(format!("asc-signing-test-{}-{nanos}", std::process::id()));
        std::fs::create_dir(&p)?;
        Ok(Self(p))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs a signing program with `input` on its stdin and collects stdout and stderr. stdin stays a
/// pipe for the data; a pinentry or a passphrase prompt reaches the terminal on its own. The child
/// is killed when it runs past `timeout`.
fn run_signer_for(
    mut cmd: Command,
    input: Vec<u8>,
    timeout: Duration,
) -> std::result::Result<Output, String> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("the signing program cannot run: {}", e.kind()))?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    std::thread::spawn(move || {
        if let Some(mut pipe) = stdin {
            let _ = pipe.write_all(&input);
        }
    });
    fn reader<R: std::io::Read + Send + 'static>(
        pipe: Option<R>,
    ) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut v = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut v);
            }
            v
        })
    }
    let out_t = reader(stdout);
    let err_t = reader(stderr);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {}
            Err(e) => return Err(e.to_string()),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "the signer timed out after {} s and was stopped",
                timeout.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Ok(Output {
        status,
        stdout: out_t.join().unwrap_or_default(),
        stderr: err_t.join().unwrap_or_default(),
    })
}

/// The command that signs with OpenPGP or X.509: `<program> --status-fd=2 -bsau <key>`, as git runs it.
fn gpg_command(
    ctx: &Ctx,
    program: &str,
    key: Option<&str>,
    who: &str,
) -> std::result::Result<Command, String> {
    let key = key
        .map(str::to_owned)
        .or_else(|| ctx.config.get("user.signingkey"))
        .unwrap_or_else(|| who.to_owned());
    let mut cmd = ctx.child(ctx.shell()).map_err(|e| e.to_string())?;
    cmd.args(["-c", "exec \"$@\"", "sh"])
        .arg(program)
        .args(["--status-fd=2", "-bsau"])
        .arg(key);
    Ok(cmd)
}

/// The command that signs with SSH: `<gpg.ssh.program> -Y sign -n git -f <key>`, as git runs it. A
/// key file is read here under the key-file limits to tell a private key (ssh-keygen signs with it)
/// from a public key (ssh-keygen signs through the agent, `-U`); an inline public key goes to a
/// scratch file that the returned guard removes.
fn ssh_command(
    ctx: &Ctx,
    key: Option<&str>,
) -> std::result::Result<(Command, Option<Scratch>), String> {
    let configured = ctx.config.get("user.signingkey").filter(|v| !v.is_empty());
    let value = key
        .map(str::to_owned)
        .or_else(|| configured.clone())
        .ok_or("error: user.signingkey needs to be set for ssh signing")?;
    let from_config = configured.as_deref() == Some(value.as_str());
    let program = ctx
        .config
        .get("gpg.ssh.program")
        .unwrap_or_else(|| "ssh-keygen".to_owned());
    let mut cmd = ctx.child(&program).map_err(|e| e.to_string())?;
    cmd.args(["-Y", "sign", "-n", "git"]);
    let (file, scratch, agent) = if let Some((_, line)) = literal_key(&value) {
        let k = PublicKey::parse(line).map_err(|e| format!("error: the key is not usable: {e}"))?;
        let dir = Scratch::new().map_err(|e| e.to_string())?;
        let f = dir.0.join("key.pub");
        std::fs::write(&f, format!("{}\n", k.line())).map_err(|e| e.to_string())?;
        (f, Some(dir), true)
    } else {
        let repo_local =
            from_config && level(&ctx.config, "user", None, "signingkey") == Some("local");
        check_key_path(&value, repo_local).map_err(|e| format!("error: {e}"))?;
        let path = from_config
            .then(|| ctx.config.path("user.signingkey"))
            .flatten()
            .unwrap_or_else(|| PathBuf::from(&value));
        let text = read_key_file(&path).map_err(|e| format!("error: {e}"))?;
        let private = text.contains("PRIVATE KEY") || KeyFileSigner::from_openssh(&text).is_ok();
        if !private && PublicKey::parse(&text).is_err() {
            return Err("error: the file is not an OpenSSH key".into());
        }
        (path, None, !private)
    };
    if agent {
        cmd.arg("-U");
    }
    cmd.arg("-f").arg(file);
    Ok((cmd, scratch))
}

/// Signs `payload` the way git signs a commit or tag. `asc commit`, `asc tag` and `asc signing test`
/// all use this one function. `gpg.format` picks the program: `openpgp` runs `gpg.program` (default
/// `gpg`) and `x509` runs `gpg.x509.program` (default `gpgsm`), both as `--status-fd=2 -bsau <key>`;
/// `ssh` makes an SSHSIG in the namespace `git` with `gpg.ssh.program` (default `ssh-keygen`). The
/// result has no trailing newline. The error is the text for the user, without the closing line
/// that names the object being written. A program that runs past 120 s is killed.
pub(crate) async fn sign_payload(
    ctx: &Ctx,
    payload: &[u8],
    key: Option<&str>,
    who: &str,
) -> std::result::Result<Vec<u8>, String> {
    let format = format_of(&ctx.config);
    let (cmd, _scratch, label, gpg_status) = match format.as_str() {
        "openpgp" => {
            let program = ctx
                .config
                .get("gpg.openpgp.program")
                .or_else(|| ctx.config.get("gpg.program"))
                .unwrap_or_else(|| "gpg".to_owned());
            (gpg_command(ctx, &program, key, who)?, None, "gpg", true)
        }
        "x509" => {
            let program = ctx
                .config
                .get("gpg.x509.program")
                .unwrap_or_else(|| "gpgsm".to_owned());
            (gpg_command(ctx, &program, key, who)?, None, "gpg", true)
        }
        "ssh" => {
            let (cmd, scratch) = ssh_command(ctx, key)?;
            (cmd, scratch, "ssh-keygen", false)
        }
        other => return Err(format!("fatal: invalid value for 'gpg.format': '{other}'")),
    };
    let payload = payload.to_vec();
    let out = tokio::task::spawn_blocking(move || run_signer_for(cmd, payload, SIGNER_TIMEOUT))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("error: {e}"))?;
    let mut sig = out.stdout;
    while sig.last() == Some(&b'\n') {
        sig.pop();
    }
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    // An OpenPGP or X.509 program must say it made a signature (`--status-fd`), as git insists.
    let made = if gpg_status {
        stderr.starts_with("[GNUPG:] SIG_CREATED ") || stderr.contains("\n[GNUPG:] SIG_CREATED ")
    } else {
        sig.starts_with(b"-----BEGIN SSH SIGNATURE-----")
    };
    if !out.status.success() || sig.is_empty() || !made {
        let mut text = format!("error: {label} failed to sign the data");
        if !stderr.trim().is_empty() {
            text.push_str(&format!(":\n{}", stderr.trim_end()));
        }
        return Err(text);
    }
    Ok(sig)
}

/// The first line of a signing failure, for `signing test`.
fn short(e: &str) -> String {
    e.lines()
        .next()
        .unwrap_or("")
        .trim_start_matches("error: ")
        .to_owned()
}

/// Signs [`FIXED`] with the code `asc commit` uses and verifies the SSHSIG in the namespace `git`
/// with `ssh-keygen -Y check-novalidate`; the signature must name the configured key.
async fn sign_with_ssh(ctx: &Ctx, k: &KeyInfo) -> std::result::Result<(), String> {
    let sig = sign_payload(ctx, FIXED, None, "")
        .await
        .map_err(|e| short(&e))?;
    let dir = Scratch::new().map_err(|e| e.to_string())?;
    let sigf = dir.0.join("sig");
    std::fs::write(&sigf, &sig).map_err(|e| e.to_string())?;
    let program = ctx
        .config
        .get("gpg.ssh.program")
        .unwrap_or_else(|| "ssh-keygen".to_owned());
    let mut cmd = ctx.child(&program).map_err(|e| e.to_string())?;
    cmd.args(["-Y", "check-novalidate", "-n", "git", "-s"])
        .arg(&sigf);
    let out = run_signer_for(cmd, FIXED.to_vec(), SIGNER_TIMEOUT)?;
    let said = String::from_utf8_lossy(&out.stdout);
    let fp = k.fingerprint.as_deref().unwrap_or("-");
    if out.status.success() && said.contains(fp) {
        Ok(())
    } else {
        Err("the signature does not verify".to_owned())
    }
}

/// Signs [`FIXED`] with the code `asc commit` uses (`gpg.program`) and verifies it with the same program.
async fn sign_with_gpg(ctx: &Ctx) -> std::result::Result<(), String> {
    let sig = sign_payload(ctx, FIXED, None, "")
        .await
        .map_err(|e| short(&e))?;
    let dir = Scratch::new().map_err(|e| e.to_string())?;
    let (sigf, data) = (dir.0.join("sig"), dir.0.join("data"));
    std::fs::write(&sigf, &sig).map_err(|e| e.to_string())?;
    std::fs::write(&data, FIXED).map_err(|e| e.to_string())?;
    let program = gpg_program(&ctx.config);
    let missing = || format!("{program} is not available");
    let mut cmd = ctx.child(&program).map_err(|_| missing())?;
    cmd.args(["--batch", "--status-fd=1", "--verify"])
        .arg(&sigf)
        .arg(&data);
    let out = run_signer_for(cmd, Vec::new(), SIGNER_TIMEOUT)?;
    if out.status.success() && String::from_utf8_lossy(&out.stdout).contains("GOODSIG") {
        Ok(())
    } else {
        Err("the signature does not verify".to_owned())
    }
}

async fn sign_and_verify(ctx: &Ctx, k: &KeyInfo) -> std::result::Result<(), String> {
    match (format_of(&ctx.config).as_str(), k.source) {
        (_, "none") => Err("user.signingkey is not set".to_owned()),
        ("ssh", _) => sign_with_ssh(ctx, k).await,
        ("openpgp", "key-id") => sign_with_gpg(ctx).await,
        _ => Err("only the openpgp and ssh formats can be tested".to_owned()),
    }
}

async fn test(ctx: &Ctx, as_json: bool) -> Result<ExitCode> {
    let k = inspect_ctx(ctx);
    let result = if k.usable {
        sign_and_verify(ctx, &k).await
    } else {
        Err(k
            .problem
            .clone()
            .unwrap_or_else(|| "the key is not usable".into()))
    };
    let error = result.err();
    let ok = error.is_none();
    let code = if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    };
    out(|w| {
        if as_json {
            let v = json!({
                "ok": ok,
                "format": format_of(&ctx.config),
                "source": k.source,
                "fingerprint": k.fingerprint,
                "error": error,
            });
            writeln!(
                w,
                "{}",
                serde_json::to_string_pretty(&v).unwrap_or_default()
            )?;
        } else if let Some(e) = &error {
            writeln!(w, "failed: {e}")?;
        } else {
            writeln!(
                w,
                "ok: signed and verified, key {}",
                k.fingerprint.as_deref().unwrap_or("")
            )?;
        }
        Ok(code)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_parse() {
        assert!(parse_switch("on").unwrap());
        assert!(!parse_switch("off").unwrap());
        assert!(parse_switch("true").unwrap());
        assert!(parse_switch("maybe").is_err());
        assert!(parse_switch("").is_err());
    }

    #[test]
    fn commit_gpgsign_wins_over_the_account_default() {
        let c = |t: &str| ConfigSnapshot::from_text(t);
        assert!(!sign_by_default(&c("")));
        assert!(sign_by_default(&c("[asc]\n\tsignByDefault = true\n")));
        assert!(!sign_by_default(&c("[asc]\n\tsignByDefault = false\n")));
        assert!(sign_by_default(&c("[commit]\n\tgpgsign = true\n")));
        assert!(!sign_by_default(&c(
            "[commit]\n\tgpgsign = false\n[asc]\n\tsignByDefault = true\n"
        )));
        assert!(sign_by_default(&c(
            "[commit]\n\tgpgsign = true\n[asc]\n\tsignByDefault = false\n"
        )));
    }

    const ED_PRIVATE: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACBEHO7rHJ3AV+cNx9RP5Y4LKRX3Ny/ywRZ9d2Sd7WVvxgAAAJDe4Cnq3uAp
6gAAAAtzc2gtZWQyNTUxOQAAACBEHO7rHJ3AV+cNx9RP5Y4LKRX3Ny/ywRZ9d2Sd7WVvxg
AAAECyVlI1ETu9uLtTIPfbajG0zVEMdAsj8RUn6qUZSM/pqEQc7uscncBX5w3H1E/ljgsp
Ffc3L/LBFn13ZJ3tZW/GAAAACWVkX2FAdGVzdAECAwQ=
-----END OPENSSH PRIVATE KEY-----
";
    const ED_PUBLIC: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEQc7uscncBX5w3H1E/ljgspFfc3L/LBFn13ZJ3tZW/G ed_a@test";

    fn no_probe<R>(f: impl FnOnce(&Probe) -> R) -> R {
        f(&Probe {
            agent_keys: &|| Err("no agent".to_owned()),
            gpg_fingerprint: &|_| Err("no gpg".to_owned()),
        })
    }

    fn key_file() -> (Scratch, String) {
        let dir = Scratch::new().unwrap();
        let path = dir.0.join("id_test");
        std::fs::write(&path, ED_PRIVATE).unwrap();
        let path = path.display().to_string().replace('\\', "/");
        (dir, path)
    }

    fn ssh_config(key: &str) -> ConfigSnapshot {
        ConfigSnapshot::from_text(&format!(
            "[user]
	signingkey = {key}
[gpg]
	format = ssh
[commit]
	gpgsign = true
"
        ))
    }

    #[test]
    fn status_with_no_config_is_unconfigured() {
        let c = ConfigSnapshot::from_text("");
        let k = no_probe(|p| inspect(&c, p));
        assert_eq!(k.source, "none");
        let v = status_json(&c, &k);
        assert_eq!(v["configured"], false);
        assert_eq!(v["key"]["usable"], false);
        assert_eq!(v["key"]["level"], Value::Null);
        assert_eq!(v["format"]["value"], "openpgp");
        assert_eq!(v["commit_sign"]["value"], false);
        assert_eq!(v["tag_sign"]["value"], false);
        assert_eq!(v["allowed_signers"]["value"], Value::Null);
        assert!(status_text(&v, &k).contains("key: none"));
    }

    #[test]
    fn an_ssh_key_file_shows_path_and_fingerprint_never_the_key() {
        let (_dir, path) = key_file();
        let c = ssh_config(&path);
        let k = no_probe(|p| inspect(&c, p));
        assert_eq!((k.source, k.usable), ("file", true), "{:?}", k.problem);
        let want = PublicKey::parse(ED_PUBLIC).unwrap();
        assert_eq!(k.fingerprint.as_deref(), Some(want.fingerprint()));
        let v = status_json(&c, &k);
        assert_eq!(v["key"]["path"], path.as_str());
        assert_eq!(v["format"]["value"], "ssh");
        assert_eq!(v["commit_sign"]["value"], true);
        assert!(v["key"]["level"].is_string());
        let shown = format!("{v}{}", status_text(&v, &k));
        for line in ED_PRIVATE.lines().filter(|l| !l.starts_with("-----")) {
            assert!(!shown.contains(line), "private key text in the output");
        }
        assert!(shown.contains(want.fingerprint()));
    }

    #[test]
    fn a_key_file_is_read_under_the_limits() {
        let (dir, path) = key_file();
        assert!(read_key_file(Path::new(&path)).is_ok());
        let big = dir.0.join("big");
        std::fs::write(&big, vec![b'a'; MAX_KEY_FILE as usize + 1]).unwrap();
        assert!(
            read_key_file(&big)
                .unwrap_err()
                .contains("larger than 64 KiB")
        );
        let exact = dir.0.join("exact");
        std::fs::write(&exact, vec![b'a'; MAX_KEY_FILE as usize]).unwrap();
        assert!(read_key_file(&exact).is_ok());
        assert!(
            read_key_file(&dir.0)
                .unwrap_err()
                .contains("not a regular file")
        );
        let c = ssh_config(&dir.0.display().to_string().replace('\\', "/"));
        let k = no_probe(|p| inspect(&c, p));
        assert!(!k.usable);
        assert!(k.problem.unwrap().contains("not a regular file"));
    }

    #[cfg(unix)]
    #[test]
    fn a_device_and_a_fifo_are_refused() {
        assert!(
            read_key_file(Path::new("/dev/zero"))
                .unwrap_err()
                .contains("not a regular file")
        );
        let dir = Scratch::new().unwrap();
        let fifo = dir.0.join("fifo");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            read_key_file(&fifo)
                .unwrap_err()
                .contains("not a regular file")
        );
    }

    #[test]
    fn network_paths_from_repo_config_are_refused_on_windows() {
        assert!(is_unc(r"\\host\share\k"));
        assert!(is_unc("//host/share/k"));
        assert!(is_unc(r"\\?\C:\k"));
        assert!(!is_unc("C:/k") && !is_unc("/home/k") && !is_unc("k"));
        assert!(check_key_path("//host/share/k", false).is_ok());
        assert_eq!(
            check_key_path("//host/share/k", true).is_err(),
            cfg!(windows)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_signer_that_runs_too_long_is_killed() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let began = Instant::now();
        let err = run_signer_for(cmd, Vec::new(), Duration::from_millis(300)).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(began.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn an_unusable_key_file_is_reported() {
        let c = ssh_config("/no/such/asc-key");
        let k = no_probe(|p| inspect(&c, p));
        assert!(!k.usable);
        assert!(k.problem.as_deref().unwrap().contains("cannot be read"));
        let dir = Scratch::new().unwrap();
        let junk = dir.0.join("junk");
        std::fs::write(
            &junk,
            "not a key
",
        )
        .unwrap();
        let c = ssh_config(&junk.display().to_string().replace('\\', "/"));
        let k = no_probe(|p| inspect(&c, p));
        assert!(!k.usable);
        assert!(k.problem.as_deref().unwrap().contains("not an OpenSSH key"));
    }

    #[test]
    fn agent_keys_are_usable_only_when_the_agent_holds_them() {
        let key = PublicKey::parse(ED_PUBLIC).unwrap();
        for form in [format!("key::{ED_PUBLIC}"), ED_PUBLIC.to_owned()] {
            let c = ssh_config(&form);
            let held = || Ok(vec![key.clone()]);
            let k = inspect(
                &c,
                &Probe {
                    agent_keys: &held,
                    gpg_fingerprint: &|_| Err(String::new()),
                },
            );
            assert!(k.usable, "{form}");
            assert_eq!(k.fingerprint.as_deref(), Some(key.fingerprint()));
            assert!(k.path.is_none());
            let k = inspect(
                &c,
                &Probe {
                    agent_keys: &|| Ok(vec![]),
                    gpg_fingerprint: &|_| Err(String::new()),
                },
            );
            assert!(!k.usable);
            assert!(k.problem.as_deref().unwrap().contains("not loaded"));
            let v = status_json(&c, &k);
            assert!(
                !v.to_string().contains("AAAAC3NzaC1lZDI1NTE5"),
                "public blob printed"
            );
        }
        assert_eq!(
            inspect(
                &ssh_config("key::bogus"),
                &Probe {
                    agent_keys: &|| Ok(vec![]),
                    gpg_fingerprint: &|_| Err(String::new()),
                }
            )
            .source,
            "agent"
        );
    }

    #[test]
    fn an_openpgp_key_id_is_checked_with_gpg() {
        let c = ConfigSnapshot::from_text(
            "[user]
	signingkey = 0xABCD
",
        );
        let k = inspect(
            &c,
            &Probe {
                agent_keys: &|| Err(String::new()),
                gpg_fingerprint: &|id| Ok(format!("FPR-{id}")),
            },
        );
        assert_eq!((k.source, k.usable), ("key-id", true));
        assert_eq!(k.fingerprint.as_deref(), Some("FPR-0xABCD"));
        let k = no_probe(|p| inspect(&c, p));
        assert!(!k.usable);
    }
}
