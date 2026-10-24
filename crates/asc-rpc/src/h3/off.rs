//! The client without the `http3` feature: it never upgrades.

use std::sync::OnceLock;

use bytes::Bytes;
use http::{HeaderMap, Request, Uri};

use super::Attempt;

/// Unused without `http3`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClientLimits;

/// Never upgrades.
#[derive(Debug, Default)]
pub struct Upgrader;

impl Upgrader {
    pub fn shared() -> &'static Self {
        static ONE: OnceLock<Upgrader> = OnceLock::new();
        ONE.get_or_init(Upgrader::default)
    }

    pub fn note_response(&self, _uri: &Uri, _headers: &HeaderMap) {}

    pub async fn send(&self, _req: &Request<Bytes>, _max_body: usize) -> Attempt {
        Attempt::Fallback
    }
}
