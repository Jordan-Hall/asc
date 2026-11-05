//! `asc cache dotnet` and `asc cache pytest` (docs/designs/build-cache.md §4.6): `dotnet build`,
//! `test`, `publish` and `pack`, and a pytest run, as cached actions (`asc cache run`) with the
//! tool's inputs, outputs, variables and versions filled in. Any other dotnet command, and a
//! pytest run that depends on the last run or writes the sources, runs uncached.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use super::run::{self, Spec};
use crate::cli::common::{CliError, Ctx, Result};

#[derive(clap::Args, Debug)]
pub struct DotnetArgs {
    /// A name for the action (default: `dotnet-<verb>`), part of its key.
    #[arg(long)]
    name: Option<String>,
    /// More paths that key the action, besides the projects and the SDK and MSBuild files above
    /// them.
    #[arg(long = "inputs", num_args = 1..)]
    inputs: Vec<OsString>,
    /// More files or directories the command writes, besides each project's `bin` and the
    /// `--output` and `--results-directory` it is given.
    #[arg(long = "outputs", num_args = 1..)]
    outputs: Vec<OsString>,
    /// More environment variables that key the action.
    #[arg(long = "env")]
    env: Vec<String>,
    /// Neither fetch from nor queue for the server.
    #[arg(long = "no-remote")]
    no_remote: bool,
    /// No `cache hit` note on stderr.
    #[arg(long, short = 'q')]
    quiet: bool,
    /// The dotnet command line, after `--`.
    #[arg(last = true, required = true, num_args = 1..)]
    args: Vec<OsString>,
}

#[derive(clap::Args, Debug)]
pub struct PytestArgs {
    /// A name for the action (default: `pytest`), part of its key.
    #[arg(long)]
    name: Option<String>,
    /// More paths that key the run, besides the current directory and the Python project files
    /// above it.
    #[arg(long = "inputs", num_args = 1..)]
    inputs: Vec<OsString>,
    /// More files or directories the run writes, besides its JUnit XML and coverage reports.
    #[arg(long = "outputs", num_args = 1..)]
    outputs: Vec<OsString>,
    /// More environment variables that key the run.
    #[arg(long = "env")]
    env: Vec<String>,
    /// The Python to run pytest with (default: `python3`, or `python` on Windows).
    #[arg(long)]
    python: Option<OsString>,
    /// Neither fetch from nor queue for the server.
    #[arg(long = "no-remote")]
    no_remote: bool,
    /// No `cache hit` note on stderr.
    #[arg(long, short = 'q')]
    quiet: bool,
    /// pytest's options and paths, after `--`.
    #[arg(last = true, num_args = 0..)]
    args: Vec<OsString>,
}

/// The dotnet commands asc caches.
const DOTNET_VERBS: &[&str] = &["build", "test", "publish", "pack"];
/// Files above a project that change what MSBuild and NuGet do.
const DOTNET_FILES: &[&str] = &[
    "global.json",
    "Directory.Build.props",
    "Directory.Build.targets",
    "Directory.Packages.props",
    "Directory.Solution.props",
    "Directory.Solution.targets",
    "NuGet.Config",
    "NuGet.config",
    "nuget.config",
];
const DOTNET_ENV: &[&str] = &["DOTNET_", "NUGET_", "MSBUILD"];
/// Directories that hold no project.
const SKIP_DIRS: &[&str] = &["bin", "obj", "node_modules", "TestResults"];
/// Files above the current directory that change what pytest collects or imports.
const PYTHON_FILES: &[&str] = &[
    "pyproject.toml",
    "setup.cfg",
    "setup.py",
    "pytest.ini",
    "tox.ini",
    "conftest.py",
    ".python-version",
    "uv.lock",
    "poetry.lock",
    "Pipfile.lock",
    "pdm.lock",
];
const PYTHON_ENV: &[&str] = &["PYTHON", "PYTEST_"];
/// pytest options whose run depends on the last run, stops for a debugger, loops or writes the
/// sources (snapshot updates).
const PYTEST_UNCACHED: &[&str] = &[
    "--lf",
    "--last-failed",
    "--ff",
    "--failed-first",
    "--nf",
    "--new-first",
    "--sw",
    "--stepwise",
    "--sw-skip",
    "--stepwise-skip",
    "-f",
    "--looponfail",
    "--pdb",
    "--trace",
    "--snapshot-update",
    "--force-regen",
];

fn lossy(a: &OsString) -> String {
    a.to_string_lossy().into_owned()
}

