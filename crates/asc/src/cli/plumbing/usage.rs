//! The usage texts of git 2.55 for the object plumbing, as it prints them after a usage error.

/// `git ls-files -h`.
pub const LS_FILES: &str = r#"usage: git ls-files [<options>] [<file>...]

    -z                    separate paths with the NUL character
    -t                    identify the file status with tags
    -v                    use lowercase letters for 'assume unchanged' files
    -f                    use lowercase letters for 'fsmonitor clean' files
    -c, --[no-]cached     show cached files in the output (default)
    -d, --[no-]deleted    show deleted files in the output
    -m, --[no-]modified   show modified files in the output
    -o, --[no-]others     show other files in the output
    -i, --[no-]ignored    show ignored files in the output
    -s, --[no-]stage      show staged contents' object name in the output
    -k, --[no-]killed     show files on the filesystem that need to be removed
    --[no-]directory      show 'other' directories' names only
    --[no-]eol            show line endings of files
    --[no-]empty-directory
                          don't show empty directories
    -u, --[no-]unmerged   show unmerged files in the output
    --[no-]resolve-undo   show resolve-undo information
    -x, --exclude <pattern>
                          skip files matching pattern
    -X, --exclude-from <file>
                          read exclude patterns from <file>
    --[no-]exclude-per-directory <file>
                          read additional per-directory exclude patterns in <file>
    --exclude-standard    add the standard git exclusions
    --full-name           make the output relative to the project top directory
    --[no-]recurse-submodules
                          recurse through submodules
    --[no-]error-unmatch  if any <file> is not in the index, treat this as an error
    --[no-]with-tree <tree-ish>
                          pretend that paths removed since <tree-ish> are still present
    --[no-]abbrev[=<n>]   use <n> digits to display object names
    --[no-]debug          show debugging data
    --[no-]deduplicate    suppress duplicate entries
    --[no-]sparse         show sparse directories in the presence of a sparse index
    --format <format>     format to use for the output
"#;

/// `git ls-tree -h`.
pub const LS_TREE: &str = r#"usage: git ls-tree [<options>] <tree-ish> [<path>...]

    -d                    only show trees
    -r                    recurse into subtrees
    -t                    show trees when recursing
    -z                    terminate entries with NUL byte
    -l, --long            include object size
    --name-only           list only filenames
    --name-status         list only filenames
    --object-only         list only objects
    --[no-]full-name      use full path names
    --[no-]full-tree      list entire tree; not just current directory (implies --full-name)
    --format <format>     format to use for the output
    --[no-]abbrev[=<n>]   use <n> digits to display object names
"#;

/// `git cat-file -h`.
pub const CAT_FILE: &str = r#"usage: git cat-file <type> <object>
   or: git cat-file (-e | -p | -t | -s) <object>
   or: git cat-file (--textconv | --filters)
                    [<rev>:<path|tree-ish> | --path=<path|tree-ish> <rev>]
   or: git cat-file (--batch | --batch-check | --batch-command) [--batch-all-objects]
                    [--buffer] [--follow-symlinks] [--unordered]
                    [--textconv | --filters] [-Z]

Check object existence or emit object contents
    -e                    check if <object> exists
    -p                    pretty-print <object> content

Emit [broken] object attributes
    -t                    show object type (one of 'blob', 'tree', 'commit', 'tag', ...)
    -s                    show object size
    --[no-]use-mailmap    use mail map file
    --[no-]mailmap ...    alias of --use-mailmap

Batch objects requested on stdin (or --batch-all-objects)
    --batch[=<format>]    show full <object> or <rev> contents
    --batch-check[=<format>]
                          like --batch, but don't emit <contents>
    -Z                    stdin and stdout is NUL-terminated
    --batch-command[=<format>]
                          read commands from stdin
    --batch-all-objects   with --batch[-check]: ignores stdin, batches all known objects

Change or optimize batch output
    --[no-]buffer         buffer --batch output
    --[no-]follow-symlinks
                          follow in-tree symlinks
    --[no-]unordered      do not order objects before emitting them

Emit object (blob or tree) with conversion or filter (stand-alone, or with batch)
    --textconv            run textconv on object's content
    --filters             run filters on object's content
    --[no-]path blob|tree use a <path> for (--textconv | --filters); Not with 'batch'
    --[no-]filter <args>  object filtering
"#;

