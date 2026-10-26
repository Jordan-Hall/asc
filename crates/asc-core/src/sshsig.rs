//! Signed changes: SSHSIG signatures over a changeset id (`ssh-keygen -Y sign` compatible).
//!
//! A signature is kept next to a changeset, not inside it, so signing never changes an id and a
//! change can be signed after it was pushed. What is signed is the fixed text
//! `asc-changeset-v1\n<64 hex characters of the changeset id>\n` in the SSHSIG namespace
//! [`NAMESPACE`], so a signature made for another purpose (a file, a git commit, an SSH login)
//! cannot be replayed as a changeset signature, and the other way round.
//!
//! Accepted keys are `ssh-ed25519`, `ecdsa-sha2-nistp256` and `ssh-rsa` keys of 3072 bits or more
//! whose signatures use `rsa-sha2-256` or `rsa-sha2-512` (never SHA-1). Certificates, security keys,
//! DSA and the other curves are refused.
//!
//! This module is pure: it parses keys and signatures, builds the signed message and verifies.
//! Who owns a key, whether it expired and whether it was revoked is the server's business
//! (`asc-serve`'s `signing` module).

use ssh_key::{Algorithm, EcdsaCurve, HashAlg, LineEnding, PrivateKey, SshSig};

/// The SSHSIG namespace of a changeset signature.
pub const NAMESPACE: &str = "asc-changeset";
/// The first line of the signed message.
pub const MESSAGE_HEADER: &str = "asc-changeset-v1";
/// Largest armored signature accepted, in bytes.
pub const MAX_SIGNATURE_BYTES: usize = 8 * 1024;
/// Longest public key line accepted, in bytes.
pub const MAX_PUBLIC_KEY_BYTES: usize = 4 * 1024;
/// Smallest RSA modulus accepted, in bits.
pub const MIN_RSA_BITS: u32 = 3072;
/// The hash a signature made by this module uses over the message.
const SIGN_HASH: HashAlg = HashAlg::Sha512;

/// The text that is signed for `changeset` (64 lower-case hex characters).
pub fn message(changeset: &str) -> Vec<u8> {
    format!("{MESSAGE_HEADER}\n{changeset}\n").into_bytes()
}

/// Why a key or signature was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Not an SSHSIG, a namespace or algorithm asc does not accept, an oversized or damaged blob.
    #[error("{0}")]
    Unsupported(String),
    /// A well-formed signature that does not verify.
    #[error("the signature does not match the message")]
    Bad,
    /// A private key that cannot sign.
    #[error("{0}")]
    Key(String),
}

fn unsupported<T>(m: impl Into<String>) -> Result<T, Error> {
    Err(Error::Unsupported(m.into()))
}

/// The kinds of key asc accepts for signing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyKind {
    Ed25519,
    EcdsaP256,
    Rsa,
}

impl KeyKind {
    /// The SSH algorithm name of the public key.
    pub fn algorithm(self) -> &'static str {
        match self {
            Self::Ed25519 => "ssh-ed25519",
            Self::EcdsaP256 => "ecdsa-sha2-nistp256",
            Self::Rsa => "ssh-rsa",
        }
    }
}

/// An accepted public key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicKey {
    inner: ssh_key::PublicKey,
    kind: KeyKind,
    fingerprint: String,
    line: String,
}

fn kind_of(key: &ssh_key::PublicKey) -> Result<KeyKind, Error> {
    match key.algorithm() {
        Algorithm::Ed25519 => Ok(KeyKind::Ed25519),
        Algorithm::Ecdsa {
            curve: EcdsaCurve::NistP256,
        } => Ok(KeyKind::EcdsaP256),
        Algorithm::Rsa { .. } => {
            let bits = key.key_data().rsa().map_or(0, |r| r.key_size());
            if bits < MIN_RSA_BITS {
                return unsupported(format!(
                    "RSA keys under {MIN_RSA_BITS} bits are not accepted (this one has {bits})"
                ));
            }
            Ok(KeyKind::Rsa)
        }
        other => unsupported(format!(
            "the key type {} is not accepted; use ed25519, ecdsa-sha2-nistp256 or RSA of {MIN_RSA_BITS}+ bits",
            other.as_str()
        )),
    }
}

