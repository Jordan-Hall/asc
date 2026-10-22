fn remove(root: &Path, path: &RepoPath) {
    let _ = std::fs::remove_file(fs_path(root, path));
}
