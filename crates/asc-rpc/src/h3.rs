//! HTTP/3 for asc's own HTTPS requests to `asc serve` (U89b).
//!
//! A response over h1 or h2 may carry `Alt-Svc: h3=":443"`. The [`Upgrader`] remembers that per
//! origin (bounded, with expiry) and sends later requests to the SAME origin over QUIC. Anything
//! that goes wrong sends the caller back to h2 and is remembered, so a network that drops UDP
//! costs one short timeout per backoff.
//!
//! The rules, each with a test:
//! - only an `https` origin is ever upgraded, and only to its own host (see [`alt_svc`]);
//! - certificates are checked by the platform verifier, as reqwest checks them for h2, on TLS 1.3
//!   only; there is no switch that turns verification off;
//! - no 0-RTT, so no request is ever replayable from the handshake;
//! - a request that may have reached the server is replayed on h2 only when its method is
//!   idempotent;
//! - tonic's gRPC stays on h2: this module carries plain HTTP requests only.
//!
//! The whole client is behind the default-on feature `http3`; without it [`Upgrader`] never
//! upgrades and the quinn and h3 crates are not built.

use bytes::Bytes;
use http::{HeaderMap, StatusCode};

#[cfg(feature = "http3")]
pub mod alt_svc;
#[cfg(feature = "http3")]
mod client;
#[cfg(feature = "http3")]
pub use client::{ClientLimits, Upgrader};

#[cfg(not(feature = "http3"))]
mod off;
#[cfg(not(feature = "http3"))]
pub use off::{ClientLimits, Upgrader};

/// A complete response.
#[derive(Debug)]
pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    /// At most the `max_body` asked for.
    pub body: Bytes,
    /// The response had more body than `max_body`.
    pub truncated: bool,
}

/// What [`Upgrader::send`] did.
#[derive(Debug)]
pub enum Attempt {
    /// h3 answered.
    Done(Reply),
    /// h3 was not used or failed where it is safe to ask again over h2.
    Fallback,
    /// h3 failed after the request may have been processed and the method is not idempotent, so it
    /// is not replayed.
    Failed(String),
}
