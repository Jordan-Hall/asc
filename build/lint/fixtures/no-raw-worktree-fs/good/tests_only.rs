fn code() {}

#[cfg(test)]
mod tests {
    #[test]
    fn fixture() {
        std::fs::write(root.join("a.txt"), b"x").unwrap();
    }
}