/// `git hash-object -h`.
pub const HASH_OBJECT: &str = r#"usage: git hash-object [-t <type>] [-w] [--path=<file> | --no-filters]
                       [--stdin [--literally]] [--] <file>...
   or: git hash-object [-t <type>] [-w] --stdin-paths [--no-filters]

    -t <type>             object type
    -w                    write the object into the object database
    --[no-]stdin          read the object from stdin
    --[no-]stdin-paths    read file names from stdin
    --no-filters          store file as is without filters
    --filters             opposite of --no-filters
    --[no-]literally      just hash any random garbage to create corrupt objects for debugging Git
    --[no-]path <file>    process file as it were from this path
"#;

/// `git update-index -h`.
pub const UPDATE_INDEX: &str = r#"usage: git update-index [<options>] [--] [<file>...]

    -q                    continue refresh even when index needs update
    --[no-]ignore-submodules
                          refresh: ignore submodules
    --[no-]add            do not ignore new files
    --[no-]replace        let files replace directories and vice-versa
    --[no-]remove         notice files missing from worktree
    --[no-]unmerged       refresh even if index contains unmerged entries
    --refresh             refresh stat information
    --really-refresh      like --refresh, but ignore assume-unchanged setting
    --cacheinfo <mode>,<object>,<path>
                          add the specified entry to the index
    --chmod (+|-)x        override the executable bit of the listed files
    --assume-unchanged    mark files as "not changing"
    --no-assume-unchanged clear assumed-unchanged bit
    --skip-worktree       mark files as "index-only"
    --no-skip-worktree    clear skip-worktree bit
    --[no-]ignore-skip-worktree-entries
                          do not touch index-only entries
    --[no-]info-only      add to index only; do not add content to object database
    --[no-]force-remove   remove named paths even if present in worktree
    -z                    with --stdin: input lines are terminated by null bytes
    --stdin               read list of paths to be updated from standard input
    --index-info          add entries from standard input to the index
    --unresolve           repopulate stages #2 and #3 for the listed paths
    -g, --again           only update entries that differ from HEAD
    --[no-]ignore-missing ignore files missing from worktree
    --[no-]verbose        report actions to standard output
    --clear-resolve-undo  (for porcelains) forget saved unresolved conflicts
    --[no-]index-version <n>
                          write index in this format
    --[no-]show-index-version
                          report on-disk index format version
    --[no-]split-index    enable or disable split index
    --[no-]untracked-cache
                          enable/disable untracked cache
    --[no-]test-untracked-cache
                          test if the filesystem supports untracked cache
    --[no-]force-untracked-cache
                          enable untracked cache without testing the filesystem
    --[no-]force-write-index
                          write out the index even if is not flagged as changed
    --[no-]fsmonitor      enable or disable file system monitor
    --fsmonitor-valid     mark files as fsmonitor valid
    --no-fsmonitor-valid  clear fsmonitor valid bit
"#;

/// `git write-tree -h`.
pub const WRITE_TREE: &str = r#"usage: git write-tree [--missing-ok] [--prefix=<prefix>/]

    --[no-]missing-ok     allow missing objects
    --[no-]prefix <prefix>/
                          write tree object for a subdirectory <prefix>
"#;

/// `git read-tree -h`.
pub const READ_TREE: &str = r#"usage: git read-tree [(-m [--trivial] [--aggressive] | --reset | --prefix=<prefix>)
                     [-u | -i]] [--index-output=<file>] [--no-sparse-checkout]
                     (--empty | <tree-ish1> [<tree-ish2> [<tree-ish3>]])

    --index-output <file> write resulting index to <file>
    --[no-]empty          only empty the index
    -v, --[no-]verbose    be verbose

Merging
    -m                    perform a merge in addition to a read
    --[no-]trivial        3-way merge if no file level merging required
    --[no-]aggressive     3-way merge in presence of adds and removes
    --[no-]reset          same as -m, but discard unmerged entries
    --prefix <subdirectory>/
                          read the tree into the index under <subdirectory>/
    -u                    update working tree with merge result
    --exclude-per-directory <gitignore>
                          allow explicitly ignored files to be overwritten
    -i                    don't check the working tree after merging
    -n, --[no-]dry-run    don't update the index or the work tree
    --no-sparse-checkout  skip applying sparse checkout filter
    --sparse-checkout     opposite of --no-sparse-checkout
    --[no-]debug-unpack   debug unpack-trees
    --[no-]recurse-submodules[=<checkout>]
                          control recursive updating of submodules
    -q, --[no-]quiet      suppress feedback messages
"#;
