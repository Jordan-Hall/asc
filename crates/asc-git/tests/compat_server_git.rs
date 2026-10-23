//! `ServerGit` (U80, SPEC §25.8): the child's environment, configuration and limits, against
//! real git >= 2.40 through `RealGit`.
#![cfg(feature = "git-compat")]

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use asc_core::git::{ObjectKind, object_oid};
use asc_core::id::GitOid;
use asc_git::compat::pktline::{check_v2_advert, parse_service_prefix};
use asc_git::compat::view::VIEW_CONFIG_TEMPLATE;
use asc_git::compat::{
    ChildExit, FsckMode, GitLimits, GitProtocol, GitView, KillReason, MIGRATION_RELAXED,
    PackRequest, PackService, RefUpdate, ServerChild, ServerGit, ServerGitConfig, ServerGitError,
};
use asc_git::odb::Odb;
use asc_git::realgit::{MIN_VERSION, RealGit, Resolver};

const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

fn real_git() -> RealGit {
    RealGit::resolve(None, None).expect("real git >= 2.40")
}

struct Env {
    dir: tempfile::TempDir,
    server: ServerGit,
}

impl Env {
    fn new() -> Self {
        Self::with(GitLimits::default(), FsckMode::Strict)
    }

    fn with(limits: GitLimits, fsck: FsckMode) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let server = ServerGit::new(
            &real_git(),
            ServerGitConfig {
                hooks_dir: dir.path().join("hooks"),
                state_dir: dir.path().join("state"),
                limits,
                fsck,
            },
        )
        .expect("server git");
        Self { dir, server }
    }

    fn hooks(&self) -> PathBuf {
        self.dir.path().join("hooks")
    }

    /// A bare view holding one commit on `refs/heads/main`; returns the view and the commit.
    fn view_with_commit(&self) -> (PathBuf, GitOid) {
        let view = self.dir.path().join("view.git");
        self.server.init_bare(&view).expect("init");
        let commit = write_commit(&view);
        self.server
            .update_refs(
                &view,
                &[RefUpdate::Set {
                    name: "refs/heads/main".into(),
                    oid: commit,
                }],
            )
            .expect("set main");
        (view, commit)
    }
}

/// Writes the empty tree and a commit on it into `view`'s object database.
fn write_commit(view: &Path) -> GitOid {
    let odb = Odb::open(view).expect("odb");
    let tree = odb.write_loose(ObjectKind::Tree, b"").expect("tree");
    assert_eq!(tree.to_string(), EMPTY_TREE);
    let body = format!(
        "tree {EMPTY_TREE}\nauthor A <a@x> 1700000000 +0000\ncommitter A <a@x> 1700000000 +0000\n\nmsg\n"
    );
    let oid = odb
        .write_loose(ObjectKind::Commit, body.as_bytes())
        .expect("commit");
    assert_eq!(oid, object_oid(ObjectKind::Commit, body.as_bytes()));
    oid
}

fn read_all(child: &mut ServerChild) -> Vec<u8> {
    let mut out = Vec::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_end(&mut out)
        .expect("read");
    out
}

fn advertise(
    env: &Env,
    view: &Path,
    service: PackService,
    protocol: Option<&str>,
) -> (Vec<u8>, ChildExit) {
    let req = PackRequest {
        advertise_refs: true,
        protocol: protocol.map(|p| GitProtocol::parse(p).expect("valid protocol")),
    };
    let mut child = env.server.pack(service, view, &req).expect("spawn");
    child.close_stdin();
    let out = read_all(&mut child);
    (out, child.wait().expect("wait"))
}

