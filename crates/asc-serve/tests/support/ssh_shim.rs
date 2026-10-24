//! A test-only `GIT_SSH_COMMAND` program built on the `russh` client, so real git can be the
//! client without OpenSSH (PLAN U87).
//!
//! The test binary is the program. `Shim::command` writes a small wrapper script; git runs it as
//! `sh <script> [-o SendEnv=GIT_PROTOCOL] [-p port] [user@]host <command>`; the script re-executes
//! this test binary on [`asc_ssh_shim_entry`], which connects, logs in with the key in the
//! environment, runs the command on a session channel and joins it to its own stdin, stdout,
//! stderr and exit code. libtest prints a two-line header on stdout before the test starts, which
//! the script strips; the entry ends the process itself, so nothing is printed after the data.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::ChannelMsg;
use russh::client;
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg};

const ENV_ARGS: &str = "ASC_SHIM_ARGS";
const ENV_KEY: &str = "ASC_SHIM_KEY";
/// The test that is the shim; it must carry this name.
pub const ENTRY_TEST: &str = "asc_ssh_shim_entry";

/// One `GIT_SSH_COMMAND` for one key.
pub struct Shim {
    pub command: String,
}

impl Shim {
    /// Writes the wrapper for `key_pem` into `dir`. `label` keeps several shims apart.
    pub fn new(dir: &Path, label: &str, key_pem: &str) -> Self {
        let fwd = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let key_file = dir.join(format!("{label}.key"));
        std::fs::write(&key_file, key_pem.trim_start()).unwrap();
        let script: PathBuf = dir.join(format!("{label}.sh"));
        let exe = std::env::current_exe().unwrap();
        let body = format!(
            "#!/bin/sh\n\
             st=\"{status}.$$\"\n\
             {{ {ENV_ARGS}=\"$(printf '%s\\n' \"$@\")\" {ENV_KEY}='{key}' '{exe}' --exact {ENTRY_TEST} --nocapture --format terse --test-threads=1; echo $? > \"$st\"; }} | {{ read -r _h1; read -r _h2; cat; }}\n\
             code=$(cat \"$st\" 2>/dev/null || echo 255)\n\
             rm -f \"$st\"\n\
             exit \"$code\"\n",
            status = fwd(&dir.join(format!("{label}.status"))),
            key = fwd(&key_file),
            exe = fwd(&exe),
        );
        std::fs::write(&script, body).unwrap();
        Self {
            command: format!("sh '{}'", fwd(&script)),
        }
    }
}

/// The body of the `asc_ssh_shim_entry` test: returns at once outside a shim run, otherwise ends
/// the process with the remote command's exit status.
pub fn entry() {
    let Some(args) = std::env::var(ENV_ARGS).ok() else {
        return;
    };
    let key = std::env::var(ENV_KEY).expect("shim key");
    let code = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(run(&args, &key))
    {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ssh shim: {e}");
            255
        }
    };
    let _ = std::io::stdout().flush();
    std::process::exit(i32::try_from(code).unwrap_or(255));
}

struct Accept;

impl client::Handler for Accept {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

async fn run(args: &str, key_file: &str) -> Result<u32, String> {
    let argv: Vec<&str> = args.lines().collect();
    let mut port = 22_u16;
    let mut send_env = false;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        match argv[i] {
            "-o" => {
                send_env |= argv
                    .get(i + 1)
                    .is_some_and(|o| o.contains("SendEnv=GIT_PROTOCOL"));
                i += 2;
            }
            "-p" => {
                port = argv
                    .get(i + 1)
                    .and_then(|p| p.parse().ok())
                    .ok_or_else(|| format!("bad -p in {argv:?}"))?;
                i += 2;
            }
            "-4" | "-6" | "-x" | "-T" | "-a" | "-q" | "-v" => i += 1,
            other => {
                rest.push(other);
                i += 1;
            }
        }
    }
    let [target, command] = rest[..] else {
        return Err(format!("unexpected arguments {argv:?}"));
    };
    let (user, host) = target.split_once('@').unwrap_or(("git", target));
    let pem = std::fs::read_to_string(key_file).map_err(|e| e.to_string())?;
    let key = PrivateKey::from_openssh(pem.trim()).map_err(|e| e.to_string())?;

    let mut handle = client::connect(
        Arc::new(client::Config::default()),
        (host.trim_matches(['[', ']']), port),
        Accept,
    )
    .await
    .map_err(|e| format!("connect: {e}"))?;
    let auth = handle
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), None))
        .await
        .map_err(|e| format!("auth: {e}"))?;
    if !auth.success() {
        return Err(format!("{user}@{host}: Permission denied (publickey)."));
    }
    let mut ch = handle
        .channel_open_session()
        .await
        .map_err(|e| format!("session: {e}"))?;
    if let (true, Ok(v)) = (send_env, std::env::var("GIT_PROTOCOL")) {
        let _ = ch.set_env(false, "GIT_PROTOCOL", v).await;
    }
    ch.exec(true, command.as_bytes().to_vec())
        .await
        .map_err(|e| format!("exec: {e}"))?;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = vec![0_u8; 32 * 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut stdin_open = true;
    let mut exit = None;
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    loop {
        tokio::select! {
            chunk = rx.recv(), if stdin_open => match chunk {
                Some(c) => {
                    if ch.data_bytes(c).await.is_err() {
                        stdin_open = false;
                    }
                }
                None => {
                    stdin_open = false;
                    let _ = ch.eof().await;
                }
            },
            msg = ch.wait() => match msg {
                Some(ChannelMsg::Data { data })
                    if out.write_all(&data).and_then(|()| out.flush()).is_err() =>
                {
                    break;
                }
                Some(ChannelMsg::Data { .. }) => {}
                Some(ChannelMsg::ExtendedData { data, .. }) => {
                    let _ = err.write_all(&data);
                    let _ = err.flush();
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => exit = Some(exit_status),
                Some(ChannelMsg::Close) | None => break,
                _ => {}
            },
        }
    }
    Ok(exit.unwrap_or(255))
}
