fn read(root: &Path) {
    let _ = std::fs::read(root.join("a/b.txt"));
}
