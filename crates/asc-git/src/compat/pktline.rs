//! Bounded pkt-line framing (SPEC §25.2, §25.8) for the few places asc itself reads or writes
//! the git smart protocol: the `# service=…` advertisement prefix, the `0000` probe, `ERR`
//! packets and the protocol v2 capability check. It never parses a pack and never negotiates;
//! git does both in its own child.
//!
//! Every length is bounded before any allocation: a packet is at most [`MAX_PKT_LEN`] bytes
//! including its four-digit header, the decoder buffers at most [`MAX_BUFFERED`] bytes, and the
//! v2 capability reader stops at [`MAX_V2_CAPABILITIES`] lines and [`MAX_V2_ADVERT_BYTES`] bytes.

use std::io::{self, Read};

/// The longest packet git writes, header included (`LARGE_PACKET_MAX`).
pub const MAX_PKT_LEN: usize = 65_520;
/// The longest payload of one packet.
pub const MAX_PKT_DATA: usize = MAX_PKT_LEN - HEADER_LEN;
/// A packet's length header: four hex digits.
pub const HEADER_LEN: usize = 4;
/// Bytes a [`Decoder`] holds that are not yet a whole packet.
pub const MAX_BUFFERED: usize = 2 * MAX_PKT_LEN;
/// Capability lines accepted in a v2 advertisement.
pub const MAX_V2_CAPABILITIES: usize = 256;
/// Bytes accepted in a v2 advertisement.
pub const MAX_V2_ADVERT_BYTES: usize = 64 * 1024;
/// Longest `GIT_PROTOCOL` value that is passed to a child.
pub const MAX_GIT_PROTOCOL_LEN: usize = 256;

/// `0000`: the end of a section.
pub const FLUSH: &[u8; HEADER_LEN] = b"0000";
/// `0001`: protocol v2's delimiter between the command header and its arguments.
pub const DELIM: &[u8; HEADER_LEN] = b"0001";
/// `0002`: protocol v2's end of a stateless response.
pub const RESPONSE_END: &[u8; HEADER_LEN] = b"0002";

/// Why framing failed.
#[derive(Debug, thiserror::Error)]
pub enum PktError {
    #[error("pkt-line header {0:?} is not four hex digits")]
    BadHeader([u8; HEADER_LEN]),
    #[error("pkt-line length {0} is reserved or below the header size")]
    BadLength(usize),
    #[error("pkt-line length {0} is above the {MAX_PKT_LEN} byte limit")]
    TooLong(usize),
    #[error("input ended inside a pkt-line")]
    Truncated,
    #[error("more than {MAX_BUFFERED} bytes are waiting to be framed")]
    Backlog,
    #[error("payload of {0} bytes does not fit one pkt-line")]
    PayloadTooLong(usize),
    #[error("the advertisement is not a protocol v2 capability list: {0}")]
    BadAdvertisement(&'static str),
    #[error("reading a pkt-line: {0}")]
    Io(#[from] io::Error),
}

/// One framed packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    /// A payload (possibly empty, from `0004`).
    Data(Vec<u8>),
    /// `0000`.
    Flush,
    /// `0001`.
    Delim,
    /// `0002`.
    ResponseEnd,
}

impl Packet {
    /// The payload with one trailing newline removed, when this is a data packet.
    pub fn text(&self) -> Option<&[u8]> {
        match self {
            Self::Data(d) => Some(d.strip_suffix(b"\n").unwrap_or(d)),
            _ => None,
        }
    }

    /// The message of an `ERR <message>` packet.
    pub fn err_message(&self) -> Option<&[u8]> {
        self.text()?.strip_prefix(b"ERR ")
    }

    /// Appends the wire form.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), PktError> {
        match self {
            Self::Data(d) => write_data(out, d),
            Self::Flush => {
                out.extend_from_slice(FLUSH);
                Ok(())
            }
            Self::Delim => {
                out.extend_from_slice(DELIM);
                Ok(())
            }
            Self::ResponseEnd => {
                out.extend_from_slice(RESPONSE_END);
                Ok(())
            }
        }
    }
}

