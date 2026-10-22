fn walk(dir: &Path) {
    for e in std::fs::read_dir(dir).unwrap() {
        drop(e);
    }
}
