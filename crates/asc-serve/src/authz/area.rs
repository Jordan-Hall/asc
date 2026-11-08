//! The part of asc a call is for, which is the scope a personal access token needs for it.
//!
//! The hub holds a personal access token to one scope per part (`routers/private/asc.go`
//! `ascAreaCategory`): the work on changesets, stacks and their review needs the scope `changes`,
//! agent runs need the scope `runs`, and the rest (code, refs, objects, search, policy, checks,
//! signing, audit, dependencies, storage) needs the scope `repository`. The admission layer runs
//! every RPC inside [`scope`], so the forge authorizer sends the part with each question it asks
//! (`POST /api/internal/asc/authz` `area`) and a token is held to it on a direct connection too,
//! not only through the hub's forwarder.

use std::future::Future;

/// A part of asc, by the scope category the hub holds a token to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Area {
    /// Code, refs, objects and everything that is not one of the others. Git over HTTP and SSH.
    #[default]
    Repository,
    /// Changesets, stacks, review, the merge queue, the inbox and projects.
    Changes,
    /// Agent runs and agent tokens.
    Runs,
}

impl Area {
    /// The area of a gRPC service, by its short name (`Changes` in `/asc.v1.Changes/Get`). The
    /// same table as the hub's forwarder (`services/ascforward/scope.go` `areaOf`).
    #[must_use]
    pub fn of_service(service: &str) -> Self {
        match service {
            "Changes" | "Stacks" | "Review" | "MergeQueue" | "Inbox" | "Projects" => Self::Changes,
            "Agents" | "AgentTokens" => Self::Runs,
            _ => Self::Repository,
        }
    }

    /// The area of a request path: `/asc.v1.<Service>/<Method>` is its service's, anything else
    /// (git over HTTP, the health routes) is [`Area::Repository`].
    #[must_use]
    pub fn of_path(path: &str) -> Self {
        path.strip_prefix("/asc.v1.")
            .and_then(|rest| rest.split_once('/'))
            .map_or(Self::Repository, |(service, _)| Self::of_service(service))
    }

    /// The word the hub takes in `area`; `None` for [`Area::Repository`], which the hub takes as
    /// the default, so a hub that predates the field keeps answering the repository questions.
    #[must_use]
    pub fn wire(self) -> Option<&'static str> {
        match self {
            Self::Repository => None,
            Self::Changes => Some("changes"),
            Self::Runs => Some("runs"),
        }
    }
}

tokio::task_local! {
    static CALL_AREA: Area;
}

/// The area of the call being served; [`Area::Repository`] outside one.
#[must_use]
pub fn current() -> Area {
    CALL_AREA.try_with(|a| *a).unwrap_or_default()
}

/// Runs `fut` with `area` as [`current`].
pub fn scope<F: Future>(area: Area, fut: F) -> impl Future<Output = F::Output> {
    CALL_AREA.scope(area, fut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn services_map_to_the_hubs_areas() {
        for s in [
            "Changes",
            "Stacks",
            "Review",
            "MergeQueue",
            "Inbox",
            "Projects",
        ] {
            assert_eq!(Area::of_service(s), Area::Changes, "{s}");
        }
        for s in ["Agents", "AgentTokens"] {
            assert_eq!(Area::of_service(s), Area::Runs, "{s}");
        }
        for s in [
            "Repo",
            "Storage",
            "Policy",
            "Signing",
            "Audit",
            "DepsService",
            "Ci",
            "",
        ] {
            assert_eq!(Area::of_service(s), Area::Repository, "{s}");
        }
    }

    #[test]
    fn paths() {
        assert_eq!(Area::of_path("/asc.v1.Stacks/List"), Area::Changes);
        assert_eq!(Area::of_path("/asc.v1.Agents/StartRun"), Area::Runs);
        assert_eq!(Area::of_path("/asc.v1.Repo/GetTree"), Area::Repository);
        assert_eq!(Area::of_path("/owner/repo.git/info/refs"), Area::Repository);
        assert_eq!(
            Area::of_path("/asc.v1.Stacks"),
            Area::Repository,
            "no method"
        );
        assert_eq!(Area::of_path("/other.v1.Stacks/List"), Area::Repository);
    }

    #[test]
    fn wire_words() {
        assert_eq!(Area::Repository.wire(), None);
        assert_eq!(Area::Changes.wire(), Some("changes"));
        assert_eq!(Area::Runs.wire(), Some("runs"));
    }

    #[tokio::test]
    async fn scoped_and_default() {
        assert_eq!(current(), Area::Repository);
        assert_eq!(scope(Area::Runs, async { current() }).await, Area::Runs);
        assert_eq!(current(), Area::Repository);
    }
}
