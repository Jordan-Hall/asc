//! `asc-bench-codeintel`: SPEC 23.9 scenarios and gates. Joins the O05 gate through
//! `--suite codeintel-quick`: `asc-bench --suite codeintel-quick|codeintel-full` runs this binary.

use asc_bench_codeintel::run::{self, Tools};
use asc_bench_codeintel::{Metrics, Row, Workload, gate, parse_queries, release_markdown, table};
use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(about = "Code intelligence benchmarks against rg, git grep and Zoekt")]
struct Opts {
    /// codeintel-quick (small, medium) or codeintel-full (adds HUGE).
    #[arg(long, default_value = "codeintel-quick")]
    suite: String,
    /// Query scenarios.
    #[arg(long, default_value = "bench/codeintel/scenarios.toml")]
    scenarios: PathBuf,
    #[arg(long, default_value = "git")]
    git: PathBuf,
    /// The asc binary (never searched on PATH).
    #[arg(long, env = "ASC_BIN")]
    asc: Option<PathBuf>,
    #[arg(long, env = "RG_BIN")]
    rg: Option<PathBuf>,
    #[arg(long, env = "ZOEKT_BIN")]
    zoekt: Option<PathBuf>,
    #[arg(long, env = "ZOEKT_GIT_INDEX_BIN")]
    zoekt_git_index: Option<PathBuf>,
    /// An existing repository (branch master) used as the medium workload, e.g. the linux import.
    #[arg(long)]
    medium_repo: Option<PathBuf>,
    /// An existing repository used as the HUGE workload.
    #[arg(long)]
    huge_repo: Option<PathBuf>,
    /// JSON of the server-side and eval measurements (incremental index, index size, RSS, NDCG@10).
    #[arg(long)]
    metrics: Option<PathBuf>,
    /// Timed runs per tool and query.
    #[arg(long, default_value_t = 20)]
    runs: usize,
    /// Treat a tool that is not available as a skip (local runs; CI never sets it).
    #[arg(long)]
    allow_unsupported: bool,
    #[arg(long)]
    json: Option<PathBuf>,
    /// Print the docs/BENCHMARKS.md section for this release label.
    #[arg(long)]
    release: Option<String>,
    #[arg(long)]
    work: Option<PathBuf>,
}

fn setup(o: &Opts) -> Result<Vec<Row>, String> {
    let workloads = run::suite(&o.suite).ok_or_else(|| {
        format!(
            "unknown suite {:?} (codeintel-quick, codeintel-full)",
            o.suite
        )
    })?;
    let queries = parse_queries(
        &std::fs::read_to_string(&o.scenarios)
            .map_err(|e| format!("{}: {e}", o.scenarios.display()))?,
    )?;
    let scratch = match &o.work {
        Some(w) => tempfile::Builder::new()
            .prefix("asc-bench-ci-")
            .tempdir_in(w),
        None => tempfile::Builder::new().prefix("asc-bench-ci-").tempdir(),
    }
    .map_err(|e| format!("scratch directory: {e}"))?;
    let tools = Tools {
        git: o.git.clone(),
        asc: o.asc.clone(),
        rg: o.rg.clone(),
        zoekt: o.zoekt.clone(),
        zoekt_git_index: o.zoekt_git_index.clone(),
    };
    let mut rows = Vec::new();
    for w in workloads {
        let supplied = match w {
            Workload::Medium => o.medium_repo.clone(),
            Workload::Huge => o.huge_repo.clone(),
            Workload::Small => None,
        };
        let repo = match supplied {
            Some(r) => r,
            None => {
                let r = scratch.path().join(w.name());
                run::build_repo(&tools.git, &r, run::files_for(w))?;
                r
            }
        };
        let idx = scratch.path().join(format!("{}-zoekt", w.name()));
        run::build_zoekt(&tools, &repo, &idx)?;
        rows.extend(run::run_queries(&tools, w, &queries, &repo, &idx, o.runs));
    }
    Ok(rows)
}

fn main() -> ExitCode {
    let o = Opts::parse();
    if o.runs == 0 {
        eprintln!("--runs must be at least 1");
        return ExitCode::from(2);
    }
    let metrics = match &o.metrics {
        Some(p) => match std::fs::read_to_string(p)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str::<Metrics>(&t).map_err(|e| e.to_string()))
        {
            Ok(m) => m,
            Err(e) => {
                eprintln!("bad metrics {}: {e}", p.display());
                return ExitCode::from(2);
            }
        },
        None => Metrics::default(),
    };
    let rows = match setup(&o) {
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
        let versions = format!(
            "git {}; rg {}; zoekt {}",
            run::tool_version(&o.git, "--version"),
            o.rg.as_deref()
                .map_or("none".into(), |p| run::tool_version(p, "--version")),
            o.zoekt
                .as_deref()
                .map_or("none".into(), |p| run::tool_version(p, "-version")),
        );
        let machine = format!(
            "{} {}, {} logical CPUs",
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::thread::available_parallelism().map_or(0, |n| n.get())
        );
        println!("\n{}", release_markdown(rel, &versions, &machine, &rows));
    }
    let violations = gate(&rows, &metrics, o.allow_unsupported);
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
