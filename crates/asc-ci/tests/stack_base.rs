mod stack_base {
    use asc_ci::stack_base::{PrBase, StackBase, StackBaseError};

    fn pr(mb: Option<&str>, d: &str) -> PrBase {
        PrBase {
            merge_base: mb.map(str::to_owned),
            default_branch: d.to_owned(),
        }
    }

    #[test]
    fn stacked_pr_uses_parent_sha_and_still_writes_origin() {
        let sha = "a".repeat(40);
        let b = StackBase::resolve(&pr(Some(&sha), "master")).unwrap();
        assert_eq!(b.turbo_base.as_deref(), Some(sha.as_str()));
        assert_eq!(b.origin_ref, "origin/master");
    }

    #[test]
    fn no_merge_base_leaves_fallback() {
        let b = StackBase::resolve(&pr(None, "main")).unwrap();
        assert_eq!(b.turbo_base, None);
        assert_eq!(b.origin_ref, "origin/main");
    }

    #[test]
    fn hostile_merge_base_is_rejected() {
        for bad in ["abc", "master", &format!("{}\nX=1", "a".repeat(40)), &"A".repeat(40)] {
            assert_eq!(
                StackBase::resolve(&pr(Some(bad), "master")),
                Err(StackBaseError::MergeBase)
            );
        }
    }

    #[test]
    fn hostile_default_branch_is_rejected() {
        for bad in ["", "-x", "a..b", "a\nb", "a b", "x/"] {
            assert_eq!(
                StackBase::resolve(&pr(None, bad)),
                Err(StackBaseError::DefaultBranch)
            );
        }
    }
}