/// Appends a data packet.
pub fn write_data(out: &mut Vec<u8>, payload: &[u8]) -> Result<(), PktError> {
    if payload.len() > MAX_PKT_DATA {
        return Err(PktError::PayloadTooLong(payload.len()));
    }
    out.extend_from_slice(format!("{:04x}", payload.len() + HEADER_LEN).as_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

/// An `ERR <message>` packet. The message is cut to fit one packet and loses control bytes, so
/// no client-visible text can carry a second packet or a terminal escape.
pub fn error_packet(message: &str) -> Vec<u8> {
    const PREFIX: &[u8] = b"ERR ";
    let clean: Vec<u8> = message
        .bytes()
        .map(|b| if b.is_ascii_control() { b' ' } else { b })
        .take(MAX_PKT_DATA - PREFIX.len() - 1)
        .collect();
    let mut payload = PREFIX.to_vec();
    payload.extend_from_slice(&clean);
    payload.push(b'\n');
    let mut out = Vec::with_capacity(payload.len() + HEADER_LEN);
    // The payload is bounded above, so this cannot fail.
    let _ = write_data(&mut out, &payload);
    out
}

fn parse_header(h: [u8; HEADER_LEN]) -> Result<usize, PktError> {
    let mut n = 0usize;
    for b in h {
        let d = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return Err(PktError::BadHeader(h)),
        };
        n = n * 16 + usize::from(d);
    }
    Ok(n)
}

/// What a header length means before any payload is read.
enum Frame {
    Special(Packet),
    Data(usize),
}

fn classify(len: usize) -> Result<Frame, PktError> {
    match len {
        0 => Ok(Frame::Special(Packet::Flush)),
        1 => Ok(Frame::Special(Packet::Delim)),
        2 => Ok(Frame::Special(Packet::ResponseEnd)),
        3 => Err(PktError::BadLength(len)),
        l if l > MAX_PKT_LEN => Err(PktError::TooLong(l)),
        l => Ok(Frame::Data(l - HEADER_LEN)),
    }
}

/// Reads one packet from a blocking reader; `None` at a clean end of input between packets.
pub fn read_packet<R: Read>(r: &mut R) -> Result<Option<Packet>, PktError> {
    let mut header = [0u8; HEADER_LEN];
    let mut got = 0;
    while got < HEADER_LEN {
        match r.read(&mut header[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(PktError::Truncated),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    match classify(parse_header(header)?)? {
        Frame::Special(p) => Ok(Some(p)),
        Frame::Data(n) => {
            // `n` is at most MAX_PKT_DATA, so the allocation is bounded by the check above.
            let mut data = vec![0u8; n];
            r.read_exact(&mut data).map_err(|e| {
                if e.kind() == io::ErrorKind::UnexpectedEof {
                    PktError::Truncated
                } else {
                    e.into()
                }
            })?;
            Ok(Some(Packet::Data(data)))
        }
    }
}

/// An incremental decoder for callers that get bytes in chunks (an HTTP body, a child pipe).
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
    pos: usize,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a chunk. Call [`Decoder::next`] until it returns `None` between chunks, so the
    /// backlog stays under [`MAX_BUFFERED`].
    pub fn feed(&mut self, chunk: &[u8]) -> Result<(), PktError> {
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        if self.buf.len() + chunk.len() > MAX_BUFFERED {
            return Err(PktError::Backlog);
        }
        self.buf.extend_from_slice(chunk);
        Ok(())
    }

    /// The next whole packet, or `None` until more bytes arrive.
    // A decoder is not an `Iterator`: it can fail, and it is fed between calls.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Packet>, PktError> {
        let rest = &self.buf[self.pos..];
        let Some(h) = rest.first_chunk::<HEADER_LEN>() else {
            return Ok(None);
        };
        match classify(parse_header(*h)?)? {
            Frame::Special(p) => {
                self.pos += HEADER_LEN;
                Ok(Some(p))
            }
            Frame::Data(n) => {
                if rest.len() < HEADER_LEN + n {
                    return Ok(None);
                }
                let data = rest[HEADER_LEN..HEADER_LEN + n].to_vec();
                self.pos += HEADER_LEN + n;
                Ok(Some(Packet::Data(data)))
            }
        }
    }

    /// Fails when the input ended inside a packet.
    pub fn finish(&self) -> Result<(), PktError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(PktError::Truncated)
        }
    }
}

/// The two services asc serves. `git-upload-archive` and anything else do not parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Service {
    UploadPack,
    ReceivePack,
}