#[test]
fn child_env_is_the_fixed_allow_list() {
    let env = Env::new();
    let proto = GitProtocol::parse("version=2").unwrap();
    let mut keys: Vec<String> = env
        .server
        .child_env(Some(&proto))
        .into_iter()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    keys.sort();
    let mut want = vec![
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_CONFIG_SYSTEM",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_PROTOCOL",
        "GIT_TERMINAL_PROMPT",
        "GIT_USER_AGENT",
        "HOME",
        "LC_ALL",
        "PATH",
    ];
    if cfg!(windows) {
        want.push("SystemRoot");
    }
    want.sort_unstable();
    assert_eq!(keys, want);
    assert!(
        !env.server
            .child_env(None)
            .iter()
            .any(|(k, _)| k == "GIT_PROTOCOL"),
        "no protocol, no GIT_PROTOCOL"
    );

    let args = env.server.config_args();
    let has = |k: &str, v: &str| args.iter().any(|(ak, av)| ak == k && av == v);
    assert!(has("protocol.allow", "never"));
    assert!(has("receive.fsckObjects", "true"));
    assert!(has("transfer.fsckObjects", "true"));
    assert!(has(
        "receive.maxInputSize",
        &(2u64 * 1024 * 1024 * 1024).to_string()
    ));
    assert!(has("uploadpack.allowFilter", "true"));
    assert!(has("uploadpack.allowAnySHA1InWant", "false"));
    assert!(has("uploadpack.allowReachableSHA1InWant", "false"));
    assert!(has("receive.advertisePushOptions", "true"));
    assert!(has("receive.advertiseAtomic", "true"));
    assert!(has("receive.denyNonFastForwards", "false"));
    assert!(has("receive.unpackLimit", "1"));
    assert!(has("receive.updateServerInfo", "false"));
    assert!(has("core.fsmonitor", "false"));
    assert!(has("core.sshCommand", "false"));
    assert!(has("gc.auto", "0"));
    assert!(has("receive.autogc", "false"));
    assert!(has("pack.threads", "2"));
    for hidden in ["refs/pull", "refs/users", "refs/snapshots"] {
        assert!(has("transfer.hideRefs", hidden), "{hidden}");
    }
    assert!(!args.iter().any(|(k, _)| k == "receive.certNonceSeed"));
    assert!(
        !args.iter().any(|(k, _)| k.starts_with("receive.fsck.")),
        "strict mode relaxes nothing"
    );
    let hooks = args.iter().find(|(k, _)| k == "core.hooksPath").unwrap();
    assert!(
        hooks
            .1
            .to_string_lossy()
            .replace('\\', "/")
            .ends_with("/hooks")
    );

    let migration = Env::with(GitLimits::default(), FsckMode::Migration);
    let relaxed: Vec<_> = migration
        .server
        .config_args()
        .into_iter()
        .filter(|(k, _)| k.starts_with("receive.fsck."))
        .collect();
    assert_eq!(relaxed.len(), MIGRATION_RELAXED.len());
    assert!(relaxed.iter().all(|(_, v)| v == "ignore"));
}

#[test]
fn the_migration_ids_are_real_fsck_message_ids() {
    // `git fsck` parses `fsck.<id>` and dies on an unknown id, so a typo in the list shows here.
    let env = Env::new();
    let (view, _) = env.view_with_commit();
    let mut cmd = real_git().command().expect("command");
    cmd.arg("--git-dir").arg(&view);
    for id in MIGRATION_RELAXED {
        cmd.arg("-c").arg(format!("fsck.{id}=ignore"));
    }
    let out = cmd.args(["fsck", "--strict"]).output().expect("fsck");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn advertisement_hides_private_refs_and_names_asc_as_the_agent() {
    let env = Env::new();
    let (view, commit) = env.view_with_commit();
    // Refs a plain git push could never put in the view, planted to prove the hide rules.
    let planted: Vec<_> = [
        "refs/pull/1/head",
        "refs/users/7/heads/x",
        "refs/snapshots/s",
    ]
    .into_iter()
    .map(|n| RefUpdate::Set {
        name: n.into(),
        oid: commit,
    })
    .collect();
    env.server.update_refs(&view, &planted).expect("plant");

    let (out, exit) = advertise(&env, &view, PackService::UploadPack, None);
    assert!(
        exit.status.success(),
        "{}",
        String::from_utf8_lossy(&exit.stderr)
    );
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("refs/heads/main"), "{text}");
    for hidden in ["refs/pull", "refs/users", "refs/snapshots"] {
        assert!(!text.contains(hidden), "{hidden} is advertised: {text}");
    }
    assert!(
        text.contains(&format!("agent={}", ServerGit::user_agent())),
        "{text}"
    );
    assert!(!text.contains("agent=git/"), "{text}");
    assert!(
        text.contains("filter"),
        "partial clone is advertised: {text}"
    );

    let (v2, exit) = advertise(&env, &view, PackService::UploadPack, Some("version=2"));
    assert!(exit.status.success());
    let advert = check_v2_advert(&v2).expect("v2 advertisement");
    assert_eq!(
        advert.agent.as_deref(),
        Some(ServerGit::user_agent().as_str())
    );
    assert_eq!(advert.object_format.as_deref(), Some("sha1"));
    assert!(advert.has("ls-refs") && advert.has("fetch"));
    assert!(
        parse_service_prefix(&v2).is_err(),
        "v2 carries no service prefix"
    );

    let (push, exit) = advertise(&env, &view, PackService::ReceivePack, None);
    assert!(
        exit.status.success(),
        "{}",
        String::from_utf8_lossy(&exit.stderr)
    );
    let push = String::from_utf8_lossy(&push);
    for cap in [
        "report-status-v2",
        "atomic",
        "push-options",
        "side-band-64k",
        "ofs-delta",
        "quiet",
    ] {
        assert!(push.contains(cap), "{cap} missing: {push}");
    }
    assert!(
        !push.contains("push-cert"),
        "signed pushes are not advertised: {push}"
    );
    assert!(!push.contains("refs/pull"));
}

