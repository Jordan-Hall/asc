//! Hydration policy (SPEC §10.2 item 5): processes whose reads must not hydrate placeholders.

use std::collections::BTreeSet;

/// `asc.vfs.denyHydration` default.
pub const DEFAULT_DENY: &str = "SearchProtocolHost.exe;SearchIndexer.exe;SearchFilterHost.exe";
/// Most entries one deny list keeps; the rest of an over-long list is ignored.
pub const MAX_DENY_ENTRIES: usize = 256;
/// Longest image name considered; a longer name is cut (and so matches nothing in practice).
pub const MAX_IMAGE_BYTES: usize = 1024;

/// Whether a read may hydrate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

/// A deny list by process image file name, compared case-insensitively on the last path component.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HydrationPolicy {
    deny: BTreeSet<String>,
}

impl Default for HydrationPolicy {
    fn default() -> Self {
        Self::parse(DEFAULT_DENY)
    }
}

/// The lower-cased last component of an image path (either separator), cut to [`MAX_IMAGE_BYTES`].
pub fn image_name(image: &str) -> String {
    let base = image.rsplit(['\\', '/']).next().unwrap_or(image);
    let mut end = base.len().min(MAX_IMAGE_BYTES);
    while !base.is_char_boundary(end) {
        end -= 1;
    }
    base[..end].trim().to_lowercase()
}

impl HydrationPolicy {
    /// Parses a `;`-separated list. Blank entries are skipped.
    pub fn parse(list: &str) -> Self {
        let deny = list
            .split(';')
            .map(image_name)
            .filter(|s| !s.is_empty())
            .take(MAX_DENY_ENTRIES)
            .collect();
        Self { deny }
    }

    /// A policy that denies nothing.
    pub fn allow_all() -> Self {
        Self {
            deny: BTreeSet::new(),
        }
    }

    pub fn check(&self, image: &str) -> Decision {
        if self.deny.contains(&image_name(image)) {
            Decision::Deny
        } else {
            Decision::Allow
        }
    }

    pub fn len(&self) -> usize {
        self.deny.len()
    }

    pub fn is_empty(&self) -> bool {
        self.deny.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_denies_windows_search() {
        let p = HydrationPolicy::default();
        assert_eq!(
            p.check(r"C:\Windows\System32\SearchProtocolHost.exe"),
            Decision::Deny
        );
        assert_eq!(p.check("searchindexer.EXE"), Decision::Deny);
        assert_eq!(p.check(r"C:\x\code.exe"), Decision::Allow);
        assert_eq!(p.check(""), Decision::Allow);
    }

    #[test]
    fn parse_skips_blanks_and_bounds() {
        let p = HydrationPolicy::parse(" a.exe ;; ;B.EXE;/usr/bin/c ");
        assert_eq!(p.len(), 3);
        assert_eq!(p.check("b.exe"), Decision::Deny);
        assert_eq!(p.check("c"), Decision::Deny);
        let many = (0..1000)
            .map(|i| format!("p{i}"))
            .collect::<Vec<_>>()
            .join(";");
        assert_eq!(HydrationPolicy::parse(&many).len(), MAX_DENY_ENTRIES);
    }
}
