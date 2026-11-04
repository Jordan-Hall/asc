//! Action keys: one blake3 over the tool, its key material and the input digest. A key never
//! holds an absolute path; each adapter makes its material repository-relative first.

use std::fmt;

/// The key context of action keys.
pub const KEY_CONTEXT: &str = "asc build action v1";

/// The key of one cached action.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActionKey(pub [u8; 32]);

impl ActionKey {
    /// Starts a key for `tool` (`rustc`, `vite`, `run:<name>`…). Tools never share entries:
    /// the tool is the first thing hashed.
    pub fn builder(tool: &str) -> KeyBuilder {
        let mut h = blake3::Hasher::new_derive_key(KEY_CONTEXT);
        put(&mut h, tool.as_bytes());
        KeyBuilder(h)
    }

    pub fn parse(text: &str) -> Option<Self> {
        crate::unhex(text).map(Self)
    }
}

impl fmt::Display for ActionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&crate::hex(&self.0))
    }
}

impl fmt::Debug for ActionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ActionKey({})", &crate::hex(&self.0)[..12])
    }
}

/// Length-prefixed, so no two sequences of parts hash alike.
fn put(h: &mut blake3::Hasher, bytes: &[u8]) {
    h.update(&(bytes.len() as u64).to_le_bytes());
    h.update(bytes);
}

/// Adds key material in order.
pub struct KeyBuilder(blake3::Hasher);

impl KeyBuilder {
    /// A named part: a version, a flag, an environment variable.
    pub fn part(mut self, name: &str, value: &[u8]) -> Self {
        put(&mut self.0, name.as_bytes());
        put(&mut self.0, value);
        self
    }

    /// The `asc inputs --digest` of the action's inputs.
    pub fn inputs(self, digest: &[u8; 32]) -> Self {
        self.part("inputs", digest)
    }

    pub fn finish(self) -> ActionKey {
        ActionKey(*self.0.finalize().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_are_separated_and_ordered() {
        let k = |f: fn(KeyBuilder) -> KeyBuilder| f(ActionKey::builder("rustc")).finish();
        let a = k(|b| b.part("ab", b"c"));
        assert_ne!(a, k(|b| b.part("a", b"bc")));
        assert_ne!(a, k(|b| b.part("ab", b"c").part("x", b"")));
        assert_eq!(a, k(|b| b.part("ab", b"c")));
        assert_ne!(
            k(|b| b.part("x", b"1").part("y", b"2")),
            k(|b| b.part("y", b"2").part("x", b"1"))
        );
        assert_ne!(
            ActionKey::builder("vite").inputs(&[0; 32]).finish(),
            ActionKey::builder("rustc").inputs(&[0; 32]).finish(),
            "tools never share entries"
        );
        assert_eq!(ActionKey::parse(&a.to_string()), Some(a));
    }
}