#[test]
fn refs_round_trip_and_bad_names_never_reach_git() {
    let env = Env::new();
    let (view, commit) = env.view_with_commit();
    assert_eq!(
        env.server.list_refs(&view).unwrap(),
        vec![("refs/heads/main".to_owned(), commit)]
    );
    env.server.set_head(&view, "refs/heads/main").unwrap();
    assert_eq!(
        env.server.read_head(&view).unwrap().as_deref(),
        Some("refs/heads/main")
    );
    env.server
        .update_refs(
            &view,
            &[RefUpdate::Delete {
                name: "refs/heads/main".into(),
            }],
        )
        .unwrap();
    assert!(env.server.list_refs(&view).unwrap().is_empty());
    for bad in [
        "refs/heads/a b",
        "refs/heads/a\nb",
        "refs/heads/..",
        "",
        "refs/heads/x.lock",
    ] {
        let err = env
            .server
            .update_refs(&view, &[RefUpdate::Delete { name: bad.into() }])
            .unwrap_err();
        assert!(
            matches!(err, ServerGitError::BadRefName(_)),
            "{bad:?}: {err}"
        );
    }
    assert!(env.server.set_head(&view, "refs/tags/x").is_err());
    assert!(
        env.server
            .pack(
                PackService::UploadPack,
                Path::new("relative"),
                &PackRequest::default()
            )
            .is_err()
    );
}

#[test]
fn an_old_or_unknown_git_is_refused() {
    // A program that is not git reports no version, which the gate treats as too old.
    let fake: PathBuf = if cfg!(windows) {
        PathBuf::from(std::env::var_os("COMSPEC").expect("COMSPEC"))
    } else {
        PathBuf::from("/bin/sh")
    };
    let mut r = Resolver::from_process(None, None);
    r.env_override = Some(fake);
    let git = r.resolve().expect("the resolver trusts the override");
    let dir = tempfile::tempdir().unwrap();
    let err = ServerGit::new(
        &git,
        ServerGitConfig {
            hooks_dir: dir.path().join("hooks"),
            state_dir: dir.path().join("state"),
            limits: GitLimits::default(),
            fsck: FsckMode::Strict,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ServerGitError::TooOld { .. }), "{err}");
}

#[test]
fn the_real_git_passes_the_gate_and_bad_configuration_is_refused() {
    let env = Env::new();
    assert!(env.server.version() >= MIN_VERSION);
    let dir = tempfile::tempdir().unwrap();
    let err = ServerGit::new(
        &real_git(),
        ServerGitConfig {
            hooks_dir: PathBuf::from("hooks"),
            state_dir: dir.path().join("state"),
            limits: GitLimits::default(),
            fsck: FsckMode::Strict,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ServerGitError::NotAbsolute(_)), "{err}");
    let err = ServerGit::new(
        &real_git(),
        ServerGitConfig {
            hooks_dir: dir.path().join("h"),
            state_dir: dir.path().join("s"),
            limits: GitLimits {
                child_memory: 0,
                ..GitLimits::default()
            },
            fsck: FsckMode::Strict,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ServerGitError::Limits(_)), "{err}");
}

/// Waits for a child whose stdin stays open (so only the limits can end it).
fn wait_with_open_stdin(mut child: ServerChild) -> ChildExit {
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let (tx, rx) = mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let _held = (stdin, stdout);
        let _ = rx.recv();
    });
    let exit = child.wait().expect("wait");
    let _ = tx.send(());
    holder.join().expect("holder");
    exit
}