impl PublicKey {
    /// Parses one `authorized_keys`-style line: `<algorithm> <base64> [comment]`. A line with
    /// options (`command=`, `from=`, ...), a certificate, several lines, a control character or
    /// more than [`MAX_PUBLIC_KEY_BYTES`] is refused.
    pub fn parse(line: &str) -> Result<Self, Error> {
        if line.len() > MAX_PUBLIC_KEY_BYTES {
            return unsupported("the public key is too long");
        }
        let line = line.trim();
        if line.is_empty() || line.chars().any(char::is_control) {
            return unsupported("a public key is one line of printable text");
        }
        let inner = ssh_key::PublicKey::from_openssh(line)
            .map_err(|_| Error::Unsupported("not an OpenSSH public key line".into()))?;
        Self::from_key(inner)
    }

    fn from_key(inner: ssh_key::PublicKey) -> Result<Self, Error> {
        let kind = kind_of(&inner)?;
        let fingerprint = inner.fingerprint(HashAlg::Sha256).to_string();
        // The key keeps no comment: the canonical line is what is stored and shown.
        let mut inner = inner;
        inner.set_comment("");
        let line = inner
            .to_openssh()
            .map_err(|_| Error::Unsupported("the public key cannot be encoded".into()))?;
        Ok(Self {
            inner,
            kind,
            fingerprint,
            line: line.trim().to_owned(),
        })
    }

    pub fn kind(&self) -> KeyKind {
        self.kind
    }

    /// `SHA256:<base64>` as `ssh-keygen -l` prints it.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The key as `<algorithm> <base64>`, without a comment.
    pub fn line(&self) -> &str {
        &self.line
    }

    /// Verifies `signature` over `message` with this key. The signature must carry this key.
    pub fn verify(&self, message: &[u8], signature: &Signature) -> Result<(), Error> {
        self.inner
            .verify(NAMESPACE, message, &signature.inner)
            .map_err(|_| Error::Bad)
    }
}

/// An accepted SSHSIG signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    inner: SshSig,
    key: PublicKey,
}

impl Signature {
    /// Parses an armored `-----BEGIN SSH SIGNATURE-----` block. The namespace must be
    /// [`NAMESPACE`], the key must be an accepted one and the algorithm must not be SHA-1 RSA.
    pub fn parse(armored: &str) -> Result<Self, Error> {
        if armored.len() > MAX_SIGNATURE_BYTES {
            return unsupported("the signature is too long");
        }
        let inner = SshSig::from_pem(armored.trim().as_bytes())
            .map_err(|_| Error::Unsupported("not an SSH signature".into()))?;
        Self::from_sig(inner)
    }

    fn from_sig(inner: SshSig) -> Result<Self, Error> {
        if inner.namespace() != NAMESPACE {
            return unsupported(format!(
                "the signature is for the namespace {:?}, not {NAMESPACE:?}",
                inner.namespace()
            ));
        }
        if !inner.reserved().is_empty() {
            return unsupported("the signature carries reserved data");
        }
        let key = PublicKey::from_key(ssh_key::PublicKey::from(inner.public_key().clone()))?;
        match inner.algorithm() {
            Algorithm::Ed25519
            | Algorithm::Ecdsa {
                curve: EcdsaCurve::NistP256,
            }
            | Algorithm::Rsa {
                hash: Some(HashAlg::Sha256 | HashAlg::Sha512),
            } => {}
            other => {
                return unsupported(format!(
                    "the signature algorithm {} is not accepted",
                    other.as_str()
                ));
            }
        }
        Ok(Self { inner, key })
    }

    /// The key the signature names. It is only a claim until the signature verifies.
    pub fn key(&self) -> &PublicKey {
        &self.key
    }

    /// Checks the signature against the key it carries.
    pub fn verify(&self, message: &[u8]) -> Result<(), Error> {
        self.key.verify(message, self)
    }

