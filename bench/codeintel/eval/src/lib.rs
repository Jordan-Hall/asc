//! Semantic eval set and model bake-off (SPEC 23.9, PLAN I28).
//!
//! `label` turns `git log` history into queries (PR title = query, changed files = relevant).
//! `ndcg_at_k` and `recall_at_k` score rankings; `decide` applies the three rules of the row:
//! a candidate model wins at >= 3 NDCG@10 points within 2x embed time, b1 retention below 93%
//! means the daemon keeps i8 only, and hybrid below BM25-only + 5 points makes `mode = semantic`
//! default to BM25.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const K: usize = 10;
pub const TARGET_QUERIES: usize = 200;
pub const MAX_RELEVANT: usize = 20;
pub const WIN_POINTS: f64 = 3.0;
pub const MAX_EMBED_RATIO: f64 = 2.0;
pub const B1_RETENTION: f64 = 0.93;
pub const HYBRID_POINTS: f64 = 5.0;

/// `git log` arguments that `parse_log` reads; the caller runs git (no shell, no pager).
pub const LOG_ARGS: [&str; 10] = [
    "-c",
    "core.quotepath=off",
    "log",
    "--first-parent",
    "-m",
    "--no-color",
    "--name-only",
    "--format=%x1e%H%x1f%s%x1f%b%x1f",
    "--no-renames",
    "--no-ext-diff",
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalQuery {
    pub id: String,
    pub query: String,
    pub relevant: Vec<String>,
    pub commit: String,
}

/// Normalises a PR title: merge-commit bodies, `(#123)` suffixes and `type(scope):` prefixes.
fn title_of(subject: &str, body: &str) -> String {
    let mut t = if subject.starts_with("Merge pull request") || subject.starts_with("Merge branch")
    {
        body.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_string()
    } else {
        subject.trim().to_string()
    };
    if let Some(i) = t.rfind(" (#")
        && t.ends_with(')')
        && t[i + 3..t.len() - 1].bytes().all(|b| b.is_ascii_digit())
    {
        t.truncate(i);
    }
    if let Some(i) = t.find(": ") {
        let head = &t[..i];
        let kind = head.split('(').next().unwrap_or("");
        if !kind.is_empty()
            && kind.bytes().all(|b| b.is_ascii_lowercase())
            && (head.len() == kind.len() || head.ends_with(')'))
        {
            t = t[i + 2..].trim().to_string();
        }
    }
    t
}

fn usable_title(t: &str) -> bool {
    let lower = t.to_ascii_lowercase();
    t.split_whitespace().count() >= 3
        && !["merge ", "revert", "wip", "fixup", "squash", "bump "]
            .iter()
            .any(|p| lower.starts_with(p))
        && !lower.contains("[bot]")
}

/// Generated, vendored, lock and binary paths are not retrievable code.
fn relevant_path(p: &str) -> bool {
    let lower = p.to_ascii_lowercase();
    const SKIP_SUFFIX: [&str; 12] = [
        ".lock", ".lockb", ".sum", ".png", ".jpg", ".gif", ".ico", ".svg", ".pdf", ".min.js",
        ".map", ".snap",
    ];
    !lower.starts_with("vendor/")
        && !lower.contains("/node_modules/")
        && !lower.starts_with("node_modules/")
        && !lower.contains("package-lock.json")
        && !SKIP_SUFFIX.iter().any(|s| lower.ends_with(s))
}

/// A history entry before the head filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub commit: String,
    pub title: String,
    pub files: Vec<String>,
}