/// Runs `program args` with the terminal as its streams.
fn passthrough(program: &str, args: &[OsString], cwd: &Path) -> Result<ExitCode> {
    let status = Command::new(run::program(&program.into()))
        .args(args)
        .current_dir(cwd)
        .status()
        .map_err(|e| CliError::Fatal(format!("cannot run '{program}': {e}")))?;
    Ok(match status.code() {
        Some(0) => ExitCode::SUCCESS,
        Some(c) => ExitCode::from(u8::try_from(c).unwrap_or(1)),
        None => ExitCode::FAILURE,
    })
}

/// What `program args` prints, or an error naming it.
fn output_of(program: &OsString, args: &[&str], cwd: &Path) -> Result<Vec<u8>> {
    let out = Command::new(run::program(program))
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| CliError::Fatal(format!("cannot run '{}': {e}", lossy(program))))?;
    if !out.status.success() {
        return Err(CliError::Fatal(format!(
            "'{} {}' failed: {}",
            lossy(program),
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

/// The files named `names` in `from` and each directory above it up to `root`.
fn files_up(root: &Path, from: &Path, names: &[&str], out: &mut BTreeSet<PathBuf>) {
    for dir in from.ancestors() {
        if !dir.starts_with(root) {
            break;
        }
        for n in names {
            let p = dir.join(n);
            if p.is_file() {
                out.insert(p);
            }
        }
    }
}

/// The names of the set variables that start with one of `prefixes`, and `extra`.
fn env_names(prefixes: &[&str], extra: &[String]) -> Vec<String> {
    let mut names: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| {
            prefixes
                .iter()
                .any(|p| k.to_ascii_uppercase().starts_with(p))
        })
        .chain(extra.iter().cloned())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The value of `--flag v`, `--flag=v` or `-f v` in `args`, for each occurrence.
fn values_of(args: &[String], flags: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for (i, a) in args.iter().enumerate() {
        for f in flags {
            if a == f {
                if let Some(v) = args.get(i + 1) {
                    out.push(v.clone());
                }
            } else if let Some(v) = a.strip_prefix(f).and_then(|r| r.strip_prefix('=')) {
                out.push(v.to_owned());
            }
        }
    }
    out
}

fn is_project(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e, "csproj" | "fsproj" | "vbproj"))
}

/// The project files under `dir`, not inside build output or hidden directories.
fn projects_under(dir: &Path, depth: usize, out: &mut BTreeSet<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        match e.file_type() {
            Ok(t) if t.is_dir() => {
                if depth > 0 && !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_str()) {
                    projects_under(&p, depth - 1, out);
                }
            }
            Ok(t) if t.is_file() && is_project(&p) => {
                out.insert(p);
            }
            _ => {}
        }
    }
}

/// The `Include` of each `ProjectReference` in a project file.
fn project_references(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("<ProjectReference") {
        rest = &rest[at + "<ProjectReference".len()..];
        let tag = &rest[..rest.find('>').unwrap_or(rest.len())];
        if let Some(i) = tag.find("Include=") {
            let v = &tag[i + "Include=".len()..];
            if let Some(q) = v.chars().next().filter(|c| *c == '"' || *c == '\'') {
                if let Some(end) = v[1..].find(q) {
                    out.push(v[1..1 + end].replace('\\', "/"));
                }
            }
        }
    }
    out
}

/// The projects a dotnet command builds: the one it names, else those under the directory it
/// names (or the current one), and every project they reference, inside the work tree.
fn dotnet_projects(root: &Path, cwd: &Path, args: &[String]) -> BTreeSet<PathBuf> {
    let target = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .map(|a| cwd.join(a))
        .filter(|p| p.exists());
    let mut found = BTreeSet::new();
    match &target {
        Some(p) if p.is_file() && is_project(p) => {
            found.insert(p.clone());
        }
        Some(p) if p.is_dir() => projects_under(p, 8, &mut found),
        // A solution file: its projects are below it.
        Some(p) => projects_under(p.parent().unwrap_or(cwd), 8, &mut found),
        None => projects_under(cwd, 8, &mut found),
    }
    let mut todo: Vec<PathBuf> = found.iter().cloned().collect();
    while let Some(p) = todo.pop() {
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let dir = p.parent().unwrap_or(root).to_owned();
        for r in project_references(&text) {
            let Ok(dep) = std::fs::canonicalize(dir.join(&r)) else {
                continue;
            };
            let dep = strip_verbatim(dep);
            if dep.starts_with(root) && found.insert(dep.clone()) {
                todo.push(dep);
            }
        }
    }
    found
}

/// `\\?\C:\x` as `C:\x`, so it compares with the work tree's root.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => p,
    }
}