#[test]
fn a_silent_child_dies_at_the_idle_limit_and_a_busy_one_at_the_deadline() {
    let limits = GitLimits {
        request_timeout: Duration::from_secs(3),
        idle_timeout: Duration::from_secs(1),
        ..GitLimits::default()
    };
    let env = Env::with(limits, FsckMode::Strict);
    let (view, _) = env.view_with_commit();

    // receive-pack waits for a request that never comes.
    let started = Instant::now();
    let child = env
        .server
        .pack(PackService::ReceivePack, &view, &PackRequest::default())
        .expect("spawn");
    let exit = wait_with_open_stdin(child);
    assert_eq!(exit.killed, Some(KillReason::Idle));
    assert!(!exit.status.success());
    assert!(started.elapsed() < Duration::from_secs(10));

    // The same child kept busy is stopped by the wall-clock deadline instead.
    let started = Instant::now();
    let child = env
        .server
        .pack(PackService::ReceivePack, &view, &PackRequest::default())
        .expect("spawn");
    let act = child.activity();
    let ticker = std::thread::spawn(move || {
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(100));
            act.touch();
        }
    });
    let exit = wait_with_open_stdin(child);
    ticker.join().unwrap();
    assert_eq!(exit.killed, Some(KillReason::Deadline));
    assert!(started.elapsed() < Duration::from_secs(10));
}

/// Environment variables a hostile parent could carry. Each must be invisible to the child.
fn canaries(decoy: &Path) -> Vec<(&'static str, String)> {
    let home = decoy.join("home");
    let evil_config = decoy.join("evil.gitconfig");
    let p = |p: &Path| p.to_string_lossy().replace('\\', "/");
    vec![
        ("GIT_DIR", p(&decoy.join("no-such-git-dir"))),
        ("GIT_WORK_TREE", p(decoy)),
        ("GIT_CONFIG_COUNT", "1".into()),
        ("GIT_CONFIG_KEY_0", "core.hooksPath".into()),
        ("GIT_CONFIG_VALUE_0", p(&decoy.join("evil-hooks"))),
        ("GIT_EXEC_PATH", p(&decoy.join("no-exec-path"))),
        ("GIT_SSH_COMMAND", "canary-ssh".into()),
        ("GIT_ASKPASS", "canary-askpass".into()),
        ("GIT_PROTOCOL", "version=9".into()),
        ("GIT_AUTHOR_NAME", "canary".into()),
        ("GIT_CONFIG_GLOBAL", p(&evil_config)),
        ("GIT_CONFIG_SYSTEM", p(&evil_config)),
        ("HOME", p(&home)),
        ("USERPROFILE", p(&home)),
        ("XDG_CONFIG_HOME", p(&decoy.join("xdg"))),
        ("ASC_CANARY_SECRET", "s3cret".into()),
    ]
}

