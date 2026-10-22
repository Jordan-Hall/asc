//! Author/committer/tagger signature (SPEC §2.6).

use serde::{Deserialize, Serialize};

/// Why a name or email cannot be rendered into a git ident line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SigError {
    #[error("signature name contains LF, '<', '>' or NUL")]
    Name,
    #[error("signature email contains LF, '<', '>' or NUL")]
    Email,
}

fn clean(b: &[u8]) -> bool {
    !b.iter().any(|c| matches!(c, b'\n' | b'<' | b'>' | 0))
}

/// A git-style identity and timestamp. Name and email are raw bytes that
/// never contain LF, `<`, `>` or NUL, so rendering them cannot inject headers.
/// Fields are private: build one with [`Signature::new`]; decode validates too.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawSignature")]
pub struct Signature {
    name: Vec<u8>,
    email: Vec<u8>,
    /// Unix seconds.
    time: i64,
    /// Timezone offset in minutes east of UTC.
    tz_min: i16,
}

#[derive(Deserialize)]
struct RawSignature {
    name: Vec<u8>,
    email: Vec<u8>,
    time: i64,
    tz_min: i16,
}

impl TryFrom<RawSignature> for Signature {
    type Error = SigError;

    fn try_from(r: RawSignature) -> Result<Self, SigError> {
        Signature::new(r.name, r.email, r.time, r.tz_min)
    }
}

impl Signature {
    /// Rejects a name or email containing LF, `<`, `>` or NUL.
    pub fn new(name: Vec<u8>, email: Vec<u8>, time: i64, tz_min: i16) -> Result<Self, SigError> {
        if !clean(&name) {
            return Err(SigError::Name);
        }
        if !clean(&email) {
            return Err(SigError::Email);
        }
        Ok(Self { name, email, time, tz_min })
    }

    pub fn name(&self) -> &[u8] {
        &self.name
    }

    pub fn email(&self) -> &[u8] {
        &self.email
    }

    /// Unix seconds.
    pub fn time(&self) -> i64 {
        self.time
    }

    /// Minutes east of UTC.
    pub fn tz_min(&self) -> i16 {
        self.tz_min
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_injection_bytes() {
        for bad in [&b"a\nb"[..], b"a<b", b"a>b", b"a\0b"] {
            assert_eq!(Signature::new(bad.to_vec(), b"e".to_vec(), 0, 0), Err(SigError::Name));
            assert_eq!(Signature::new(b"n".to_vec(), bad.to_vec(), 0, 0), Err(SigError::Email));
        }
        assert!(Signature::new(b"Jos\xe9 A".to_vec(), b"a@b".to_vec(), 1, 60).is_ok());
    }

    #[test]
    fn decode_validates() {
        #[derive(Serialize)]
        struct Wire {
            name: Vec<u8>,
            email: Vec<u8>,
            time: i64,
            tz_min: i16,
        }
        let wire = |name: &[u8]| {
            postcard::to_allocvec(&Wire { name: name.to_vec(), email: b"e".to_vec(), time: 1, tz_min: 0 }).unwrap()
        };
        let ok: Signature = postcard::from_bytes(&wire(b"n")).unwrap();
        assert_eq!(ok, Signature::new(b"n".to_vec(), b"e".to_vec(), 1, 0).unwrap());
        assert!(postcard::from_bytes::<Signature>(&wire(b"n\nparent x")).is_err());
        assert_eq!(postcard::to_allocvec(&ok).unwrap(), wire(b"n"));
    }
}