/// The action for a cacheable dotnet command, `None` for any other.
fn dotnet_spec(ctx: &Ctx, root: &Path, a: &DotnetArgs) -> Result<Option<Spec>> {
    let args: Vec<String> = a.args.iter().map(lossy).collect();
    let verb = args[0].as_str();
    if !DOTNET_VERBS.contains(&verb) {
        return Ok(None);
    }
    let cwd = &ctx.cwd;
    let projects = dotnet_projects(root, cwd, &args);
    let mut inputs = BTreeSet::new();
    let mut outputs: BTreeSet<PathBuf> = BTreeSet::new();
    inputs.insert(cwd.clone());
    files_up(root, cwd, DOTNET_FILES, &mut inputs);
    for p in &projects {
        let dir = p.parent().unwrap_or(root);
        inputs.insert(dir.to_owned());
        files_up(root, dir, DOTNET_FILES, &mut inputs);
        outputs.insert(dir.join("bin"));
    }
    for v in values_of(&args, &["-o", "--output", "--results-directory"]) {
        outputs.insert(cwd.join(v));
    }
    let sdk = output_of(&"dotnet".into(), &["--version"], cwd)?;
    Ok(Some(Spec {
        tool: "dotnet",
        name: a.name.clone().unwrap_or_else(|| format!("dotnet-{verb}")),
        inputs: inputs
            .into_iter()
            .map(PathBuf::into_os_string)
            .chain(a.inputs.iter().cloned())
            .collect(),
        outputs: outputs
            .into_iter()
            .map(PathBuf::into_os_string)
            .chain(a.outputs.iter().cloned())
            .collect(),
        env: env_names(DOTNET_ENV, &a.env),
        parts: vec![("sdk", sdk.trim_ascii().to_vec())],
        no_remote: a.no_remote,
        quiet: a.quiet,
        command: std::iter::once("dotnet".into())
            .chain(a.args.iter().cloned())
            .collect(),
    }))
}

pub async fn dotnet(ctx: &Ctx, a: DotnetArgs) -> Result<ExitCode> {
    let root = ctx.ws()?.projection.toplevel().to_owned();
    match dotnet_spec(ctx, &root, &a)? {
        Some(spec) => run::cached(ctx, &spec).await,
        None => passthrough("dotnet", &a.args, &ctx.cwd),
    }
}

/// A run asc caches: none of the options that make it depend on the last run or write sources.
fn pytest_cacheable(args: &[String]) -> bool {
    !args.iter().any(|a| {
        let flag = a.split('=').next().unwrap_or(a);
        PYTEST_UNCACHED.contains(&flag) || a.starts_with("--inline-snapshot")
    })
}

/// The reports a pytest run writes: `--junitxml` and the coverage reports and data file.
fn pytest_outputs(args: &[String]) -> Vec<String> {
    let mut out = values_of(args, &["--junitxml", "--junit-xml"]);
    for r in values_of(args, &["--cov-report"]) {
        let (kind, path) = r.split_once(':').unwrap_or((r.as_str(), ""));
        let default = match kind {
            "xml" => "coverage.xml",
            "html" => "htmlcov",
            "lcov" => "coverage.lcov",
            "json" => "coverage.json",
            "annotate" => "",
            _ => continue,
        };
        let path = if path.is_empty() { default } else { path };
        if !path.is_empty() {
            out.push(path.to_owned());
        }
    }
    if args.iter().any(|a| a == "--cov" || a.starts_with("--cov=")) {
        out.push(".coverage".to_owned());
    }
    out
}

/// The interpreter's version and its installed distributions: what the tests import.
const PYTHON_ID: &str = "import sys, importlib.metadata as m\n\
print(sys.version)\n\
print(sorted({(d.metadata['Name'] or '').lower() + '==' + d.version for d in m.distributions()}))";

fn default_python() -> OsString {
    if cfg!(windows) { "python" } else { "python3" }.into()
}

