//! `asc-bench`: runs git and asc on the same scenarios, prints the table and
//! fails when the gate of SPEC 16.4 is not met.

use asc_bench::run::{Tool, run_suite, suite};
use asc_bench::{Baseline, gate, release_markdown, table};
use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(about = "Benchmark gate: git vs asc")]
struct Opts {
    /// quick (small and 10x) or full (adds 100x).
    #[arg(long, default_value = "quick")]
    suite: String,
    /// Only scenarios whose name contains this text.
    #[arg(long)]
    only: Option<String>,
    /// The asc binary (default: ASC_BIN, else `asc` next to this executable). Never searched on PATH.
    #[arg(long, env = "ASC_BIN")]
    asc: Option<PathBuf>,
    /// The git binary.
    #[arg(long, default_value = "git")]
    git: PathBuf,
    #[arg(long, default_value = "bench/baseline.json")]
    baseline: PathBuf,
    /// Rewrite the baseline from this run instead of checking it.
    #[arg(long)]
    update_baseline: bool,
    /// Treat an asc command that is not implemented yet as a skip (local runs; CI never sets it).
    #[arg(long)]
    allow_unsupported: bool,
    /// Write results.json here.
    #[arg(long)]
    json: Option<PathBuf>,
    /// Print the docs/BENCHMARKS.md section for this release label.
    #[arg(long)]
    release: Option<String>,
    /// Scratch directory (default: the OS temp directory).
    #[arg(long)]
    work: Option<PathBuf>,
}

fn sibling_asc() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let p = exe
        .parent()?
        .join(if cfg!(windows) { "asc.exe" } else { "asc" });
    p.is_file().then_some(p)
}

fn main() -> ExitCode {
    let o = Opts::parse();
    let Some(workloads) = suite(&o.suite) else {
        eprintln!("unknown suite {:?} (quick, full)", o.suite);
        return ExitCode::from(2);
    };
    let scratch = match &o.work {
        Some(w) => tempfile::Builder::new().prefix("asc-bench-").tempdir_in(w),
        None => tempfile::Builder::new().prefix("asc-bench-").tempdir(),
    };
    let scratch = match scratch {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot create scratch directory: {e}");
            return ExitCode::from(2);
        }
    };
    let git = match Tool::new("git", o.git.clone(), scratch.path()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let asc =
        o.asc.clone()
            .or_else(sibling_asc)
            .and_then(|p| Tool::new("asc", p, scratch.path()).ok());
    if asc.is_none() {
        eprintln!(
            "warning: no asc binary (build `cargo build -p asc --release`, then set ASC_BIN or pass --asc)"
        );
    }
    let rows = match run_suite(
        &git,
        asc.as_ref(),
        &workloads,
        o.only.as_deref(),
        scratch.path(),
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("benchmark setup failed: {e}");
            return ExitCode::from(2);
        }
    };
    print!("{}", table(&rows));
    if let Some(j) = &o.json
        && let Err(e) = serde_json::to_vec_pretty(&rows)
            .map_err(|e| e.to_string())
            .and_then(|b| std::fs::write(j, b).map_err(|e| e.to_string()))
    {
        eprintln!("cannot write {}: {e}", j.display());
        return ExitCode::from(2);
    }
    if let Some(rel) = &o.release {
        let gv = git.version(scratch.path());
        let cv = asc
            .as_ref()
            .map_or("none".into(), |c| c.version(scratch.path()));
        let machine = format!(
            "{} {}, {} logical CPUs",
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::thread::available_parallelism().map_or(0, |n| n.get())
        );
        println!("\n{}", release_markdown(rel, &gv, &cv, &machine, &rows));
    }
    if o.update_baseline {
        let b = Baseline::from_rows(&rows);
        match serde_json::to_string_pretty(&b)
            .map_err(|e| e.to_string())
            .and_then(|s| std::fs::write(&o.baseline, s + "\n").map_err(|e| e.to_string()))
        {
            Ok(()) => {
                eprintln!("baseline written to {}", o.baseline.display());
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("cannot write baseline: {e}");
                return ExitCode::from(2);
            }
        }
    }
    let baseline = match std::fs::read_to_string(&o.baseline) {
        Ok(s) => match serde_json::from_str::<Baseline>(&s) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("bad baseline {}: {e}", o.baseline.display());
                return ExitCode::from(2);
            }
        },
        Err(_) => Baseline::default(),
    };
    let violations = gate(&rows, &baseline, o.allow_unsupported);
    if violations.is_empty() {
        println!("\ngate: pass");
        ExitCode::SUCCESS
    } else {
        println!("\ngate: FAIL");
        for v in &violations {
            println!("  {v}");
        }
        ExitCode::FAILURE
    }
}