/// Parses the output of `git log` with [`LOG_ARGS`].
pub fn parse_log(text: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    for rec in text.split('\x1e').filter(|r| !r.trim().is_empty()) {
        let mut parts = rec.splitn(4, '\x1f');
        let (Some(h), Some(s), Some(b), Some(files)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let commit = h.trim().to_string();
        if commit.len() < 7 || !commit.bytes().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let mut fs: Vec<String> = files
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        fs.sort();
        fs.dedup();
        out.push(Candidate {
            commit,
            title: title_of(s.trim(), b),
            files: fs,
        });
    }
    out
}

/// Keeps usable, deduplicated candidates whose changed files still exist at the eval revision
/// (`head_files`), then samples `n` of them at an even stride so the set is reproducible.
pub fn label(cands: &[Candidate], head_files: &BTreeSet<String>, n: usize) -> Vec<EvalQuery> {
    let mut seen = BTreeSet::new();
    let mut kept = Vec::new();
    for c in cands {
        if !usable_title(&c.title) || !seen.insert(c.title.to_ascii_lowercase()) {
            continue;
        }
        let rel: Vec<String> = c
            .files
            .iter()
            .filter(|f| relevant_path(f) && head_files.contains(*f))
            .cloned()
            .collect();
        if rel.is_empty() || rel.len() > MAX_RELEVANT {
            continue;
        }
        kept.push((c, rel));
    }
    let take = n.min(kept.len());
    (0..take)
        .map(|i| {
            let (c, rel) = &kept[i * kept.len() / take];
            EvalQuery {
                id: format!("q{:03}", i + 1),
                query: c.title.clone(),
                relevant: rel.clone(),
                commit: c.commit.clone(),
            }
        })
        .collect()
}

/// Binary-relevance NDCG@k of one ranking; duplicate paths count once, at their first rank.
pub fn ndcg_at_k(ranking: &[String], relevant: &BTreeSet<&str>, k: usize) -> f64 {
    if relevant.is_empty() {
        return 0.0;
    }
    let mut seen = BTreeSet::new();
    let mut dcg = 0.0;
    let mut rank = 0usize;
    for p in ranking {
        if !seen.insert(p.as_str()) {
            continue;
        }
        if rank >= k {
            break;
        }
        if relevant.contains(p.as_str()) {
            dcg += 1.0 / ((rank + 2) as f64).log2();
        }
        rank += 1;
    }
    let ideal: f64 = (0..relevant.len().min(k))
        .map(|i| 1.0 / ((i + 2) as f64).log2())
        .sum();
    dcg / ideal
}

pub fn recall_at_k(ranking: &[String], relevant: &BTreeSet<&str>, k: usize) -> f64 {
    if relevant.is_empty() {
        return 0.0;
    }
    let mut seen = BTreeSet::new();
    let hits = ranking
        .iter()
        .filter(|p| seen.insert(p.as_str()))
        .take(k)
        .filter(|p| relevant.contains(p.as_str()))
        .count();
    hits as f64 / relevant.len() as f64
}

/// Rankings by query id.
pub type Rankings = BTreeMap<String, Vec<String>>;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Score {
    /// Points, 0 to 100.
    pub ndcg10: f64,
    pub recall10: f64,
}

/// Mean scores over the whole eval set; a query without a ranking scores 0. A ranking for an
/// unknown query id is an error (it would hide a mismatched eval set).
pub fn score(set: &[EvalQuery], r: &Rankings) -> Result<Score, String> {
    if set.is_empty() {
        return Err("empty eval set".into());
    }
    let ids: BTreeSet<&str> = set.iter().map(|q| q.id.as_str()).collect();
    if let Some(bad) = r.keys().find(|k| !ids.contains(k.as_str())) {
        return Err(format!("ranking for unknown query {bad:?}"));
    }
    let (mut n, mut rc) = (0.0, 0.0);
    for q in set {
        let rel: BTreeSet<&str> = q.relevant.iter().map(String::as_str).collect();
        let empty = Vec::new();
        let ranking = r.get(&q.id).unwrap_or(&empty);
        n += ndcg_at_k(ranking, &rel, K);
        rc += recall_at_k(ranking, &rel, K);
    }
    let m = set.len() as f64;
    Ok(Score {
        ndcg10: 100.0 * n / m,
        recall10: 100.0 * rc / m,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelRun {
    pub model: String,
    /// Marks the incumbent (jina-embeddings-v2-base-code); exactly one run has it.
    #[serde(default)]
    pub default: bool,
    /// Mean embed time per chunk (any fixed unit, the same for all models).
    pub embed_ms: f64,
    /// Hybrid rankings at full precision.
    pub hybrid: Rankings,
    /// Hybrid rankings with b1 (binary) vectors, when measured.
    #[serde(default)]
    pub hybrid_b1: Option<Rankings>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Results {
    pub bm25: Rankings,
    pub models: Vec<ModelRun>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ModelReport {
    pub model: String,
    pub hybrid: Score,
    pub embed_ms: f64,
    pub b1: Option<Score>,
    /// b1 NDCG@10 / full-precision NDCG@10.
    pub b1_retention: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Decision {
    pub bm25: Score,
    pub models: Vec<ModelReport>,
    pub chosen: String,
    /// False when retention is below 93% (or not measured): i8 only in the daemon.
    pub daemon_b1: bool,
    /// What `search` with `mode = semantic` defaults to: "hybrid" or "bm25".
    pub semantic_default: String,
    /// NDCG@10 of the chosen model's hybrid and of BM25, as the I27 `Metrics` JSON names them.
    pub hybrid_ndcg10: f64,
    pub bm25_ndcg10: f64,
    pub notes: Vec<String>,
}

/// Applies the I28 rules. A candidate wins only if it beats the default by >= 3 points and its
/// embed time is at most 2x the default's; among winners the highest NDCG@10 is chosen.
pub fn decide(set: &[EvalQuery], res: &Results) -> Result<Decision, String> {
    let defaults: Vec<&ModelRun> = res.models.iter().filter(|m| m.default).collect();
    let [def] = defaults.as_slice() else {
        return Err("exactly one model must be marked default".into());
    };
    let bm25 = score(set, &res.bm25)?;
    let mut reports = Vec::new();
    for m in &res.models {
        if !(m.embed_ms.is_finite() && m.embed_ms > 0.0) {
            return Err(format!("model {}: embed_ms must be positive", m.model));
        }
        let hybrid = score(set, &m.hybrid)?;
        let b1 = m.hybrid_b1.as_ref().map(|r| score(set, r)).transpose()?;
        let b1_retention = b1.map(|b| {
            if hybrid.ndcg10 > 0.0 {
                b.ndcg10 / hybrid.ndcg10
            } else {
                0.0
            }
        });
        reports.push(ModelReport {
            model: m.model.clone(),
            hybrid,
            embed_ms: m.embed_ms,
            b1,
            b1_retention,
        });
    }
    let base = reports
        .iter()
        .find(|r| r.model == def.model)
        .ok_or("default model missing")?
        .clone();
    let mut notes = Vec::new();
    let mut chosen = base.clone();
    for r in &reports {
        if r.model == base.model {
            continue;
        }
        let gain = r.hybrid.ndcg10 - base.hybrid.ndcg10;
        let ratio = r.embed_ms / base.embed_ms;
        if gain >= WIN_POINTS && ratio <= MAX_EMBED_RATIO {
            if r.hybrid.ndcg10 > chosen.hybrid.ndcg10 {
                chosen = r.clone();
            }
        } else {
            notes.push(format!(
                "{}: {gain:+.1} NDCG@10 points at {ratio:.2}x embed time does not win \
                 (needs >= {WIN_POINTS} within {MAX_EMBED_RATIO}x)",
                r.model
            ));
        }
    }
    let daemon_b1 = match chosen.b1_retention {
        Some(x) if x >= B1_RETENTION => true,
        Some(x) => {
            notes.push(format!(
                "b1 retention {:.1}% < 93%: i8 only in the daemon",
                x * 100.0
            ));
            false
        }
        None => {
            notes.push("b1 retention not measured: i8 only in the daemon".into());
            false
        }
    };
    let semantic_default = if chosen.hybrid.ndcg10 >= bm25.ndcg10 + HYBRID_POINTS {
        "hybrid"
    } else {
        notes.push(format!(
            "hybrid {:.1} < BM25-only {:.1} + {HYBRID_POINTS}: mode = semantic defaults to BM25",
            chosen.hybrid.ndcg10, bm25.ndcg10
        ));
        "bm25"
    };
    Ok(Decision {
        bm25,
        chosen: chosen.model.clone(),
        daemon_b1,
        semantic_default: semantic_default.into(),
        hybrid_ndcg10: chosen.hybrid.ndcg10,
        bm25_ndcg10: bm25.ndcg10,
        models: reports,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }
    fn set<'a>(rel: &[&'a str]) -> BTreeSet<&'a str> {
        rel.iter().copied().collect()
    }

    #[test]
    fn ndcg_and_recall() {
        let rel = set(&["a", "b"]);
        assert!((ndcg_at_k(&s(&["a", "b"]), &rel, 10) - 1.0).abs() < 1e-12);
        let late = ndcg_at_k(&s(&["x", "a", "b"]), &rel, 10);
        assert!(late < 1.0 && late > 0.0);
        assert_eq!(recall_at_k(&s(&["a", "x"]), &rel, 10), 0.5);
        assert_eq!(recall_at_k(&s(&["x", "a"]), &rel, 1), 0.0);
        // duplicates cannot inflate the score
        assert_eq!(recall_at_k(&s(&["a", "a", "a"]), &rel, 10), 0.5);
        assert!(ndcg_at_k(&s(&["a", "a", "a"]), &rel, 10) < 1.0);
    }

    const LOG: &str = "\x1eabcdef1234\x1ffeat(x): add the thing quickly (#12)\x1f\x1f\nsrc/a.rs\nCargo.lock\nsrc/gone.rs\n\
\x1e1111111aaa\x1fMerge pull request #5 from o/b\x1f\nFix the parser overflow bug\n\x1f\nsrc/b.rs\n\
\x1e2222222bbb\x1fwip\x1f\x1f\nsrc/a.rs\n\
\x1e3333333ccc\x1ffix: add the thing quickly\x1f\x1f\nsrc/a.rs\n\
\x1ezz\x1fbad hash here ok\x1f\x1f\nsrc/a.rs\n";

    #[test]
    fn parses_and_labels() {
        let c = parse_log(LOG);
        assert_eq!(c.len(), 4);
        assert_eq!(c[0].title, "add the thing quickly");
        assert_eq!(c[1].title, "Fix the parser overflow bug");
        let head: BTreeSet<String> = ["src/a.rs", "src/b.rs", "Cargo.lock"]
            .map(String::from)
            .into();
        let q = label(&c, &head, 200);
        // wip dropped, duplicate title dropped, lock file and deleted file not relevant
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].relevant, vec!["src/a.rs"]);
        assert_eq!(q[1].relevant, vec!["src/b.rs"]);
        assert_eq!(q[0].id, "q001");
    }

    #[test]
    fn sampling_is_even_and_deterministic() {
        let head: BTreeSet<String> = ["f.rs".to_string()].into();
        let c: Vec<Candidate> = (0..1000)
            .map(|i| Candidate {
                commit: format!("{i:040x}"),
                title: format!("change number {i} of things"),
                files: vec!["f.rs".into()],
            })
            .collect();
        let a = label(&c, &head, 200);
        assert_eq!(a.len(), 200);
        assert_eq!(a, label(&c, &head, 200));
        assert_eq!(a[1].commit, format!("{:040x}", 5));
    }

    fn eval() -> Vec<EvalQuery> {
        (1..=4)
            .map(|i| EvalQuery {
                id: format!("q{i}"),
                query: format!("query {i}"),
                relevant: vec![format!("f{i}")],
                commit: "abcdef0".into(),
            })
            .collect()
    }
    /// A system that finds `hits` of the 4 queries at rank 1.
    fn sys(hits: usize) -> Rankings {
        (1..=4)
            .map(|i| {
                let first = if i <= hits {
                    format!("f{i}")
                } else {
                    "zz".into()
                };
                (format!("q{i}"), vec![first])
            })
            .collect()
    }
    fn run(model: &str, default: bool, ms: f64, hits: usize, b1: Option<usize>) -> ModelRun {
        ModelRun {
            model: model.into(),
            default,
            embed_ms: ms,
            hybrid: sys(hits),
            hybrid_b1: b1.map(sys),
        }
    }

    #[test]
    fn candidate_wins_only_within_time_and_gain() {
        let set = eval();
        let mk = |ms| Results {
            bm25: sys(1),
            models: vec![
                run("jina", true, 10.0, 2, Some(2)),
                run("cand", false, ms, 4, Some(4)),
            ],
        };
        assert_eq!(decide(&set, &mk(19.0)).unwrap().chosen, "cand");
        let slow = decide(&set, &mk(25.0)).unwrap();
        assert_eq!(slow.chosen, "jina");
        assert!(slow.notes.iter().any(|n| n.contains("does not win")));
        // gain below 3 points
        let r = Results {
            bm25: sys(1),
            models: vec![
                run("jina", true, 10.0, 2, None),
                run("cand", false, 10.0, 2, None),
            ],
        };
        assert_eq!(decide(&set, &r).unwrap().chosen, "jina");
    }

    #[test]
    fn b1_retention_and_hybrid_defaults() {
        let set = eval();
        let r = Results {
            bm25: sys(1),
            models: vec![run("jina", true, 10.0, 4, Some(4))],
        };
        let d = decide(&set, &r).unwrap();
        assert!(d.daemon_b1);
        assert_eq!(d.semantic_default, "hybrid");
        // retention 3/4 = 75% < 93%
        let r = Results {
            bm25: sys(1),
            models: vec![run("jina", true, 10.0, 4, Some(3))],
        };
        assert!(!decide(&set, &r).unwrap().daemon_b1);
        // hybrid not 5 points above BM25
        let r = Results {
            bm25: sys(4),
            models: vec![run("jina", true, 10.0, 4, None)],
        };
        let d = decide(&set, &r).unwrap();
        assert_eq!(d.semantic_default, "bm25");
        assert!(!d.daemon_b1);
    }

    #[test]
    fn rejects_bad_inputs() {
        let set = eval();
        let mut bad = sys(1);
        bad.insert("nope".into(), vec![]);
        assert!(score(&set, &bad).is_err());
        let r = Results {
            bm25: sys(1),
            models: vec![run("a", false, 1.0, 1, None)],
        };
        assert!(decide(&set, &r).is_err());
        let r = Results {
            bm25: sys(1),
            models: vec![run("a", true, 0.0, 1, None)],
        };
        assert!(decide(&set, &r).is_err());
    }
}
