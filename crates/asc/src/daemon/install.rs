//! Registering the daemon with the operating system (SPEC §9.1, "Start").
//!
//! Windows gets a per-user logon task created with `schtasks.exe /Create /XML` from a generated
//! task definition (no COM, no `unsafe`); the task restarts the daemon on failure 3 times, 10 s
//! apart. Linux gets a systemd user unit with `Restart=on-failure` and `RestartSec=2`. The
//! generators are pure and golden-tested; the commands run through [`Runner`] so the round trip
//! is testable without touching the host.
//!
//! Every value that reaches a generated file (the executable path, the home directory) is
//! untrusted: control characters are rejected and each format's own escaping is applied.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Name of the Windows logon task.
pub const TASK_NAME: &str = "AscDaemon";
/// Name of the systemd user unit.
pub const UNIT_NAME: &str = "asc-daemon.service";

/// Longest executable or home path accepted.
const MAX_PATH_LEN: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("install: {0}")]
    Invalid(String),
    #[error("install: {0}")]
    Io(#[from] io::Error),
    #[error("install: `{cmd}` failed: {detail}")]
    Command { cmd: String, detail: String },
}

/// Which service manager to register with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    WindowsTask,
    SystemdUser,
}

impl Platform {
    /// The native manager of the build target, if there is one.
    pub fn current() -> Option<Self> {
        if cfg!(windows) {
            Some(Self::WindowsTask)
        } else if cfg!(target_os = "linux") {
            Some(Self::SystemdUser)
        } else {
            None
        }
    }
}

/// What to register: the daemon executable and the `ASC_HOME` it serves.
#[derive(Clone, Debug)]
pub struct Target {
    pub exe: PathBuf,
    pub home: PathBuf,
}

/// Runs an external program; faked in tests.
pub trait Runner {
    /// Run `program args`, returning its stdout. A non-zero exit is an error carrying stderr.
    fn run(&self, program: &str, args: &[String]) -> Result<String, InstallError>;
}

/// Runs real processes with no inherited stdin.
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<String, InstallError> {
        let out = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(InstallError::Io)?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        let mut detail = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        detail.truncate(detail.floor_char_boundary(512));
        Err(InstallError::Command {
            cmd: format!("{program} {}", args.first().map(String::as_str).unwrap_or("")),
            detail: format!("{} {detail}", out.status),
        })
    }
}

fn path_text(p: &Path, what: &str) -> Result<String, InstallError> {
    let s = p
        .to_str()
        .ok_or_else(|| InstallError::Invalid(format!("{what} is not valid UTF-8")))?;
    if s.is_empty() || s.len() > MAX_PATH_LEN {
        return Err(InstallError::Invalid(format!("{what} has an unusable length")));
    }
    if s.chars().any(char::is_control) {
        return Err(InstallError::Invalid(format!("{what} contains a control character")));
    }
    if !p.is_absolute() {
        return Err(InstallError::Invalid(format!("{what} is not absolute")));
    }
    Ok(s.to_owned())
}

fn xml_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c => o.push(c),
        }
    }
    o
}

/// The Task Scheduler definition: run `asc daemon run` at the user's logon, hidden, restarting
/// on failure 3 times at 10 s.
pub fn windows_task_xml(t: &Target) -> Result<String, InstallError> {
    let exe = path_text(&t.exe, "executable")?;
    let home = path_text(&t.home, "home")?;
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>asc daemon</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Hidden>true</Hidden>
    <RestartOnFailure>
      <Interval>PT10S</Interval>
      <Count>3</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{}</Command>
      <Arguments>daemon run</Arguments>
      <WorkingDirectory>{}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        xml_escape(&exe),
        xml_escape(&home)
    ))
}

/// `schtasks.exe /XML` reads UTF-16LE with a byte-order mark.
pub fn utf16le_with_bom(s: &str) -> Vec<u8> {
    let mut v = vec![0xFF, 0xFE];
    for u in s.encode_utf16() {
        v.extend_from_slice(&u.to_le_bytes());
    }
    v
}

