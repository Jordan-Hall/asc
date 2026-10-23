//! Output formats of `asc affected` (SPEC §12A.9): `list`, `json`, `turbo`, `nx`, `matrix` and
//! `gha`. Pure functions of a [`Report`], so the golden tests need no repository.
//!
//! Project names come from untrusted manifests: text formats escape control characters, and
//! the `turbo`, `nx` and `gha` formats never emit a name that could split a filter or inject a
//! line into `$GITHUB_OUTPUT`.

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

/// A name a turbo filter or an nx project list can carry.
fn plain_name(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(|c| c.is_control() || c.is_whitespace() || c == ',')
}

/// The names for `turbo` and `nx`, and the ids no Node package makes visible.
fn visible(r: &Report) -> (Vec<&str>, Vec<&str>) {
    let (mut names, mut hidden) = (Vec::new(), Vec::new());
    for row in &r.rows {
        if row.turbo_visible {
            if plain_name(&row.name) {
                names.push(row.name.as_str());
            } else {
                hidden.push(row.id.as_str());
            }
        } else if row.ecosystem != "node" && !row.contained_by_node {
            hidden.push(row.id.as_str());
        }
    }
    (names, hidden)
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
    let (names, _) = visible(r);
    let filters: Vec<String> = names.iter().map(|n| format!("--filter={n}")).collect();
    format!("{}\n", filters.join(" "))
}

/// `nx`: comma-separated names for `nx run-many --projects=`.
pub fn nx(r: &Report) -> String {
    let (names, _) = visible(r);
    format!("{}\n", names.join(","))
}

/// `matrix`: `{"include":[...]}`.
pub fn matrix(r: &Report, by: MatrixBy) -> Value {
    let row = |p: &Row| {
        json!({"project": p.id, "name": p.name, "ecosystem": p.ecosystem, "root": p.root,
               "workspace": p.workspace})
    };
    let include: Vec<Value> = match by {
        MatrixBy::Project => r.rows.iter().map(row).collect(),
        MatrixBy::Workspace => {
            let mut seen = std::collections::BTreeSet::new();
            r.rows
                .iter()
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
            r.rows
                .iter()
                .filter(|p| seen.insert(p.ecosystem))
                .map(|p| {
                    json!({"project": p.ecosystem, "name": p.ecosystem,
                           "ecosystem": p.ecosystem, "root": ".", "workspace": "."})
                })
                .collect()
        }
    };
    json!({ "include": include })
}

/// `gha`: lines for `$GITHUB_OUTPUT`.
pub fn gha(r: &Report, by: MatrixBy) -> String {
    let ids: Vec<&str> = r.rows.iter().map(|p| p.id.as_str()).collect();
    let has = |eco: &str| r.all || r.rows.iter().any(|p| p.ecosystem == eco);
    let (names, _) = visible(r);
    let filters: Vec<String> = names.iter().map(|n| format!("--filter={n}")).collect();
    let mut out = String::new();
    let mut line = |k: &str, v: String| {
        out.push_str(k);
        out.push('=');
        out.push_str(&v);
        out.push('\n');
    };
    line("affected", Value::from(ids).to_string());
    line("count", r.rows.len().to_string());
    line("all", r.all.to_string());
    for eco in ["rust", "python", "dotnet", "node"] {
        line(eco, has(eco).to_string());
    }
    line("turbo_filter", filters.join(" "));
    line("nx_projects", names.join(","));
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
    fn names_that_split_filters_are_hidden() {
        let r = report(vec![row("node", "a b", "pkg/a"), row("node", "x,y", "pkg/b")]);
        assert_eq!(turbo(&r), "\n");
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
                "affected=[\"node:a\"]\ncount=1\nall=false\nrust=false\npython=false\n",
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
}
