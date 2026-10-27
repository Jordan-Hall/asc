//! `Version.features` for `CodeIntel` (SPEC §23.4.4, unit I21).
//!
//! The MCP `search` tool (SPEC §22.3.2) is listed only when the server advertises `search.v1`,
//! and the server advertises it only while the `CodeIntel` service is actually registered.

use super::rpc::CodeIntelSlot;

/// The `Version.features` entry that lists the MCP `search` tool.
pub const SEARCH_FEATURE: &str = "search.v1";

/// The `Version.features` entry of the `CrossSearch` service.
pub const CROSS_SEARCH_FEATURE: &str = "cross-search.v1";

/// What `Version.features` adds for `CodeIntel`: `search.v1` and `cross-search.v1` (the
/// `CrossSearch` service is mounted beside it) while a handler is registered, nothing otherwise.
pub fn features(slot: &CodeIntelSlot) -> Vec<String> {
    if slot.get().is_some() {
        vec![SEARCH_FEATURE.to_owned(), CROSS_SEARCH_FEATURE.to_owned()]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_without_a_registered_service() {
        assert!(features(&CodeIntelSlot::default()).is_empty());
    }

    #[test]
    fn cross_search_is_advertised_with_search() {
        assert_eq!(CROSS_SEARCH_FEATURE, "cross-search.v1");
    }

    #[test]
    fn feature_name_is_the_spec_literal() {
        assert_eq!(SEARCH_FEATURE, "search.v1");
    }
}