/// Runs inside the child test process started by [`the_child_sees_only_the_allow_list`]: the
/// canary variables are set by the parent, so no test in this binary changes its own environment.
#[test]
fn canary_child() {
    let Some(root) = std::env::var_os("ASC_CANARY_ROOT").map(PathBuf::from) else {
        return; // an ordinary run: the parent test does the work
    };
    let marker_evil = root.join("evil-ran");
    let hook_out = root.join("hook-env.txt");
    let fwd = |p: &Path| p.to_string_lossy().replace('\\', "/");

    // A hostile global config, home config and hook directory, all pointing at one marker.
    let evil_hooks = root.join("evil-hooks");
    fs::create_dir_all(&evil_hooks).unwrap();
    let evil_hook = format!("#!/bin/sh\necho ran > \"{}\"\n", fwd(&marker_evil));
    for hook in [
        "reference-transaction",
        "pre-receive",
        "post-receive",
        "update",
    ] {
        fs::write(evil_hooks.join(hook), &evil_hook).unwrap();
    }
    fs::write(
        root.join("evil.gitconfig"),
        format!(
            "[core]\n\thooksPath = {}\n[alias]\n\tupdate-ref = !echo hijacked\n",
            fwd(&evil_hooks)
        ),
    )
    .unwrap();
    fs::create_dir_all(root.join("home")).unwrap();
    fs::write(
        root.join("home").join(".gitconfig"),
        format!("[core]\n\thooksPath = {}\n", fwd(&evil_hooks)),
    )
    .unwrap();

    let env = Env::new();
    // The asc-owned hook dumps the environment git hands it.
    fs::write(
        env.hooks().join("reference-transaction"),
        format!("#!/bin/sh\nenv > \"{}\"\ncat > /dev/null\n", fwd(&hook_out)),
    )
    .unwrap();
    // The view's own hooks directory is hostile too.
    let view = env.dir.path().join("view.git");
    env.server.init_bare(&view).unwrap();
    fs::create_dir_all(view.join("hooks")).unwrap();
    for hook in ["reference-transaction", "pre-receive", "post-receive"] {
        fs::write(view.join("hooks").join(hook), &evil_hook).unwrap();
    }
    let commit = write_commit(&view);
    env.server
        .update_refs(
            &view,
            &[RefUpdate::Set {
                name: "refs/heads/main".into(),
                oid: commit,
            }],
        )
        .unwrap();

    assert!(
        !marker_evil.exists(),
        "a hook from the parent's config or the view ran"
    );
    let dump = fs::read_to_string(&hook_out).expect("the asc-owned hook ran");
    let vars: Vec<(&str, &str)> = dump.lines().filter_map(|l| l.split_once('=')).collect();
    let get = |k: &str| {
        vars.iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map(|(_, v)| *v)
    };
    for (name, _) in canaries(&root) {
        match name {
            // Git and the shell set some of these themselves; they must not carry the canary.
            "GIT_DIR" | "GIT_EXEC_PATH" | "GIT_CONFIG_GLOBAL" | "GIT_CONFIG_SYSTEM" | "HOME"
            | "USERPROFILE" => {
                let v = get(name).unwrap_or("");
                assert!(
                    !v.contains("no-such-git-dir")
                        && !v.contains("no-exec-path")
                        && !v.contains("evil.gitconfig")
                        && !v.contains("canary"),
                    "{name}={v}"
                );
                if name == "HOME" || name == "USERPROFILE" {
                    assert!(!v.contains("home") || v.contains("state"), "{name}={v}");
                }
            }
            other => assert!(get(other).is_none(), "{other} reached the child: {dump}"),
        }
    }
    assert_eq!(get("GIT_TERMINAL_PROMPT"), Some("0"));
    assert_eq!(get("GIT_NO_REPLACE_OBJECTS"), Some("1"));
    assert_eq!(get("GIT_CONFIG_NOSYSTEM"), Some("1"));
    assert_eq!(
        get("GIT_USER_AGENT"),
        Some(ServerGit::user_agent().as_str())
    );
    assert_eq!(get("LC_ALL"), Some("C"));
    assert!(get("GIT_CONFIG_GLOBAL").is_some_and(|v| v.ends_with("empty.gitconfig")));
    // Nothing else with a GIT_ prefix, beyond what git itself adds to a hook.
    const GIT_ADDED: &[&str] = &[
        "GIT_DIR",
        "GIT_EXEC_PATH",
        "GIT_TERMINAL_PROMPT",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_USER_AGENT",
        "GIT_REFLOG_ACTION",
        "GIT_PREFIX",
        "GIT_INTERNAL_SUPER_PREFIX",
        "GIT_CONFIG_PARAMETERS",
        "GIT_REFLOG_ACTION",
    ];
    for (name, _) in &vars {
        if name.to_ascii_uppercase().starts_with("GIT_") {
            assert!(
                GIT_ADDED.iter().any(|a| a.eq_ignore_ascii_case(name)),
                "unexpected {name} in the child: {dump}"
            );
        }
    }
    fs::write(root.join("canary-ok"), b"ok").unwrap();
}

