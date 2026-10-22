//! The detector registry (SPEC §12A.2): each detector unit adds its one line.

use crate::detect::Detector;

/// Every built-in detector, in no particular order.
pub const DETECTORS: &[&dyn Detector] = &[
    &crate::detect::node::Node,
    &crate::detect::python::Python,
    &crate::detect::rust::Rust,
];
