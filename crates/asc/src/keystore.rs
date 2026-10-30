//! Where `asc` keeps a token when no git credential helper is configured, so a sign-in does not
//! repeat on every run.
//!
//! A configured credential helper (`credential.helper`) always wins: git's own store, the
//! keychain helpers and Git Credential Manager already do this job. Only without one, the token
//! of a sign-in is kept here, per origin (`scheme://host:port`):
//!
//! 1. the operating system's keychain, through its own command line tool, with the secret on
//!    standard input and never in an argument: Windows Credential Manager (Windows PowerShell's
//!    `PasswordVault`), the macOS Keychain (`security -i`), and a Secret Service keyring on
//!    Linux and the BSDs (`secret-tool`);
//! 2. else, when the keychain is missing or refuses, a `0600` file `credentials.json` in the asc
//!    home directory (on Windows the user's profile ACLs apply).
//!
//! `ASC_CREDENTIAL_STORE` picks: `auto` (the default: keychain, then file), `keychain`, `file` or
//! `off`. `asc auth logout` removes the token from both places. Nothing here prints or logs a
//! token, and a token that is not one visible word is never stored.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use asc_git::credential::section_entries;
use gix_config::File as Config;

/// Which stores `asc` may use; the value of this variable.
pub const STORE_ENV: &str = "ASC_CREDENTIAL_STORE";
/// The service name every entry is filed under.
const SERVICE: &str = "asc";
/// Longest wait for a keychain tool (it may be showing an unlock prompt).
const TOOL_TIMEOUT: Duration = Duration::from_secs(20);
/// Longest token kept.
const MAX_TOKEN: usize = 4096;
const FILE_NAME: &str = "credentials.json";

/// Whether any `credential.helper` is in effect (an empty value resets the list, as in git).
pub fn helper_configured(file: &Config) -> bool {
    let mut on = false;
    for e in section_entries(file, "credential") {
        if e.name == "helper" && e.trusted {
            on = e.value.as_deref().is_some_and(|v| !v.trim().is_empty());
        }
    }
    on
}

/// The origin an entry is filed under: `scheme://host[:port]`, lower case, no user information.
pub fn account_of(url: &url::Url) -> Option<String> {
    let host = url.host_str()?.to_ascii_lowercase();
    Some(match url.port() {
        Some(p) => format!("{}://{host}:{p}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    })
}

/// A token that may be handed to a keychain tool or a file: one visible word without quotes.
fn storable(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_TOKEN
        && token.bytes().all(|b| b.is_ascii_graphic())
        && !token.contains(['"', '\'', '\\', '`', '$'])
}

fn account_ok(account: &str) -> bool {
    !account.is_empty()
        && account.len() <= 300
        && account
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b":/.-_[]".contains(&b))
}

// ------------------------------------------------------------------------------ running tools

/// What a keychain tool answered.
#[derive(Debug)]
pub struct Answer {
    pub code: i32,
    pub stdout: String,
}

/// Runs a program (a test passes a fake).
pub trait Runner: Send + Sync {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        env: &[(&str, &str)],
        stdin: &str,
    ) -> std::io::Result<Answer>;
}

struct Real;

impl Runner for Real {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        env: &[(&str, &str)],
        stdin: &str,
    ) -> std::io::Result<Answer> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn()?;
        if let Some(mut pipe) = child.stdin.take() {
            // A tool that never reads its input is not an error here; its exit code tells.
            let _ = pipe.write_all(stdin.as_bytes());
        }
        let mut out = child.stdout.take();
        let reader = std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(o) = out.as_mut() {
                let _ = o.take(MAX_TOKEN as u64 + 16).read_to_string(&mut text);
            }
            text
        });
        let started = Instant::now();
        let code = loop {
            if let Some(status) = child.try_wait()? {
                break status.code().unwrap_or(-1);
            }
            if started.elapsed() > TOOL_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the keychain did not answer",
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let stdout = reader.join().unwrap_or_default();
        Ok(Answer { code, stdout })
    }
}

// ------------------------------------------------------------------------------ the keychains