#[test]
fn the_child_sees_only_the_allow_list() {
    let root = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(std::env::current_exe().unwrap()); // lint:allow-raw-git
    cmd.args(["--exact", "canary_child", "--test-threads=1", "--nocapture"]);
    cmd.env("ASC_CANARY_ROOT", root.path());
    for (k, v) in canaries(root.path()) {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run the canary child");
    assert!(
        out.status.success() && root.path().join("canary-ok").exists(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_hostile_view_config_is_refused_then_rewritten_and_never_runs_code() {
    let env = Env::new();
    let (view, commit) = env.view_with_commit();
    let marker = env.dir.path().join("ran");
    let fwd = |p: &Path| p.to_string_lossy().replace('\\', "/");
    let script = env.dir.path().join("evil.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh
echo ran > \"{}\"
cat > /dev/null
",
            fwd(&marker)
        ),
    )
    .unwrap();
    let evil_hooks = env.dir.path().join("evil-hooks");
    fs::create_dir_all(&evil_hooks).unwrap();
    for hook in ["reference-transaction", "pre-receive", "post-receive"] {
        fs::copy(&script, evil_hooks.join(hook)).unwrap();
        fs::create_dir_all(view.join("hooks")).unwrap();
        fs::copy(&script, view.join("hooks").join(hook)).unwrap();
    }
    let hostile = [
        ("core", "hooksPath", fwd(&evil_hooks)),
        ("core", "fsmonitor", fwd(&script)),
        ("core", "sshCommand", fwd(&script)),
        ("uploadpack", "packObjectsHook", fwd(&script)),
        ("uploadpack", "allowAnySHA1InWant", "true".into()),
        ("receive", "denyCurrentBranch", "ignore".into()),
        ("receive", "certNonceSeed", "x".into()),
        ("receive", "fsckObjects", "false".into()),
        ("protocol", "allow", "always".into()),
        ("include", "path", fwd(&script)),
    ];
    let original = fs::read_to_string(view.join("config")).unwrap();
    for (section, key, value) in &hostile {
        fs::write(
            view.join("config"),
            format!(
                "{original}
[{section}]
	{key} = {value}
"
            ),
        )
        .unwrap();
        // ServerGit itself refuses to run on it.
        let err = env
            .server
            .pack(PackService::UploadPack, &view, &PackRequest::default())
            .map(|_| ())
            .unwrap_err();
        assert!(
            matches!(err, ServerGitError::UntrustedConfig { .. }),
            "{key}: {err}"
        );
        let err = env.server.list_refs(&view).unwrap_err();
        assert!(
            matches!(err, ServerGitError::UntrustedConfig { .. }),
            "{key}: {err}"
        );
        fs::write(view.join("config"), &original).unwrap();
    }
    // A hostile file that fails to parse is refused too.
    fs::write(
        view.join("config"),
        b"[core
broken",
    )
    .unwrap();
    assert!(env.server.list_refs(&view).is_err());
    fs::write(view.join("config"), &original).unwrap();

    // The view rewrites it from the template before any child starts: the config goes back to the template and git runs, with no hook executing.
    fs::write(
        view.join("config"),
        format!(
            "{original}
[core]
	hooksPath = {}
[receive]
	certNonceSeed = x
",
            fwd(&evil_hooks)
        ),
    )
    .unwrap();
    let root = view.parent().unwrap();
    // `GitView::open` derives `<view_dir>/<id>.git`; place the hostile repository there.
    let id = 7;
    let placed = root.join(format!("{id}.git"));
    fs::rename(&view, &placed).unwrap();
    let gv = GitView::open(&env.server, root, id).unwrap();
    assert_eq!(
        fs::read_to_string(placed.join("config")).unwrap(),
        VIEW_CONFIG_TEMPLATE
    );
    let other = write_second_commit(&placed, commit);
    env.server
        .update_refs(
            &placed,
            &[RefUpdate::Set {
                name: "refs/heads/other".into(),
                oid: other,
            }],
        )
        .unwrap();
    let mut child = gv
        .spawn_pack(
            PackService::ReceivePack,
            &PackRequest {
                advertise_refs: true,
                protocol: None,
            },
        )
        .unwrap();
    child.close_stdin();
    let push = String::from_utf8_lossy(&read_all(&mut child)).into_owned();
    assert!(child.wait().unwrap().status.success());
    assert!(!push.contains("push-cert"), "{push}");
    let (out, exit) = advertise(&env, &placed, PackService::UploadPack, None);
    assert!(exit.status.success());
    let out = String::from_utf8_lossy(&out);
    assert!(
        !out.contains("allow-tip-sha1-in-want") && !out.contains("allow-reachable-sha1-in-want")
    );
    assert!(
        !marker.exists(),
        "a hook, hook path or command from the view ran"
    );
}

fn write_second_commit(view: &Path, parent: GitOid) -> GitOid {
    let odb = Odb::open(view).expect("odb");
    let body = format!(
        "tree {EMPTY_TREE}\nparent {parent}\nauthor A <a@x> 1700000001 +0000\ncommitter A <a@x> 1700000001 +0000\n\nsecond\n"
    );
    odb.write_loose(ObjectKind::Commit, body.as_bytes())
        .expect("commit")
}
