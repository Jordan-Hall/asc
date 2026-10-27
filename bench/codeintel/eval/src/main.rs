//! `asc-bench-codeintel-eval`: builds the ASC eval set from history and scores the model
//! bake-off (PLAN I28). It runs git with fixed arguments and never a shell.
//! It takes subcommands (`build`, `score`) rather than a suite, so `asc-bench` does not forward to it.

use asc_bench_codeintel_eval::{
    EvalQuery, LOG_ARGS, Results, TARGET_QUERIES, decide, label, parse_log,
};
use clap::{Parser, Subcommand};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

#[derive(Parser)]
#[command(about = "Semantic eval set from ASC history and the model bake-off")]
struct Opts {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Label queries from the history of REPO (PR title = query, changed files = relevant).
    Build {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long, default_value = "master")]
        rev: String,
        #[arg(long, default_value_t = TARGET_QUERIES)]
        count: usize,
        #[arg(long, default_value = "git")]
        git: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Score rankings, pick the model, and print the decision (JSON). Exit 1 when the
    /// hybrid gate (BM25-only + 5 points) is not met.
    Score {
        #[arg(long)]
        set: PathBuf,
        #[arg(long)]
        results: PathBuf,
        /// Write the decision JSON here as well.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

fn git(bin: &Path, repo: &Path, args: &[&str]) -> Result<String, String> {
    let o = Command::new(bin)
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    if !o.status.success() {
        return Err(format!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&o.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

fn read(p: &Path) -> Result<String, String> {
    std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))
}

fn run(o: Opts) -> Result<ExitCode, String> {
    match o.cmd {
        Cmd::Build {
            repo,
            rev,
            count,
            git: g,
            out,
        } => {
            if rev.starts_with('-') {
                return Err("rev must not look like an option".into());
            }
            let mut args: Vec<&str> = LOG_ARGS.to_vec();
            args.push(&rev);
            args.push("--");
            let log = git(&g, &repo, &args)?;
            let tree = git(&g, &repo, &["ls-tree", "-r", "--name-only", &rev, "--"])?;
            let head: BTreeSet<String> = tree.lines().map(String::from).collect();
            let set = label(&parse_log(&log), &head, count);
            if set.is_empty() {
                return Err("no usable queries in the history".into());
            }
            let json = serde_json::to_vec_pretty(&set).map_err(|e| e.to_string())?;
            std::fs::write(&out, json).map_err(|e| format!("{}: {e}", out.display()))?;
            println!("{} queries -> {}", set.len(), out.display());
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Score { set, results, out } => {
            let set: Vec<EvalQuery> =
                serde_json::from_str(&read(&set)?).map_err(|e| e.to_string())?;
            let res: Results = serde_json::from_str(&read(&results)?).map_err(|e| e.to_string())?;
            let d = decide(&set, &res)?;
            let json = serde_json::to_string_pretty(&d).map_err(|e| e.to_string())?;
            println!("{json}");
            if let Some(p) = out {
                std::fs::write(&p, &json).map_err(|e| format!("{}: {e}", p.display()))?;
            }
            Ok(if d.semantic_default == "hybrid" {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
    }
}

fn main() -> ExitCode {
    match run(Opts::parse()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}