impl Service {
    /// The `service=` query value, exactly; no case folding and no prefix match.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "git-upload-pack" => Some(Self::UploadPack),
            "git-receive-pack" => Some(Self::ReceivePack),
            _ => None,
        }
    }

    /// `git-upload-pack` or `git-receive-pack`.
    pub fn name(self) -> &'static str {
        match self {
            Self::UploadPack => "git-upload-pack",
            Self::ReceivePack => "git-receive-pack",
        }
    }
}

/// The bytes before the child's advertisement on `info/refs`: `# service=<name>` and a flush.
/// Protocol v2 responses carry no prefix ([`ProtocolVersion::wants_service_prefix`]).
pub fn service_advert_prefix(service: Service) -> Vec<u8> {
    let mut out = Vec::new();
    let line = format!("# service={}\n", service.name());
    // A fixed short line.
    let _ = write_data(&mut out, line.as_bytes());
    out.extend_from_slice(FLUSH);
    out
}

/// Whether a request body is the bare `0000` probe that git sends before a large POST, which is
/// answered before any child starts (SPEC §25.5).
pub fn is_probe(body: &[u8]) -> bool {
    body == FLUSH
}

/// Reads and checks the `# service=<name>` prefix at the start of `input`; returns the service
/// and the number of bytes the prefix took.
pub fn parse_service_prefix(input: &[u8]) -> Result<(Service, usize), PktError> {
    let mut d = Decoder::new();
    d.feed(&input[..input.len().min(MAX_PKT_LEN * 2)])?;
    let bad = PktError::BadAdvertisement("no service prefix");
    let Some(Packet::Data(line)) = d.next()? else {
        return Err(bad);
    };
    let name = line
        .strip_prefix(b"# service=")
        .map(|n| n.strip_suffix(b"\n").unwrap_or(n))
        .and_then(|n| std::str::from_utf8(n).ok())
        .and_then(Service::parse)
        .ok_or(PktError::BadAdvertisement("unknown service"))?;
    match d.next()? {
        Some(Packet::Flush) => Ok((name, d.pos)),
        _ => Err(PktError::BadAdvertisement(
            "prefix is not followed by a flush",
        )),
    }
}

/// A git wire protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProtocolVersion {
    V0,
    V1,
    V2,
}

impl ProtocolVersion {
    /// `info/refs` carries the `# service=` prefix for v0 and v1 only.
    pub fn wants_service_prefix(self) -> bool {
        self != Self::V2
    }
}

/// A `Git-Protocol` header value that is safe to pass as `GIT_PROTOCOL`: at most
/// [`MAX_GIT_PROTOCOL_LEN`] bytes, `version=[0-2]` and then `:`-separated `[a-z0-9=]+` items.
/// Anything else (a space, NUL, newline, `;`, a long value) is dropped, never trimmed or fixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitProtocol {
    value: String,
    version: ProtocolVersion,
}

impl GitProtocol {
    /// `None` when `value` does not match `^version=[0-2](:[a-z0-9=]+)*$` within the length.
    pub fn parse(value: &str) -> Option<Self> {
        if value.len() > MAX_GIT_PROTOCOL_LEN {
            return None;
        }
        let rest = value.strip_prefix("version=")?;
        let (digit, extras) = match rest.split_once(':') {
            Some((d, e)) => (d, Some(e)),
            None => (rest, None),
        };
        let version = match digit {
            "0" => ProtocolVersion::V0,
            "1" => ProtocolVersion::V1,
            "2" => ProtocolVersion::V2,
            _ => return None,
        };
        if let Some(extras) = extras {
            let ok = extras.split(':').all(|item| {
                !item.is_empty()
                    && item
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'=')
            });
            if !ok {
                return None;
            }
        }
        Some(Self {
            value: value.to_owned(),
            version,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }

    pub fn version(&self) -> ProtocolVersion {
        self.version
    }
}

/// The checked contents of a protocol v2 capability advertisement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V2Advert {
    /// The `agent=` value.
    pub agent: Option<String>,
    /// The `object-format=` value.
    pub object_format: Option<String>,
    /// Every capability line, `key` or `key=value`.
    pub capabilities: Vec<String>,
}

impl V2Advert {
    /// Whether a capability is advertised (`fetch`, `ls-refs`, …), ignoring any `=value`.
    pub fn has(&self, name: &str) -> bool {
        self.capabilities
            .iter()
            .any(|c| c.split('=').next() == Some(name))
    }
}

