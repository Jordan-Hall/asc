fn remove(gate: &SafeRoot, path: &RepoPath) {
    // fs_path( is mentioned in a comment only.
    if let Some(abs) = gate.resolve(path) {
        let _ = std::fs::remove_file(abs);
    }
    let _ = std::fs::read(root.join(".asc").join("info"));
    let _ = std::fs::read(root.join(".git"));
}