/// A system keychain reached through its command line tool.
pub trait Native: Send + Sync {
    fn name(&self) -> &'static str;
    fn get(&self, account: &str) -> Result<Option<String>, String>;
    fn set(&self, account: &str, token: &str) -> Result<(), String>;
    fn delete(&self, account: &str) -> Result<(), String>;
}

fn tool_error(e: &std::io::Error) -> String {
    format!("{:?}", e.kind())
}

/// Linux and the BSDs: a Secret Service keyring through `secret-tool`.
pub struct SecretTool<'a>(pub &'a dyn Runner);

impl Native for SecretTool<'_> {
    fn name(&self) -> &'static str {
        "Secret Service keyring"
    }

    fn get(&self, account: &str) -> Result<Option<String>, String> {
        let a = self
            .0
            .run(
                "secret-tool",
                &["lookup", "service", SERVICE, "account", account],
                &[],
                "",
            )
            .map_err(|e| tool_error(&e))?;
        let token = a.stdout.trim_end_matches(['\r', '\n']).to_owned();
        Ok((a.code == 0 && storable(&token)).then_some(token))
    }

    fn set(&self, account: &str, token: &str) -> Result<(), String> {
        let a = self
            .0
            .run(
                "secret-tool",
                &[
                    "store",
                    "--label=asc access token",
                    "service",
                    SERVICE,
                    "account",
                    account,
                ],
                &[],
                token,
            )
            .map_err(|e| tool_error(&e))?;
        (a.code == 0)
            .then_some(())
            .ok_or_else(|| format!("secret-tool exited with {}", a.code))
    }

    fn delete(&self, account: &str) -> Result<(), String> {
        // Clearing nothing is not a failure.
        self.0
            .run(
                "secret-tool",
                &["clear", "service", SERVICE, "account", account],
                &[],
                "",
            )
            .map(drop)
            .map_err(|e| tool_error(&e))
    }
}

/// macOS: the login Keychain through `security`. The secret reaches `security` on standard
/// input (`security -i` reads commands there), never in an argument.
pub struct MacKeychain<'a>(pub &'a dyn Runner);

impl Native for MacKeychain<'_> {
    fn name(&self) -> &'static str {
        "macOS Keychain"
    }

    fn get(&self, account: &str) -> Result<Option<String>, String> {
        let a = self
            .0
            .run(
                "security",
                &["find-generic-password", "-s", SERVICE, "-a", account, "-w"],
                &[],
                "",
            )
            .map_err(|e| tool_error(&e))?;
        let token = a.stdout.trim_end_matches(['\r', '\n']).to_owned();
        Ok((a.code == 0 && storable(&token)).then_some(token))
    }

    fn set(&self, account: &str, token: &str) -> Result<(), String> {
        let command =
            format!("add-generic-password -U -s {SERVICE} -a \"{account}\" -w \"{token}\"\n");
        let a = self
            .0
            .run("security", &["-i"], &[], &command)
            .map_err(|e| tool_error(&e))?;
        (a.code == 0)
            .then_some(())
            .ok_or_else(|| format!("security exited with {}", a.code))
    }

    fn delete(&self, account: &str) -> Result<(), String> {
        self.0
            .run(
                "security",
                &["delete-generic-password", "-s", SERVICE, "-a", account],
                &[],
                "",
            )
            .map(drop)
            .map_err(|e| tool_error(&e))
    }
}

/// Windows: Credential Manager, through Windows PowerShell's `PasswordVault`. The account name
/// travels in the environment and the secret on standard input.
pub struct WindowsVault<'a>(pub &'a dyn Runner);

const VAULT_SCRIPT: &str = "$ErrorActionPreference = 'Stop'; \
[void][Windows.Security.Credentials.PasswordVault,Windows.Security.Credentials,ContentType=WindowsRuntime]; \
$v = New-Object Windows.Security.Credentials.PasswordVault; \
$r = 'asc'; $a = $env:ASC_KS_ACCOUNT; \
switch ($env:ASC_KS_OP) { \
'set' { $t = [Console]::In.ReadLine(); \
try { $v.Remove($v.Retrieve($r, $a)) } catch { }; \
$v.Add((New-Object Windows.Security.Credentials.PasswordCredential($r, $a, $t))) } \
'get' { try { $c = $v.Retrieve($r, $a); $c.RetrievePassword(); [Console]::Out.Write($c.Password) } catch { exit 3 } } \
'del' { try { $v.Remove($v.Retrieve($r, $a)) } catch { } } }";

