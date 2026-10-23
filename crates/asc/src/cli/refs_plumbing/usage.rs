//! The usage texts of git 2.55 for the ref plumbing, as it prints them after a usage error.

/// `git for-each-ref -h`.
pub const FOR_EACH_REF: &str = r#"usage: git for-each-ref [--count=<count>] [--shell|--perl|--python|--tcl]
                                [(--sort=<key>)...] [--format=<format>]
                                [--include-root-refs] [--points-at=<object>]
                                [--merged[=<object>]] [--no-merged[=<object>]]
                                [--contains[=<object>]] [--no-contains[=<object>]]
                                [(--exclude=<pattern>)...] [--start-after=<marker>]
                                [ --stdin | (<pattern>...)]

    -s, --[no-]shell      quote placeholders suitably for shells
    -p, --[no-]perl       quote placeholders suitably for perl
    --[no-]python         quote placeholders suitably for python
    --[no-]tcl            quote placeholders suitably for Tcl
    --[no-]omit-empty     do not output a newline after empty formatted refs

    --[no-]count <n>      show only <n> matched refs
    --[no-]format <format>
                          format to use for the output
    --[no-]start-after <marker>
                          start iteration after the provided marker
    --[no-]color[=<when>] respect format colors
    --[no-]exclude <pattern>
                          exclude refs which match pattern
    --[no-]sort <key>     field name to sort on
    --[no-]points-at <object>
                          print only refs which points at the given object
    --merged <commit>     print only refs that are merged
    --no-merged <commit>  print only refs that are not merged
    --contains <commit>   print only refs which contain the commit
    --no-contains <commit>
                          print only refs which don't contain the commit
    --[no-]ignore-case    sorting and filtering are case insensitive
    --[no-]stdin          read reference patterns from stdin
    --[no-]include-root-refs
                          also include HEAD ref and pseudorefs
"#;

/// `git show-ref -h`.
pub const SHOW_REF: &str = r#"usage: git show-ref [--head] [-d | --dereference]
                    [-s | --hash[=<n>]] [--abbrev[=<n>]] [--branches] [--tags]
                    [--] [<pattern>...]
   or: git show-ref --verify [-q | --quiet] [-d | --dereference]
                    [-s | --hash[=<n>]] [--abbrev[=<n>]]
                    [--] [<ref>...]
   or: git show-ref --exclude-existing[=<pattern>]
   or: git show-ref --exists <ref>

    --[no-]tags           only show tags (can be combined with --branches)
    --[no-]branches       only show branches (can be combined with --tags)
    --[no-]exists         check for reference existence without resolving
    --[no-]verify         stricter reference checking, requires exact ref path
    --[no-]head           show the HEAD reference, even if it would be filtered out
    -d, --[no-]dereference
                          dereference tags into object IDs
    -s, --[no-]hash[=<n>] only show SHA1 hash using <n> digits
    --[no-]abbrev[=<n>]   use <n> digits to display object names
    -q, --[no-]quiet      do not print results to stdout (useful with --verify)
    --exclude-existing[=<pattern>]
                          show refs from stdin that aren't in local repository
"#;

/// `git symbolic-ref -h`.
pub const SYMBOLIC_REF: &str = r#"usage: git symbolic-ref [-m <reason>] <name> <ref>
   or: git symbolic-ref [-q] [--short] [--no-recurse] <name>
   or: git symbolic-ref --delete [-q] <name>

    -q, --[no-]quiet      suppress error message for non-symbolic (detached) refs
    -d, --[no-]delete     delete symbolic ref
    --[no-]short          shorten ref output
    --[no-]recurse        recursively dereference (default)
    -m <reason>           reason of the update
"#;

/// `git update-ref -h`.
pub const UPDATE_REF: &str = r#"usage: git update-ref [<options>] -d <refname> [<old-oid>]
   or: git update-ref [<options>]    <refname> <new-oid> [<old-oid>]
   or: git update-ref [<options>] --stdin [-z] [--batch-updates]

    -m <reason>           reason of the update
    -d                    delete the reference
    --no-deref            update <refname> not the one it points to
    --deref               opposite of --no-deref
    -z                    stdin has NUL-terminated arguments
    --[no-]stdin          read updates from stdin
    --[no-]create-reflog  create a reflog
    -0, --[no-]batch-updates
                          batch reference updates
"#;

/// `git check-ref-format -h`.
pub const CHECK_REF_FORMAT: &str = r#"usage: git check-ref-format [--normalize] [<options>] <refname>
   or: git check-ref-format --branch <branchname-shorthand>"#;

/// `git var -h`.
pub const VAR: &str = r#"usage: git var (-l | <variable>)"#;
