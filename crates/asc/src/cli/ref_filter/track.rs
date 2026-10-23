//! What a local branch tracks: `@{upstream}` and `@{push}` from `branch.<n>.*`, `remote.<r>.*`
//! and `push.default`, as git's `branch_get_upstream` and `branch_get_push` decide them.

use crate::cli::config::ConfigSnapshot;

/// A fetch or push refspec, `[+]<src>:<dst>`.
struct RefSpec<'a> {
    src: &'a str,
    dst: &'a str,
}

impl<'a> RefSpec<'a> {
    fn parse(text: &'a str) -> Option<Self> {
        let text = text.strip_prefix('+').unwrap_or(text);
        let (src, dst) = text.split_once(':')?;
        Some(Self { src, dst })
    }

    /// The destination `name` maps to, `*` standing for the same text on both sides.
    fn apply(&self, name: &str) -> Option<String> {
        match (self.src.split_once('*'), self.dst.split_once('*')) {
            (Some((head, tail)), Some((dst_head, dst_tail))) => {
                let rest = name.strip_prefix(head)?;
                let middle = rest.strip_suffix(tail)?;
                Some(format!("{dst_head}{middle}{dst_tail}"))
            }
            (None, None) => (self.src == name).then(|| self.dst.to_owned()),
            _ => None,
        }
    }
}

fn apply_all(specs: &[String], name: &str) -> Option<String> {
    specs
        .iter()
        .filter_map(|s| RefSpec::parse(s))
        .find_map(|s| s.apply(name))
}

/// The tracking facts of one local branch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tracking {
    /// `branch.<n>.remote`, when set.
    pub remote: Option<String>,
    /// `branch.<n>.merge`: the branch on the remote.
    pub merge: Option<String>,
    /// The local ref the upstream is stored as (`refs/remotes/origin/master`).
    pub upstream: Option<String>,
}

/// `branch.<branch>.remote/merge` mapped through the remote's fetch refspecs.
pub fn upstream(config: &ConfigSnapshot, branch: &str) -> Tracking {
    let remote = config.get(&format!("branch.{branch}.remote"));
    let merge = config.get(&format!("branch.{branch}.merge"));
    let upstream = match (&remote, &merge) {
        (Some(r), Some(m)) if r == "." => Some(m.clone()),
        (Some(r), Some(m)) => apply_all(&config.get_all(&format!("remote.{r}.fetch")), m),
        _ => None,
    };
    Tracking {
        remote,
        merge,
        upstream,
    }
}

/// The remote a push of `branch` goes to, and whether config names it.
pub fn push_remote(config: &ConfigSnapshot, branch: &str) -> (String, bool) {
    for key in [
        format!("branch.{branch}.pushRemote"),
        "remote.pushDefault".to_owned(),
        format!("branch.{branch}.remote"),
    ] {
        if let Some(r) = config.get(&key) {
            return (r, true);
        }
    }
    ("origin".to_owned(), false)
}

/// The remote a fetch of `branch` comes from, and whether config names it.
pub fn fetch_remote(config: &ConfigSnapshot, branch: &str) -> (String, bool) {
    match config.get(&format!("branch.{branch}.remote")) {
        Some(r) => (r, true),
        None => ("origin".to_owned(), false),
    }
}

/// `@{push}` of `branch` as a local tracking ref; `None` when git has none.
pub fn push_target(config: &ConfigSnapshot, branch: &str) -> Option<String> {
    let (remote, _) = push_remote(config, branch);
    let full = format!("refs/heads/{branch}");
    let destination =
        |dest: &str| apply_all(&config.get_all(&format!("remote.{remote}.fetch")), dest);
    let dest = match apply_all(&config.get_all(&format!("remote.{remote}.push")), &full) {
        Some(d) => d,
        None => full.clone(),
    };
    let up = upstream(config, branch).upstream;
    match config.get("push.default").as_deref() {
        Some("nothing") => None,
        Some("matching" | "current") => destination(&dest),
        Some("upstream" | "tracking") => up,
        _ => {
            let up = up?;
            let cur = destination(&dest)?;
            (cur == up).then_some(cur)
        }
    }
}

/// The branch on the remote that a push of `branch` updates, when config says it explicitly.
pub fn push_remote_ref(config: &ConfigSnapshot, branch: &str) -> Option<String> {
    let (remote, _) = push_remote(config, branch);
    apply_all(
        &config.get_all(&format!("remote.{remote}.push")),
        &format!("refs/heads/{branch}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refspecs_map_names() {
        let spec = RefSpec::parse("+refs/heads/*:refs/remotes/origin/*").unwrap();
        assert_eq!(
            spec.apply("refs/heads/a/b").as_deref(),
            Some("refs/remotes/origin/a/b")
        );
        assert_eq!(spec.apply("refs/tags/x"), None);
        let exact = RefSpec::parse("refs/heads/m:refs/remotes/o/m").unwrap();
        assert_eq!(
            exact.apply("refs/heads/m").as_deref(),
            Some("refs/remotes/o/m")
        );
        assert_eq!(exact.apply("refs/heads/n"), None);
    }
}