    /// The armored block, ready to store or send.
    pub fn armored(&self) -> Result<String, Error> {
        self.inner
            .to_pem(LineEnding::LF)
            .map_err(|_| Error::Unsupported("the signature cannot be encoded".into()))
    }
}

/// A private key held in memory, for `asc sign` with a key file.
pub struct KeyFileSigner {
    key: PrivateKey,
    public: PublicKey,
}

impl std::fmt::Debug for KeyFileSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KeyFileSigner({})", self.public.fingerprint())
    }
}

impl KeyFileSigner {
    /// Reads an unencrypted OpenSSH private key. An encrypted key is refused with advice to use
    /// an agent; the passphrase is never asked for here.
    pub fn from_openssh(text: &str) -> Result<Self, Error> {
        let key = PrivateKey::from_openssh(text)
            .map_err(|_| Error::Key("not an OpenSSH private key".into()))?;
        if key.is_encrypted() {
            return Err(Error::Key(
                "the key is encrypted; load it into ssh-agent (ssh-add) and sign through the agent"
                    .into(),
            ));
        }
        let public = PublicKey::from_key(key.public_key().clone())?;
        Ok(Self { key, public })
    }

    pub fn public_key(&self) -> &PublicKey {
        &self.public
    }

    /// Signs `message`.
    pub fn sign(&self, message: &[u8]) -> Result<Signature, Error> {
        let sig = self
            .key
            .sign(NAMESPACE, SIGN_HASH, message)
            .map_err(|_| Error::Key("the key cannot sign".into()))?;
        Signature::from_sig(sig)
    }
}

/// What a signer outside this process (an SSH agent) needs: the bytes to sign, and then the way
/// to turn the raw signature back into an SSHSIG.
pub fn signed_data(message: &[u8]) -> Result<Vec<u8>, Error> {
    SshSig::signed_data(NAMESPACE, SIGN_HASH, message)
        .map_err(|_| Error::Key("the message cannot be prepared for signing".into()))
}