/// Checks a protocol v2 advertisement as git's `upload-pack` writes it: `version 2`, capability
/// lines, a flush. It must offer `ls-refs` and `fetch`.
pub fn check_v2_advert(input: &[u8]) -> Result<V2Advert, PktError> {
    let bad = PktError::BadAdvertisement;
    if input.len() > MAX_V2_ADVERT_BYTES {
        return Err(bad("larger than the advertisement limit"));
    }
    let mut d = Decoder::new();
    d.feed(input)?;
    match d.next()? {
        Some(p) if p.text() == Some(b"version 2") => {}
        _ => return Err(bad("does not start with 'version 2'")),
    }
    let mut capabilities = Vec::new();
    loop {
        match d.next()? {
            Some(Packet::Data(line)) => {
                if capabilities.len() == MAX_V2_CAPABILITIES {
                    return Err(bad("too many capabilities"));
                }
                let line = line.strip_suffix(b"\n").unwrap_or(&line);
                let line =
                    std::str::from_utf8(line).map_err(|_| bad("a capability is not UTF-8"))?;
                capabilities.push(line.to_owned());
            }
            Some(Packet::Flush) => break,
            Some(_) => return Err(bad("unexpected delimiter")),
            None => return Err(bad("no closing flush")),
        }
    }
    let value = |key: &str| {
        capabilities
            .iter()
            .find_map(|c| c.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
            .map(str::to_owned)
    };
    let advert = V2Advert {
        agent: value("agent"),
        object_format: value("object-format"),
        capabilities,
    };
    if !advert.has("ls-refs") || !advert.has("fetch") {
        return Err(bad("ls-refs or fetch is missing"));
    }
    Ok(advert)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(s: &str) -> Vec<u8> {
        let mut v = Vec::new();
        write_data(&mut v, s.as_bytes()).unwrap();
        v
    }

    #[test]
    fn writes_and_reads_round_trip() {
        let mut wire = data("hello\n");
        wire.extend_from_slice(FLUSH);
        wire.extend_from_slice(DELIM);
        wire.extend_from_slice(RESPONSE_END);
        wire.extend_from_slice(b"0004");
        let mut r = &wire[..];
        let mut got = Vec::new();
        while let Some(p) = read_packet(&mut r).unwrap() {
            got.push(p);
        }
        assert_eq!(
            got,
            vec![
                Packet::Data(b"hello\n".to_vec()),
                Packet::Flush,
                Packet::Delim,
                Packet::ResponseEnd,
                Packet::Data(Vec::new()),
            ]
        );
        assert_eq!(&wire[..4], b"000a");
    }

    #[test]
    fn lengths_are_bounded_before_allocation() {
        assert!(matches!(
            read_packet(&mut &b"fff1x"[..]),
            Err(PktError::TooLong(65_521))
        ));
        assert!(matches!(
            read_packet(&mut &b"0003"[..]),
            Err(PktError::BadLength(3))
        ));
        assert!(matches!(
            read_packet(&mut &b"zzzz"[..]),
            Err(PktError::BadHeader(_))
        ));
        assert!(matches!(
            read_packet(&mut &b"00"[..]),
            Err(PktError::Truncated)
        ));
        assert!(matches!(
            read_packet(&mut &b"0009ab"[..]),
            Err(PktError::Truncated)
        ));
        let max = vec![b'x'; MAX_PKT_DATA];
        let mut wire = Vec::new();
        write_data(&mut wire, &max).unwrap();
        assert_eq!(&wire[..4], b"fff0");
        assert!(write_data(&mut wire, &vec![b'x'; MAX_PKT_DATA + 1]).is_err());
        assert_eq!(
            read_packet(&mut &wire[..]).unwrap(),
            Some(Packet::Data(max))
        );
    }

    #[test]
    fn decoder_waits_for_whole_packets_and_bounds_its_backlog() {
        let wire = data("abc");
        let mut d = Decoder::new();
        d.feed(&wire[..5]).unwrap();
        assert_eq!(d.next().unwrap(), None);
        assert!(matches!(d.finish(), Err(PktError::Truncated)));
        d.feed(&wire[5..]).unwrap();
        assert_eq!(d.next().unwrap(), Some(Packet::Data(b"abc".to_vec())));
        d.finish().unwrap();
        assert!(matches!(
            d.feed(&vec![b'0'; MAX_BUFFERED + 1]),
            Err(PktError::Backlog)
        ));
    }

    #[test]
    fn err_packets_cannot_smuggle_a_second_packet() {
        let wire = error_packet("bad\n0000thing\u{1b}[31m");
        let p = read_packet(&mut &wire[..]).unwrap().unwrap();
        let msg = p.err_message().unwrap();
        assert!(!msg.iter().any(|b| b.is_ascii_control()));
        let long = error_packet(&"x".repeat(MAX_PKT_LEN * 2));
        assert_eq!(long.len(), MAX_PKT_LEN);
    }

    #[test]
    fn service_prefix_round_trips_and_rejects_other_services() {
        let p = service_advert_prefix(Service::UploadPack);
        assert_eq!(p, b"001e# service=git-upload-pack\n0000");
        assert_eq!(
            parse_service_prefix(&p).unwrap(),
            (Service::UploadPack, p.len())
        );
        let r = service_advert_prefix(Service::ReceivePack);
        assert_eq!(parse_service_prefix(&r).unwrap().0, Service::ReceivePack);
        let archive = data("# service=git-upload-archive\n");
        assert!(parse_service_prefix(&archive).is_err());
        assert_eq!(Service::parse("git-upload-archive"), None);
        assert_eq!(Service::parse("Git-Upload-Pack"), None);
        assert_eq!(Service::parse("git-upload-pack "), None);
    }

    #[test]
    fn probe_is_exactly_four_zeros() {
        assert!(is_probe(b"0000"));
        assert!(!is_probe(b"00000"));
        assert!(!is_probe(b""));
        assert!(!is_probe(b"0001"));
    }

    #[test]
    fn git_protocol_values_are_validated_not_repaired() {
        for ok in [
            "version=2",
            "version=1",
            "version=0",
            "version=2:objectformat=sha1",
        ] {
            assert!(GitProtocol::parse(ok).is_some(), "{ok}");
        }
        assert_eq!(
            GitProtocol::parse("version=2").unwrap().version(),
            ProtocolVersion::V2
        );
        assert!(GitProtocol::parse("version=2:foo=bar:baz").is_some());
        // The spec alphabet has no `-`.
        assert!(GitProtocol::parse("version=2:object-format=sha1").is_none());
        for bad in [
            "version=3",
            "version=",
            "version=2 ",
            "version=2:a b",
            "version=2:a\0b",
            "version=2\n",
            "version=2:a;b",
            "version=2:",
            "version=2::a",
            "version=2:UP",
            "Version=2",
            "x version=2",
            "",
        ] {
            assert!(GitProtocol::parse(bad).is_none(), "{bad:?}");
        }
        let long = format!("version=2:{}", "a".repeat(MAX_GIT_PROTOCOL_LEN));
        assert!(GitProtocol::parse(&long).is_none());
        let max = format!(
            "version=2:{}",
            "a".repeat(MAX_GIT_PROTOCOL_LEN - "version=2:".len())
        );
        assert!(GitProtocol::parse(&max).is_some());
        assert!(ProtocolVersion::V1.wants_service_prefix());
        assert!(!ProtocolVersion::V2.wants_service_prefix());
    }

    #[test]
    fn v2_advertisement_is_checked() {
        let mut wire = data("version 2\n");
        for l in [
            "agent=asc/0.1.0\n",
            "ls-refs=unborn\n",
            "fetch=shallow wait-for-done\n",
            "object-format=sha1\n",
        ] {
            wire.extend(data(l));
        }
        wire.extend_from_slice(FLUSH);
        let a = check_v2_advert(&wire).unwrap();
        assert_eq!(a.agent.as_deref(), Some("asc/0.1.0"));
        assert_eq!(a.object_format.as_deref(), Some("sha1"));
        assert!(a.has("fetch") && a.has("ls-refs") && !a.has("object-info"));

        let mut no_fetch = data("version 2\n");
        no_fetch.extend(data("ls-refs\n"));
        no_fetch.extend_from_slice(FLUSH);
        assert!(check_v2_advert(&no_fetch).is_err());
        assert!(check_v2_advert(b"").is_err());
        assert!(check_v2_advert(&data("version 1\n")).is_err());
        let mut unterminated = data("version 2\n");
        unterminated.extend(data("fetch\n"));
        assert!(check_v2_advert(&unterminated).is_err());
        let mut many = data("version 2\n");
        for _ in 0..=MAX_V2_CAPABILITIES {
            many.extend(data("x\n"));
        }
        many.extend_from_slice(FLUSH);
        assert!(check_v2_advert(&many).is_err());
    }
}
