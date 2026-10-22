fn init(root: &Path) {
    // lint:allow-worktree-fs: a new workspace has no tracked paths to protect.
    let _ = std::fs::write(root.join(".ascignore"), b""); // lint:allow-worktree-fs
}