/// Builds the SSHSIG from `key` and the signature blob an agent returned (`string algorithm`,
/// `string signature`, as the agent protocol sends it), then checks it against `message`.
pub fn assemble(key: &PublicKey, blob: &[u8], message: &[u8]) -> Result<Signature, Error> {
    use ssh_key::encoding::Decode;
    let mut reader = blob;
    let raw = ssh_key::Signature::decode(&mut reader)
        .map_err(|_| Error::Unsupported("the agent's signature cannot be read".into()))?;
    let sig = SshSig::new(key.inner.key_data().clone(), NAMESPACE, SIGN_HASH, raw)
        .map_err(|_| Error::Unsupported("the agent's signature cannot be used".into()))?;
    let signature = Signature::from_sig(sig)?;
    signature.verify(message)?;
    Ok(signature)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "abababababababababababababababababababababababababababababababab";

    // Public keys and signatures made by OpenSSH (`ssh-keygen -Y sign -n asc-changeset`) over
    // `message(HEX)`. No private key is kept in the repository.
    const ED_PUB: &str = include_str!("../tests/sshsig/k_ed.pub");
    const P256_PUB: &str = include_str!("../tests/sshsig/k_p256.pub");
    const P384_PUB: &str = include_str!("../tests/sshsig/k_p384.pub");
    const RSA_PUB: &str = include_str!("../tests/sshsig/k_rsa.pub");
    const RSA2K_PUB: &str = include_str!("../tests/sshsig/k_rsa2k.pub");
    const OTHER_PUB: &str = include_str!("../tests/sshsig/k_other.pub");
    const ED_SIG: &str = include_str!("../tests/sshsig/msg_ed.sig");
    const P256_SIG: &str = include_str!("../tests/sshsig/msg_p256.sig");
    const P384_SIG: &str = include_str!("../tests/sshsig/msg_p384.sig");
    const RSA_SIG: &str = include_str!("../tests/sshsig/msg_rsa.sig");
    const RSA2K_SIG: &str = include_str!("../tests/sshsig/msg_rsa2k.sig");
    const OTHER_SIG: &str = include_str!("../tests/sshsig/msg_other.sig");
    const WRONG_MESSAGE_SIG: &str = include_str!("../tests/sshsig/msg_wrong.sig");
    const WRONG_NAMESPACE_SIG: &str = include_str!("../tests/sshsig/msg_ns.sig");

    #[test]
    fn the_message_is_the_fixed_header_and_the_id() {
        assert_eq!(
            message(HEX),
            format!("asc-changeset-v1\n{HEX}\n").into_bytes()
        );
    }

    #[test]
    fn each_accepted_key_kind_verifies_an_openssh_signature() {
        for (public, sig, kind) in [
            (ED_PUB, ED_SIG, KeyKind::Ed25519),
            (P256_PUB, P256_SIG, KeyKind::EcdsaP256),
            (RSA_PUB, RSA_SIG, KeyKind::Rsa),
            (OTHER_PUB, OTHER_SIG, KeyKind::Ed25519),
        ] {
            let key = PublicKey::parse(public).unwrap();
            assert_eq!(key.kind(), kind);
            let signature = Signature::parse(sig).unwrap();
            assert_eq!(
                signature.key(),
                &key,
                "{kind:?}: the signature carries the key"
            );
            signature.verify(&message(HEX)).unwrap();
            key.verify(&message(HEX), &signature).unwrap();
            assert!(key.fingerprint().starts_with("SHA256:"));
            assert!(!key.line().contains("@test"), "the comment is not kept");
        }
    }

    #[test]
    fn a_different_message_fails() {
        let signature = Signature::parse(ED_SIG).unwrap();
        let other = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";
        assert_eq!(signature.verify(&message(other)), Err(Error::Bad));
        assert_eq!(signature.verify(b""), Err(Error::Bad));
        // A signature made over another changeset does not verify this one either.
        let wrong = Signature::parse(WRONG_MESSAGE_SIG).unwrap();
        assert_eq!(wrong.verify(&message(HEX)), Err(Error::Bad));
        assert!(wrong.verify(&message(other)).is_ok());
    }

    #[test]
    fn a_signature_for_another_namespace_is_refused() {
        let e = Signature::parse(WRONG_NAMESPACE_SIG).unwrap_err();
        assert!(
            matches!(&e, Error::Unsupported(m) if m.contains("namespace")),
            "{e}"
        );
    }

    #[test]
    fn a_signature_by_a_key_other_than_the_claimed_one_fails() {
        // Swap the key of a good signature for another key: the signature cannot verify.
        let signature = Signature::parse(ED_SIG).unwrap();
        let other = PublicKey::parse(OTHER_PUB).unwrap();
        assert_eq!(other.verify(&message(HEX), &signature), Err(Error::Bad));
    }

    #[test]
    fn keys_of_unaccepted_kinds_and_sizes_are_refused() {
        let e = PublicKey::parse(RSA2K_PUB).unwrap_err();
        assert!(
            matches!(&e, Error::Unsupported(m) if m.contains("3072")),
            "{e}"
        );
        assert!(matches!(
            PublicKey::parse(P384_PUB),
            Err(Error::Unsupported(_))
        ));
        // Signatures by such keys are refused too, even though they are valid.
        assert!(matches!(
            Signature::parse(RSA2K_SIG),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            Signature::parse(P384_SIG),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn malformed_keys_and_signatures_are_refused() {
        let ed = ED_PUB.trim();
        let cases = [
            String::new(),
            "   ".into(),
            "ssh-ed25519".into(),
            "ssh-ed25519 !!!!".into(),
            format!("command=\"echo hi\" {ed}"),
            format!("from=\"10.0.0.1\",no-pty {ed}"),
            format!("{ed}\n{ed}"),
            format!("{ed}\u{0}"),
            "ssh-dss AAAAB3NzaC1kc3M=".into(),
            "-----BEGIN SSH SIGNATURE-----".into(),
            "x".repeat(MAX_PUBLIC_KEY_BYTES + 1),
        ];
        for c in &cases {
            assert!(PublicKey::parse(c).is_err(), "{:?}", &c[..c.len().min(40)]);
        }
        assert!(Signature::parse("").is_err());
        assert!(Signature::parse("not a signature").is_err());
        assert!(Signature::parse(&"A".repeat(MAX_SIGNATURE_BYTES + 1)).is_err());
        let truncated = &ED_SIG[..ED_SIG.len() / 2];
        assert!(Signature::parse(truncated).is_err());
        // A public key line is not a signature and the other way round.
        assert!(Signature::parse(ED_PUB).is_err());
        assert!(PublicKey::parse(ED_SIG).is_err());
    }

    #[test]
    fn a_comment_is_dropped_from_the_canonical_line() {
        let key = PublicKey::parse(ED_PUB).unwrap();
        let again = PublicKey::parse(&format!("{} some other words", key.line())).unwrap();
        assert_eq!(key.line(), again.line());
        assert_eq!(key.fingerprint(), again.fingerprint());
        assert_eq!(key.line().split(' ').count(), 2);
    }

    #[test]
    fn a_key_file_signs_and_the_result_verifies_like_openssh_output() {
        let mut rng = ssh_key::rand_core::UnwrapErr(ssh_key::getrandom::SysRng);
        for algorithm in [
            Algorithm::Ed25519,
            Algorithm::Ecdsa {
                curve: EcdsaCurve::NistP256,
            },
        ] {
            let private = PrivateKey::random(&mut rng, algorithm.clone()).unwrap();
            let text = private.to_openssh(LineEnding::LF).unwrap();
            let signer = KeyFileSigner::from_openssh(&text).unwrap();
            let signature = signer.sign(&message(HEX)).unwrap();
            signature.verify(&message(HEX)).unwrap();
            // It round-trips through its armor and is a real SSHSIG block.
            let armor = signature.armored().unwrap();
            assert!(armor.starts_with("-----BEGIN SSH SIGNATURE-----"));
            let parsed = Signature::parse(&armor).unwrap();
            assert_eq!(parsed, signature);
            assert_eq!(parsed.key(), signer.public_key());
            assert_eq!(signature.verify(&message("00")), Err(Error::Bad));
            assert!(!format!("{signer:?}").contains("PRIVATE"));
        }
    }

    #[test]
    fn an_agent_blob_is_assembled_and_checked() {
        use ssh_key::encoding::Encode;
        let mut rng = ssh_key::rand_core::UnwrapErr(ssh_key::getrandom::SysRng);
        let private = PrivateKey::random(&mut rng, Algorithm::Ed25519).unwrap();
        let signer =
            KeyFileSigner::from_openssh(&private.to_openssh(LineEnding::LF).unwrap()).unwrap();
        let data = signed_data(&message(HEX)).unwrap();
        // What an agent does: sign the prepared bytes and answer the signature blob.
        let whole = SshSig::sign(&private, NAMESPACE, SIGN_HASH, &message(HEX)).unwrap();
        assert_eq!(
            data,
            SshSig::signed_data(NAMESPACE, SIGN_HASH, &message(HEX)).unwrap()
        );
        let mut blob = Vec::new();
        whole.signature().encode(&mut blob).unwrap();
        let signature = assemble(signer.public_key(), &blob, &message(HEX)).unwrap();
        signature.verify(&message(HEX)).unwrap();
        // A blob for another message is caught.
        assert_eq!(
            assemble(signer.public_key(), &blob, &message("00")),
            Err(Error::Bad)
        );
        assert!(assemble(signer.public_key(), &[1, 2, 3], &message(HEX)).is_err());
    }

    #[test]
    fn an_encrypted_or_unreadable_private_key_is_refused() {
        assert!(matches!(
            KeyFileSigner::from_openssh("nope"),
            Err(Error::Key(_))
        ));
        assert!(matches!(
            KeyFileSigner::from_openssh(ED_PUB),
            Err(Error::Key(_))
        ));
    }
}