pub async fn pytest(ctx: &Ctx, a: PytestArgs) -> Result<ExitCode> {
    let root = ctx.ws()?.projection.toplevel().to_owned();
    let python = a.python.clone().unwrap_or_else(default_python);
    let command: Vec<OsString> = [python.clone(), "-m".into(), "pytest".into()]
        .into_iter()
        .chain(a.args.iter().cloned())
        .collect();
    let args: Vec<String> = a.args.iter().map(lossy).collect();
    if !pytest_cacheable(&args) {
        return passthrough(&lossy(&python), &command[1..], &ctx.cwd);
    }
    let mut inputs = BTreeSet::new();
    inputs.insert(ctx.cwd.clone());
    files_up(&root, &ctx.cwd, PYTHON_FILES, &mut inputs);
    for dir in ctx.cwd.ancestors().take_while(|d| d.starts_with(&root)) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("requirements") && n.ends_with(".txt") {
                inputs.insert(e.path());
            }
        }
    }
    let id = output_of(&python, &["-c", PYTHON_ID], &ctx.cwd)?;
    let spec = Spec {
        tool: "pytest",
        name: a.name.clone().unwrap_or_else(|| "pytest".into()),
        inputs: inputs
            .into_iter()
            .map(PathBuf::into_os_string)
            .chain(a.inputs.iter().cloned())
            .collect(),
        outputs: pytest_outputs(&args)
            .into_iter()
            .map(OsString::from)
            .chain(a.outputs.iter().cloned())
            .collect(),
        env: env_names(PYTHON_ENV, &a.env),
        parts: vec![("python", id)],
        no_remote: a.no_remote,
        quiet: a.quiet,
        command,
    };
    run::cached(ctx, &spec).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn option_values_in_every_spelling() {
        let a = strings(&[
            "build",
            "-o",
            "out",
            "--output=pub",
            "--results-directory",
            "tr",
        ]);
        assert_eq!(values_of(&a, &["-o", "--output"]), ["out", "pub"]);
        assert_eq!(values_of(&a, &["--results-directory"]), ["tr"]);
        assert!(values_of(&strings(&["-o"]), &["-o"]).is_empty());
    }

    #[test]
    fn project_references_are_read_in_either_quote_and_slash() {
        let text = r#"<Project><ItemGroup>
  <ProjectReference Include="..\lib\lib.csproj" />
  <ProjectReference Include='../util/util.fsproj'/>
  <PackageReference Include="x" Version="1" />
</ItemGroup></Project>"#;
        assert_eq!(
            project_references(text),
            ["../lib/lib.csproj", "../util/util.fsproj"]
        );
    }

    #[test]
    fn projects_come_with_what_they_reference_and_skip_build_output() {
        let d = tempfile::tempdir().unwrap();
        let root = strip_verbatim(std::fs::canonicalize(d.path()).unwrap());
        let w = |rel: &str, body: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        w(
            "app/app.csproj",
            r#"<Project><ItemGroup><ProjectReference Include="..\lib\lib.csproj" /></ItemGroup></Project>"#,
        );
        w("lib/lib.csproj", "<Project />");
        w("app/bin/Debug/copy.csproj", "<Project />");
        w("other/other.csproj", "<Project />");
        let app = root.join("app");
        let got = dotnet_projects(&root, &app, &strings(&["build"]));
        assert_eq!(
            got.into_iter().collect::<Vec<_>>(),
            [root.join("app/app.csproj"), root.join("lib/lib.csproj")]
        );
        let named = dotnet_projects(&root, &root, &strings(&["build", "lib/lib.csproj"]));
        assert_eq!(
            named.into_iter().collect::<Vec<_>>(),
            [root.join("lib/lib.csproj")]
        );
    }

    #[test]
    fn config_files_are_found_up_to_the_root_only() {
        let d = tempfile::tempdir().unwrap();
        let top = d.path().join("top");
        std::fs::create_dir_all(top.join("a/b")).unwrap();
        std::fs::write(d.path().join("global.json"), "{}").unwrap();
        std::fs::write(top.join("global.json"), "{}").unwrap();
        std::fs::write(top.join("a/Directory.Build.props"), "<Project />").unwrap();
        let mut got = BTreeSet::new();
        files_up(&top, &top.join("a/b"), DOTNET_FILES, &mut got);
        assert_eq!(
            got.into_iter().collect::<Vec<_>>(),
            [top.join("a/Directory.Build.props"), top.join("global.json")]
        );
    }

    #[test]
    fn pytest_runs_that_depend_on_the_last_run_are_not_cached() {
        assert!(pytest_cacheable(&strings(&["-x", "tests/", "-k", "sum"])));
        for a in [
            "--lf",
            "--last-failed",
            "--ff",
            "--sw",
            "-f",
            "--pdb",
            "--snapshot-update",
        ] {
            assert!(!pytest_cacheable(&strings(&[a])), "{a}");
        }
        assert!(!pytest_cacheable(&strings(&["--inline-snapshot=fix"])));
    }

    #[test]
    fn pytest_reports_are_outputs() {
        let a = strings(&[
            "--junitxml=out/junit.xml",
            "--cov=pkg",
            "--cov-report",
            "xml",
            "--cov-report=html:cov_html",
            "--cov-report=term-missing",
        ]);
        assert_eq!(
            pytest_outputs(&a),
            ["out/junit.xml", "coverage.xml", "cov_html", ".coverage"]
        );
        assert!(pytest_outputs(&strings(&["-q"])).is_empty());
    }

    #[test]
    fn tool_variables_are_found_by_prefix() {
        let names = env_names(&["PATH"], &["ASC_EXTRA".into()]);
        assert!(names.iter().any(|n| n.eq_ignore_ascii_case("PATH")));
        assert!(names.contains(&"ASC_EXTRA".to_owned()));
    }
}
