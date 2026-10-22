//! Author/committer/tagger signature (SPEC §2.6).

use serde::{Deserialize, Serialize};

/// A git-style identity and timestamp. Name and email are raw bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    pub name: Vec<u8>,
    pub email: Vec<u8>,
    /// Unix seconds.
    pub time: i64,
    /// Timezone offset in minutes east of UTC.
    pub tz_min: i16,
}
