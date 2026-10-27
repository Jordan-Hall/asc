//! The scheduled workflow that goes with `.asc/deps.toml`.
//!
//! A repository that sets `schedule` gets `.asc/workflows/deps.toml`: a workflow of the repository
//! (docs/WORKFLOWS.md) with a schedule trigger, a manual trigger and one command step that runs
//! `asc deps propose --remote <this repository>` against this server. The definition is data like any other workflow: the server
//! writes it into the same changeset as the config, a maintainer reviews and lands it, and a maintainer
//! switches the workflow on. The step holds one repository secret, `deps-token`, a token that may write
//! to the repository; it reaches the program as `DEPS_TOKEN` and the sandbox lets it talk to this server
//! only.

use url::Url;

/// The workflow name, and so the file stem.
pub const NAME: &str = "deps";
/// Where the definition lives.
pub const PATH: &str = ".asc/workflows/deps.toml";
/// The repository secret that holds the token.
pub const SECRET: &str = "deps-token";
/// The environment variable the secret is exposed as.
pub const TOKEN_ENV: &str = "DEPS_TOKEN";

fn valid_repo(r: &str) -> bool {
    let mut parts = r.split('/');
    let ok = |s: &str| {
        !s.is_empty()
            && !s.starts_with('.')
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    matches!((parts.next(), parts.next(), parts.next()), (Some(a), Some(b), None) if ok(a) && ok(b))
}

/// The definition for `repo` (`owner/repo`) on the server at `server` (an https URL), running on
/// `cron`. The text is a valid workflow file.
pub fn render(repo: &str, server: &str, cron: &str) -> Result<String, String> {
    if !valid_repo(repo) {
        return Err("the repository name is not owner/repo".into());
    }
    let url = Url::parse(server).map_err(|_| "the server URL is not a URL".to_owned())?;
    if url.scheme() != "https" {
        return Err("the server URL must be https".into());
    }
    let host = url.host_str().ok_or("the server URL has no host")?;
    let server = server.trim_end_matches('/');
    if cron
        .chars()
        .any(|c| c.is_control() || c == '"' || c == '\\')
    {
        return Err("the schedule holds a character a workflow cannot carry".into());
    }
    let q = |s: &str| format!("{s:?}");
    let remote = format!("{server}/{repo}");
    let args = ["asc", "deps", "propose", "--remote", &remote, "--token-env", TOKEN_ENV]
    .iter()
    .map(|a| q(a))
    .collect::<Vec<_>>()
    .join(", ");
    Ok(format!(
        "# Generated from .asc/deps.toml (the server writes it with SetConfig); change the schedule there.\n\
         # The step needs the repository secret `{SECRET}` and `asc` in [workflows.sandbox] allow.\n\
         description = \"Scan dependencies and propose updates (docs/DEPENDENCIES.md)\"\n\
         max_parallel_runs = 1\n\
         \n\
         [[trigger]]\n\
         kind = \"schedule\"\n\
         cron = {cron}\n\
         \n\
         [[trigger]]\n\
         kind = \"manual\"\n\
         \n\
         [[step]]\n\
         id = \"propose\"\n\
         kind = \"command\"\n\
         command = [{args}]\n\
         secrets = {{ {TOKEN_ENV} = \"{SECRET}\" }}\n\
         egress = [{host}]\n\
         timeout_seconds = 900\n",
        cron = q(cron),
        host = q(host),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use asc_core::workflow::{StepKind, TriggerKind, Workflow};

    #[test]
    fn the_definition_is_a_valid_scheduled_workflow() {
        let text = render("acme/app", "https://asc.example.com/", "0 4 * * 1").unwrap();
        let wf = Workflow::parse(NAME, text.as_bytes()).unwrap();
        assert_eq!(wf.max_parallel_runs, 1);
        let kinds: Vec<_> = wf.triggers.iter().map(|t| t.kind).collect();
        assert_eq!(kinds, [TriggerKind::Schedule, TriggerKind::Manual]);
        assert_eq!(wf.triggers[0].cron.as_ref().unwrap().as_str(), "0 4 * * 1");
        let [step] = &wf.steps[..] else {
            panic!("one step")
        };
        assert_eq!(step.kind, StepKind::Command);
        assert_eq!(
            step.command,
            [
                "asc",
                "deps",
                "propose",
                "--remote",
                "https://asc.example.com/acme/app",
                "--token-env",
                "DEPS_TOKEN"
            ]
        );
        assert_eq!(
            step.secrets.get("DEPS_TOKEN").map(String::as_str),
            Some("deps-token")
        );
        assert_eq!(step.egress, ["asc.example.com"]);
    }

    #[test]
    fn what_cannot_be_carried_is_refused() {
        for (repo, server, cron) in [
            ("acme", "https://h.example", "0 4 * * 1"),
            ("acme/app/x", "https://h.example", "0 4 * * 1"),
            ("acme/.hidden", "https://h.example", "0 4 * * 1"),
            ("acme/app\"", "https://h.example", "0 4 * * 1"),
            ("acme/app", "http://h.example", "0 4 * * 1"),
            ("acme/app", "not a url", "0 4 * * 1"),
            ("acme/app", "https://h.example", "0 4 * * \"1"),
            ("acme/app", "https://h.example", "0 4 * * 1\n"),
        ] {
            assert!(
                render(repo, server, cron).is_err(),
                "{repo} {server} {cron:?}"
            );
        }
        // An address with a port keeps the port in the argument and only the host as egress.
        let text = render("a/b", "https://h.example:8443", "0 4 * * 1").unwrap();
        let wf = Workflow::parse(NAME, text.as_bytes()).unwrap();
        assert_eq!(wf.steps[0].egress, ["h.example"]);
        assert!(
            wf.steps[0]
                .command
                .contains(&"https://h.example:8443/a/b".to_owned())
        );
    }
}