impl WindowsVault<'_> {
    fn call(&self, op: &str, account: &str, stdin: &str) -> Result<Answer, String> {
        self.0
            .run(
                "powershell.exe",
                &[
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    VAULT_SCRIPT,
                ],
                &[("ASC_KS_OP", op), ("ASC_KS_ACCOUNT", account)],
                stdin,
            )
            .map_err(|e| tool_error(&e))
    }
}

impl Native for WindowsVault<'_> {
    fn name(&self) -> &'static str {
        "Windows Credential Manager"
    }

    fn get(&self, account: &str) -> Result<Option<String>, String> {
        let a = self.call("get", account, "")?;
        let token = a.stdout.trim_end_matches(['\r', '\n']).to_owned();
        Ok((a.code == 0 && storable(&token)).then_some(token))
    }

    fn set(&self, account: &str, token: &str) -> Result<(), String> {
        let a = self.call("set", account, &format!("{token}\n"))?;
        (a.code == 0)
            .then_some(())
            .ok_or_else(|| format!("powershell exited with {}", a.code))
    }

    fn delete(&self, account: &str) -> Result<(), String> {
        self.call("del", account, "").map(drop)
    }
}

// ------------------------------------------------------------------------------ the file

/// `credentials.json`: origin to token, `0600`, written whole through a temporary file.
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join(FILE_NAME),
        }
    }

    fn read(&self) -> BTreeMap<String, String> {
        let Ok(file) = std::fs::File::open(&self.path) else {
            return BTreeMap::new();
        };
        let mut text = String::new();
        if file.take(1 << 20).read_to_string(&mut text).is_err() {
            return BTreeMap::new();
        }
        serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("tokens")?.as_object().map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                        .collect()
                })
            })
            .unwrap_or_default()
    }

    fn write(&self, tokens: &BTreeMap<String, String>) -> Result<(), String> {
        let dir = self.path.parent().ok_or("no directory")?;
        std::fs::create_dir_all(dir).map_err(|e| tool_error(&e))?;
        let body = serde_json::json!({ "version": 1, "tokens": tokens }).to_string();
        let tmp = dir.join(format!("{FILE_NAME}.{}.tmp", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let result = options
            .open(&tmp)
            .and_then(|mut f| f.write_all(body.as_bytes()).and_then(|()| f.sync_all()))
            .and_then(|()| std::fs::rename(&tmp, &self.path));
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result.map_err(|e| tool_error(&e))
    }

    pub fn get(&self, account: &str) -> Option<String> {
        self.read().remove(account).filter(|t| storable(t))
    }

    pub fn set(&self, account: &str, token: &str) -> Result<(), String> {
        let mut all = self.read();
        all.insert(account.to_owned(), token.to_owned());
        self.write(&all)
    }

    pub fn delete(&self, account: &str) -> Result<(), String> {
        let mut all = self.read();
        if all.remove(account).is_some() {
            self.write(&all)?;
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------------ the store

/// Which store a call used or found a token in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    Keychain(&'static str),
    File,
}

impl std::fmt::Display for Place {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Keychain(name) => f.write_str(name),
            Self::File => f.write_str("a private file in the asc home directory"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Auto,
    Keychain,
    File,
    Off,
}

fn mode_of(value: Option<&str>) -> Mode {
    match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("off" | "none" | "never") => Mode::Off,
        Some("file") => Mode::File,
        Some("keychain") => Mode::Keychain,
        _ => Mode::Auto,
    }
}

/// The token store of one run.
pub struct KeyStore {
    mode: Mode,
    file: FileStore,
    runner: Box<dyn Runner>,
    os: &'static str,
}

impl KeyStore {
    /// The store the environment asks for, with its file in `dir` (the asc home).
    pub fn from_env(dir: &Path) -> Self {
        Self::with(
            mode_of(std::env::var(STORE_ENV).ok().as_deref()),
            dir,
            Box::new(Real),
            std::env::consts::OS,
        )
    }

    fn with(mode: Mode, dir: &Path, runner: Box<dyn Runner>, os: &'static str) -> Self {
        Self {
            mode,
            file: FileStore::new(dir),
            runner,
            os,
        }
    }

    /// Whether anything may be stored at all.
    pub fn enabled(&self) -> bool {
        self.mode != Mode::Off
    }

    fn with_native<T>(&self, f: impl FnOnce(&dyn Native) -> T) -> T {
        let r: &dyn Runner = &*self.runner;
        match self.os {
            "windows" => f(&WindowsVault(r)),
            "macos" => f(&MacKeychain(r)),
            _ => f(&SecretTool(r)),
        }
    }

    fn keychain_on(&self) -> bool {
        matches!(self.mode, Mode::Auto | Mode::Keychain)
    }

    fn file_on(&self) -> bool {
        matches!(self.mode, Mode::Auto | Mode::File)
    }

    /// The token filed for `account`, and where it was.
    pub fn get(&self, account: &str) -> Option<(String, Place)> {
        if !account_ok(account) {
            return None;
        }
        if self.keychain_on()
            && let Some(found) =
                self.with_native(|n| n.get(account).ok().flatten().map(|t| (t, n.name())))
        {
            return Some((found.0, Place::Keychain(found.1)));
        }
        if self.file_on() {
            return self.file.get(account).map(|t| (t, Place::File));
        }
        None
    }

    /// Files `token` for `account`: in the keychain, else in the file. Returns where.
    pub fn set(&self, account: &str, token: &str) -> Result<Place, String> {
        if !account_ok(account) || !storable(token) {
            return Err("the token cannot be stored".to_owned());
        }
        if !self.enabled() {
            return Err("credential storage is switched off".to_owned());
        }
        if self.keychain_on() {
            let done = self.with_native(|n| n.set(account, token).map(|()| n.name()));
            match done {
                Ok(name) => {
                    // A copy left in the file by an earlier fallback would go stale.
                    if self.file_on() {
                        let _ = self.file.delete(account);
                    }
                    return Ok(Place::Keychain(name));
                }
                Err(e) if self.mode == Mode::Keychain => return Err(e),
                Err(_) => {}
            }
        }
        self.file.set(account, token).map(|()| Place::File)
    }

    /// Forgets the token of `account` everywhere it may be. Returns whether one was found.
    pub fn delete(&self, account: &str) -> bool {
        if !account_ok(account) {
            return false;
        }
        let found = self.get(account).is_some();
        if self.keychain_on() {
            let _ = self.with_native(|n| n.delete(account));
        }
        if self.file_on() {
            let _ = self.file.delete(account);
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Records every call; answers from a map like the real tools.
    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<(String, Vec<String>, String)>>,
        fail: bool,
    }

    impl Runner for Fake {
        fn run(
            &self,
            program: &str,
            args: &[&str],
            env: &[(&str, &str)],
            stdin: &str,
        ) -> std::io::Result<Answer> {
            let mut all: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
            all.extend(env.iter().map(|(k, v)| format!("{k}={v}")));
            self.calls
                .lock()
                .unwrap()
                .push((program.to_owned(), all, stdin.to_owned()));
            if self.fail {
                return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
            }
            Ok(Answer {
                code: 0,
                stdout: "tok_from_tool\n".to_owned(),
            })
        }
    }

    const TOKEN: &str = "tok_Secret-123";
    const ACCOUNT: &str = "https://hub.example:8443";

    #[test]
    fn the_secret_goes_to_a_keychain_tool_on_stdin_never_in_an_argument() {
        for os in ["linux", "macos", "windows"] {
            let fake = Fake::default();
            let n: Box<dyn Native + '_> = match os {
                "windows" => Box::new(WindowsVault(&fake)),
                "macos" => Box::new(MacKeychain(&fake)),
                _ => Box::new(SecretTool(&fake)),
            };
            n.set(ACCOUNT, TOKEN).unwrap();
            let calls = fake.calls.lock().unwrap();
            let (program, args, stdin) = &calls[0];
            assert!(!program.contains(TOKEN), "{os}");
            assert!(args.iter().all(|a| !a.contains(TOKEN)), "{os}: {args:?}");
            assert!(stdin.contains(TOKEN), "{os}: the token is on stdin");
            if os == "windows" {
                assert!(args.iter().any(|a| a == "ASC_KS_OP=set"));
                assert!(
                    args.iter()
                        .any(|a| a == &format!("ASC_KS_ACCOUNT={ACCOUNT}"))
                );
            }
        }
    }

    #[test]
    fn a_tool_that_is_missing_falls_back_to_the_file_in_auto_mode() {
        let d = tempfile::tempdir().unwrap();
        let fake = Fake {
            fail: true,
            ..Fake::default()
        };
        let store = KeyStore::with(Mode::Auto, d.path(), Box::new(fake), "linux");
        assert_eq!(store.set(ACCOUNT, TOKEN).unwrap(), Place::File);
        assert_eq!(store.get(ACCOUNT), Some((TOKEN.to_owned(), Place::File)));
        assert!(store.delete(ACCOUNT));
        assert_eq!(store.get(ACCOUNT), None);
        // `keychain` mode does not fall back.
        let fake = Fake {
            fail: true,
            ..Fake::default()
        };
        let strict = KeyStore::with(Mode::Keychain, d.path(), Box::new(fake), "linux");
        assert!(strict.set(ACCOUNT, TOKEN).is_err());
    }

    #[test]
    fn the_file_is_private_and_holds_one_token_per_origin() {
        let d = tempfile::tempdir().unwrap();
        let f = FileStore::new(d.path());
        f.set("https://a.example", "one").unwrap();
        f.set("https://b.example", "two").unwrap();
        f.set("https://a.example", "three").unwrap();
        assert_eq!(f.get("https://a.example").as_deref(), Some("three"));
        assert_eq!(f.get("https://b.example").as_deref(), Some("two"));
        f.delete("https://a.example").unwrap();
        assert_eq!(f.get("https://a.example"), None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.path().join(FILE_NAME))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(
            std::fs::read_dir(d.path()).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")),
            "no temporary file is left"
        );
    }

    #[test]
    fn odd_tokens_and_accounts_are_refused_and_off_stores_nothing() {
        let d = tempfile::tempdir().unwrap();
        let store = KeyStore::with(Mode::File, d.path(), Box::new(Fake::default()), "linux");
        for bad in ["", "a b", "a\"b", "a\\b", "a'b", "a$b", "a\nb"] {
            assert!(store.set(ACCOUNT, bad).is_err(), "{bad:?}");
        }
        assert!(store.set("https://a b", TOKEN).is_err());
        let off = KeyStore::with(Mode::Off, d.path(), Box::new(Fake::default()), "linux");
        assert!(off.set(ACCOUNT, TOKEN).is_err());
        assert_eq!(off.get(ACCOUNT), None);
        assert_eq!(mode_of(Some("OFF")), Mode::Off);
        assert_eq!(mode_of(Some("file")), Mode::File);
        assert_eq!(mode_of(None), Mode::Auto);
    }

    #[test]
    fn accounts_are_origins() {
        let u = |s: &str| account_of(&url::Url::parse(s).unwrap()).unwrap();
        assert_eq!(u("https://User:pw@Hub.Example/o/r"), "https://hub.example");
        assert_eq!(u("http://127.0.0.1:8080/x"), "http://127.0.0.1:8080");
    }

    /// The real Windows Credential Manager, under an account of its own that is removed again.
    #[cfg(windows)]
    #[test]
    fn windows_credential_manager_round_trips() {
        let account = format!("http://asc-keystore-test.invalid:{}", std::process::id());
        let vault = WindowsVault(&Real);
        vault.set(&account, TOKEN).unwrap();
        assert_eq!(vault.get(&account).unwrap().as_deref(), Some(TOKEN));
        vault.set(&account, "tok_replaced").unwrap();
        assert_eq!(
            vault.get(&account).unwrap().as_deref(),
            Some("tok_replaced")
        );
        vault.delete(&account).unwrap();
        assert_eq!(vault.get(&account).unwrap(), None);
    }
}