/// Quote one systemd word: `"` and `\` escaped, `%` and `$` doubled so no specifier or
/// variable expands.
fn systemd_word(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '%' => o.push_str("%%"),
            '$' => o.push_str("$$"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// The systemd user unit.
pub fn systemd_unit(t: &Target) -> Result<String, InstallError> {
    let exe = path_text(&t.exe, "executable")?;
    let home = path_text(&t.home, "home")?;
    Ok(format!(
        "[Unit]\nDescription=asc daemon\n\n[Service]\nType=simple\nEnvironment={}\nExecStart={} daemon run\nRestart=on-failure\nRestartSec=2\n\n[Install]\nWantedBy=default.target\n",
        systemd_word(&format!("ASC_HOME={home}")),
        systemd_word(&exe),
    ))
}

/// Where the unit file lives: `$XDG_CONFIG_HOME/systemd/user` or `~/.config/systemd/user`.
pub fn unit_dir(env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Result<PathBuf, InstallError> {
    let abs = |k: &str| env(k).map(PathBuf::from).filter(|p| p.is_absolute());
    if let Some(x) = abs("XDG_CONFIG_HOME") {
        return Ok(x.join("systemd").join("user"));
    }
    abs("HOME")
        .map(|h| h.join(".config").join("systemd").join("user"))
        .ok_or_else(|| InstallError::Invalid("no absolute HOME".into()))
}

fn is_symlink(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

fn write_unit(dir: &Path, text: &str) -> Result<PathBuf, InstallError> {
    // The unit directory is `<base>/systemd/user`; its two levels must not be symlinks.
    for p in dir.ancestors().take(3) {
        if is_symlink(p) {
            return Err(InstallError::Invalid(format!("{} is a symlink", p.display())));
        }
    }
    fs::create_dir_all(dir)?;
    for p in dir.ancestors().take(3) {
        if is_symlink(p) {
            return Err(InstallError::Invalid(format!("{} is a symlink", p.display())));
        }
    }
    let file = dir.join(UNIT_NAME);
    if is_symlink(&file) {
        return Err(InstallError::Invalid(format!("{} is a symlink", file.display())));
    }
    let tmp = dir.join(format!(".{UNIT_NAME}.tmp"));
    let _ = fs::remove_file(&tmp);
    fs::write(&tmp, text)?;
    fs::rename(&tmp, &file)?;
    Ok(file)
}

/// Register the daemon. `unit_dir` is only used for [`Platform::SystemdUser`] and `scratch` is
/// where the Windows task XML is staged.
pub fn install(
    platform: Platform,
    t: &Target,
    unit_dir: &Path,
    scratch: &Path,
    run: &dyn Runner,
) -> Result<(), InstallError> {
    match platform {
        Platform::WindowsTask => {
            let xml = windows_task_xml(t)?;
            fs::create_dir_all(scratch)?;
            let file = scratch.join("asc-daemon-task.xml");
            fs::write(&file, utf16le_with_bom(&xml))?;
            let r = run.run(
                "schtasks.exe",
                &[
                    "/Create".into(),
                    "/TN".into(),
                    TASK_NAME.into(),
                    "/XML".into(),
                    file.to_string_lossy().into_owned(),
                    "/F".into(),
                ],
            );
            let _ = fs::remove_file(&file);
            r.map(|_| ())
        }
        Platform::SystemdUser => {
            let text = systemd_unit(t)?;
            write_unit(unit_dir, &text)?;
            run.run("systemctl", &["--user".into(), "daemon-reload".into()])?;
            run.run("systemctl", &["--user".into(), "enable".into(), UNIT_NAME.into()])?;
            Ok(())
        }
    }
}

/// Remove the registration. Absence is success.
pub fn uninstall(platform: Platform, unit_dir: &Path, run: &dyn Runner) -> Result<(), InstallError> {
    match platform {
        Platform::WindowsTask => {
            if run
                .run("schtasks.exe", &["/Query".into(), "/TN".into(), TASK_NAME.into()])
                .is_err()
            {
                return Ok(());
            }
            run.run(
                "schtasks.exe",
                &["/Delete".into(), "/TN".into(), TASK_NAME.into(), "/F".into()],
            )
            .map(|_| ())
        }
        Platform::SystemdUser => {
            let file = unit_dir.join(UNIT_NAME);
            if fs::symlink_metadata(&file).is_err() {
                return Ok(());
            }
            let _ = run.run("systemctl", &["--user".into(), "disable".into(), UNIT_NAME.into()]);
            fs::remove_file(&file)?;
            run.run("systemctl", &["--user".into(), "daemon-reload".into()])?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Fake(RefCell<Vec<String>>, bool);
    impl Runner for Fake {
        fn run(&self, program: &str, args: &[String]) -> Result<String, InstallError> {
            self.0.borrow_mut().push(format!("{program} {}", args.join(" ")));
            if self.1 && args.first().map(String::as_str) == Some("/Query") {
                return Err(InstallError::Command {
                    cmd: "q".into(),
                    detail: "none".into(),
                });
            }
            Ok(String::new())
        }
    }

    #[cfg(windows)]
    #[test]
    fn task_xml_matches_golden() {
        let t = Target {
            exe: "C:\\Program Files\\asc\\asc.exe".into(),
            home: "C:\\Users\\a&b\\.asc".into(),
        };
        let xml = windows_task_xml(&t).unwrap();
        assert!(xml.contains("<Command>C:\\Program Files\\asc\\asc.exe</Command>"));
        assert!(xml.contains("<Arguments>daemon run</Arguments>"));
        assert!(xml.contains("<WorkingDirectory>C:\\Users\\a&amp;b\\.asc</WorkingDirectory>"));
        assert!(xml.contains("<Interval>PT10S</Interval>\n      <Count>3</Count>"));
        assert!(xml.contains("<LogonTrigger>"));
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
    }

    #[cfg(unix)]
    #[test]
    fn unit_matches_golden() {
        let t = Target {
            exe: "/opt/asc dir/asc".into(),
            home: "/home/a%b/.asc".into(),
        };
        assert_eq!(
            systemd_unit(&t).unwrap(),
            "[Unit]\nDescription=asc daemon\n\n[Service]\nType=simple\nEnvironment=\"ASC_HOME=/home/a%%b/.asc\"\nExecStart=\"/opt/asc dir/asc\" daemon run\nRestart=on-failure\nRestartSec=2\n\n[Install]\nWantedBy=default.target\n"
        );
    }

    #[test]
    fn rejects_hostile_paths() {
        let root = if cfg!(windows) { "C:\\" } else { "/" };
        let t = Target {
            exe: PathBuf::from(format!("{root}x\ny")),
            home: PathBuf::from(format!("{root}h")),
        };
        assert!(windows_task_xml(&t).is_err());
        assert!(systemd_unit(&t).is_err());
        let r = Target {
            exe: PathBuf::from("relative/asc"),
            home: PathBuf::from(format!("{root}h")),
        };
        assert!(systemd_unit(&r).is_err());
    }

    #[test]
    fn quoting_helpers() {
        assert_eq!(xml_escape("<&'\">"), "&lt;&amp;&apos;&quot;&gt;");
        assert_eq!(systemd_word("a$b\"c"), "\"a$$b\\\"c\"");
        assert_eq!(&utf16le_with_bom("a")[..], &[0xFF, 0xFE, b'a', 0]);
    }

    #[test]
    fn unit_dir_prefers_xdg() {
        let abs = if cfg!(windows) { "C:\\x" } else { "/x" };
        let env = |k: &str| (k == "XDG_CONFIG_HOME").then(|| abs.into());
        assert_eq!(unit_dir(&env).unwrap(), Path::new(abs).join("systemd").join("user"));
        let none = |_: &str| None;
        assert!(unit_dir(&none).is_err());
        let rel = |_: &str| Some("rel".into());
        assert!(unit_dir(&rel).is_err());
    }

    #[test]
    fn systemd_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("systemd").join("user");
        let t = Target {
            exe: d.path().join("asc"),
            home: d.path().join("h"),
        };
        let f = Fake(RefCell::new(vec![]), false);
        install(Platform::SystemdUser, &t, &dir, d.path(), &f).unwrap();
        assert!(dir.join(UNIT_NAME).is_file());
        install(Platform::SystemdUser, &t, &dir, d.path(), &f).unwrap();
        uninstall(Platform::SystemdUser, &dir, &f).unwrap();
        assert!(!dir.join(UNIT_NAME).exists());
        uninstall(Platform::SystemdUser, &dir, &f).unwrap();
        let calls = f.0.borrow();
        assert!(calls.iter().any(|c| c == "systemctl --user enable asc-daemon.service"));
        assert!(calls.iter().any(|c| c == "systemctl --user disable asc-daemon.service"));
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_unit() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("systemd").join("user");
        fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(d.path().join("victim"), dir.join(UNIT_NAME)).unwrap();
        let t = Target {
            exe: d.path().join("asc"),
            home: d.path().join("h"),
        };
        let f = Fake(RefCell::new(vec![]), false);
        assert!(install(Platform::SystemdUser, &t, &dir, d.path(), &f).is_err());
        assert!(!d.path().join("victim").exists());
    }

    #[test]
    fn windows_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let t = Target {
            exe: d.path().join("asc.exe"),
            home: d.path().join("h"),
        };
        let f = Fake(RefCell::new(vec![]), false);
        install(Platform::WindowsTask, &t, d.path(), d.path(), &f).unwrap();
        uninstall(Platform::WindowsTask, d.path(), &f).unwrap();
        let calls = f.0.borrow();
        assert!(calls[0].starts_with("schtasks.exe /Create /TN AscDaemon /XML "));
        assert!(calls[0].ends_with(" /F"));
        assert_eq!(calls[2], "schtasks.exe /Delete /TN AscDaemon /F");
        assert!(!d.path().join("asc-daemon-task.xml").exists());
        let g = Fake(RefCell::new(vec![]), true);
        uninstall(Platform::WindowsTask, d.path(), &g).unwrap();
        assert_eq!(g.0.borrow().len(), 1);
    }
}
