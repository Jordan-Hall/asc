//! ONC RPC version 2 (RFC 5531) over TCP record marking: the call header, the `AUTH_SYS`
//! credential, and the replies. Nothing here trusts the peer: every length is bounded and a
//! record larger than [`MAX_RECORD`] closes the connection.

use std::io::{self, Read, Write};

use super::xdr::{Garbage, Reader, Writer, XResult};

/// The largest record accepted or sent: a 1 MiB `WRITE` or `READ` and its headers.
pub const MAX_RECORD: usize = (1 << 20) + 16 * 1024;
/// The largest credential or verifier body (RFC 5531 §8.2).
const MAX_AUTH: usize = 400;
/// The longest `AUTH_SYS` machine name and the most supplementary groups (RFC 5531 §A.1).
const MAX_MACHINE: usize = 255;
const MAX_GIDS: u32 = 16;

pub const AUTH_NONE: u32 = 0;
pub const AUTH_SYS: u32 = 1;

const CALL: u32 = 0;
const REPLY: u32 = 1;
const MSG_ACCEPTED: u32 = 0;
const MSG_DENIED: u32 = 1;
const RPC_MISMATCH: u32 = 0;
const AUTH_ERROR: u32 = 1;

/// Why a credential was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthStat {
    /// The credential is malformed.
    BadCred = 1,
    /// The flavor is not strong enough (only `AUTH_SYS` is served).
    TooWeak = 5,
}

/// The outcome of an accepted call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accept {
    Success,
    ProgUnavail,
    /// The program exists in versions `low..=high` only.
    ProgMismatch(u32, u32),
    ProcUnavail,
    GarbageArgs,
    SystemErr,
}

/// Who made a call: the `AUTH_SYS` user and group, when the credential was one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cred {
    pub flavor: u32,
    pub uid: u32,
    pub gid: u32,
}

/// A parsed call; `args` is what follows the header.
pub struct Call<'a> {
    pub xid: u32,
    pub prog: u32,
    pub vers: u32,
    pub proc_: u32,
    pub cred: Cred,
    pub args: Reader<'a>,
}

/// Why a record is not a call this server can answer.
#[derive(Debug, PartialEq, Eq)]
pub enum Bad {
    /// Not even a transaction id: drop the record.
    Drop,
    /// Answer `xid` with this reply (already encoded).
    Reply(Vec<u8>),
}

fn auth_sys(body: &[u8]) -> XResult<(u32, u32)> {
    let mut r = Reader::new(body);
    let _stamp = r.u32()?;
    r.opaque(MAX_MACHINE)?;
    let uid = r.u32()?;
    let gid = r.u32()?;
    let n = r.u32()?;
    if n > MAX_GIDS {
        return Err(Garbage);
    }
    for _ in 0..n {
        r.u32()?;
    }
    if r.remaining() != 0 {
        return Err(Garbage);
    }
    Ok((uid, gid))
}

/// Parses the call header of a record.
pub fn parse_call(record: &[u8]) -> Result<Call<'_>, Bad> {
    let mut r = Reader::new(record);
    let xid = r.u32().map_err(|_| Bad::Drop)?;
    let header = (|| -> XResult<_> {
        let mtype = r.u32()?;
        let rpcvers = r.u32()?;
        let prog = r.u32()?;
        let vers = r.u32()?;
        let proc_ = r.u32()?;
        let flavor = r.u32()?;
        let body = r.opaque(MAX_AUTH)?;
        let _vflavor = r.u32()?;
        r.opaque(MAX_AUTH)?;
        Ok((mtype, rpcvers, prog, vers, proc_, flavor, body))
    })();
    let Ok((mtype, rpcvers, prog, vers, proc_, flavor, body)) = header else {
        return Err(Bad::Reply(accepted(xid, Accept::GarbageArgs, &[])));
    };
    if mtype != CALL {
        return Err(Bad::Drop);
    }
    if rpcvers != 2 {
        let mut w = Writer::with_capacity(24);
        w.u32(xid)
            .u32(REPLY)
            .u32(MSG_DENIED)
            .u32(RPC_MISMATCH)
            .u32(2)
            .u32(2);
        return Err(Bad::Reply(w.into_bytes()));
    }
    let cred = match flavor {
        AUTH_SYS => match auth_sys(body) {
            Ok((uid, gid)) => Cred { flavor, uid, gid },
            Err(_) => return Err(Bad::Reply(denied(xid, AuthStat::BadCred))),
        },
        _ => Cred {
            flavor,
            uid: u32::MAX,
            gid: u32::MAX,
        },
    };
    Ok(Call {
        xid,
        prog,
        vers,
        proc_,
        cred,
        args: r,
    })
}

/// An accepted reply; `body` follows a `SUCCESS` status.
pub fn accepted(xid: u32, how: Accept, body: &[u8]) -> Vec<u8> {
    let mut w = Writer::with_capacity(24 + body.len());
    w.u32(xid)
        .u32(REPLY)
        .u32(MSG_ACCEPTED)
        .u32(AUTH_NONE)
        .u32(0);
    match how {
        Accept::Success => {
            w.u32(0).raw(body);
        }
        Accept::ProgUnavail => {
            w.u32(1);
        }
        Accept::ProgMismatch(low, high) => {
            w.u32(2).u32(low).u32(high);
        }
        Accept::ProcUnavail => {
            w.u32(3);
        }
        Accept::GarbageArgs => {
            w.u32(4);
        }
        Accept::SystemErr => {
            w.u32(5);
        }
    }
    w.into_bytes()
}

/// A reply refusing the credential.
pub fn denied(xid: u32, why: AuthStat) -> Vec<u8> {
    let mut w = Writer::with_capacity(20);
    w.u32(xid)
        .u32(REPLY)
        .u32(MSG_DENIED)
        .u32(AUTH_ERROR)
        .u32(why as u32);
    w.into_bytes()
}

/// Reads one record (all its fragments). `None` at a clean end of stream.
pub fn read_record(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    loop {
        let mut h = [0u8; 4];
        match r.read_exact(&mut h) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof && out.is_empty() => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        }
        let h = u32::from_be_bytes(h);
        let last = h & 0x8000_0000 != 0;
        let len = (h & 0x7fff_ffff) as usize;
        if out.len() + len > MAX_RECORD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "an RPC record is too large",
            ));
        }
        let at = out.len();
        out.resize(at + len, 0);
        r.read_exact(&mut out[at..])?;
        if last {
            return Ok(Some(out));
        }
    }
}

/// Writes `msg` as one last fragment.
pub fn write_record(w: &mut impl Write, msg: &[u8]) -> io::Result<()> {
    let len = u32::try_from(msg.len())
        .ok()
        .filter(|&n| n < 0x8000_0000)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "reply too large"))?;
    let mut buf = Vec::with_capacity(4 + msg.len());
    buf.extend_from_slice(&(len | 0x8000_0000).to_be_bytes());
    buf.extend_from_slice(msg);
    w.write_all(&buf)?;
    w.flush()
}

/// Encodes a call, for the test client.
#[cfg(test)]
pub fn call(xid: u32, prog: u32, vers: u32, proc_: u32, cred: Option<(u32, u32)>, args: &[u8]) -> Vec<u8> {
    let mut w = Writer::new();
    w.u32(xid).u32(CALL).u32(2).u32(prog).u32(vers).u32(proc_);
    match cred {
        Some((uid, gid)) => {
            let mut c = Writer::new();
            c.u32(0).opaque(b"test").u32(uid).u32(gid).u32(1).u32(gid);
            w.u32(AUTH_SYS).opaque(c.bytes());
        }
        None => {
            w.u32(AUTH_NONE).u32(0);
        }
    }
    w.u32(AUTH_NONE).u32(0).raw(args);
    w.into_bytes()
}

/// An accepted body, or `(0, accept_stat)` / `(1, reject kind * 100 + detail)`.
#[cfg(test)]
pub type Reply = Result<Vec<u8>, (u32, u32)>;

/// Splits a reply, for the test client: the transaction id and the accepted body, or the
/// refusal.
#[cfg(test)]
pub fn parse_reply(msg: &[u8]) -> XResult<(u32, Reply)> {
    let mut r = Reader::new(msg);
    let xid = r.u32()?;
    if r.u32()? != REPLY {
        return Err(Garbage);
    }
    match r.u32()? {
        MSG_ACCEPTED => {
            r.u32()?;
            r.opaque(MAX_AUTH)?;
            let stat = r.u32()?;
            if stat == 0 {
                Ok((xid, Ok(r.fixed(r.remaining())?.to_vec())))
            } else {
                Ok((xid, Err((MSG_ACCEPTED, stat))))
            }
        }
        _ => {
            let kind = r.u32()?;
            Ok((xid, Err((MSG_DENIED, kind * 100 + r.u32()?))))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_auth_sys_call_and_answers_it() {
        let msg = call(9, 100_003, 3, 1, Some((501, 20)), &[0, 0, 0, 7]);
        let mut c = parse_call(&msg).unwrap();
        assert_eq!((c.xid, c.prog, c.vers, c.proc_), (9, 100_003, 3, 1));
        assert_eq!(c.cred, Cred { flavor: AUTH_SYS, uid: 501, gid: 20 });
        assert_eq!(c.args.u32(), Ok(7));
        let (xid, body) = parse_reply(&accepted(9, Accept::Success, &[1, 2, 3, 4])).unwrap();
        assert_eq!((xid, body), (9, Ok(vec![1, 2, 3, 4])));
        let (_, mismatch) = parse_reply(&accepted(9, Accept::ProgMismatch(3, 3), &[])).unwrap();
        assert_eq!(mismatch, Err((MSG_ACCEPTED, 2)));
        assert_eq!(
            parse_reply(&denied(4, AuthStat::TooWeak)).unwrap(),
            (4, Err((MSG_DENIED, 105)))
        );
    }

    #[test]
    fn refuses_bad_versions_credentials_and_headers() {
        // RPC version 3.
        let mut msg = call(1, 100_003, 3, 0, None, &[]);
        msg[11] = 3;
        let Err(Bad::Reply(r)) = parse_call(&msg) else { panic!() };
        assert_eq!(parse_reply(&r).unwrap().1, Err((MSG_DENIED, 2)));
        // An AUTH_SYS body with 17 groups.
        let mut c = Writer::new();
        c.u32(0).opaque(b"m").u32(1).u32(1).u32(17);
        for _ in 0..17 {
            c.u32(1);
        }
        let mut w = Writer::new();
        w.u32(2).u32(CALL).u32(2).u32(100_003).u32(3).u32(0).u32(AUTH_SYS).opaque(c.bytes());
        w.u32(0).u32(0);
        let Err(Bad::Reply(r)) = parse_call(w.bytes()) else { panic!() };
        assert_eq!(parse_reply(&r).unwrap().1, Err((MSG_DENIED, 101)));
        // A header cut short, and a record without even a transaction id.
        let Err(Bad::Reply(r)) = parse_call(&msg[..10]) else { panic!() };
        assert_eq!(parse_reply(&r).unwrap().1, Err((MSG_ACCEPTED, 4)));
        assert_eq!(parse_call(&[0, 0]).err(), Some(Bad::Drop));
    }

    #[test]
    fn records_reassemble_fragments_and_cap_their_size() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&3u32.to_be_bytes());
        stream.extend_from_slice(b"abc");
        stream.extend_from_slice(&(0x8000_0000u32 | 2).to_be_bytes());
        stream.extend_from_slice(b"de");
        let mut out = Vec::new();
        write_record(&mut out, b"xyz").unwrap();
        stream.extend_from_slice(&out);
        let mut r = &stream[..];
        assert_eq!(read_record(&mut r).unwrap().unwrap(), b"abcde");
        assert_eq!(read_record(&mut r).unwrap().unwrap(), b"xyz");
        assert!(read_record(&mut r).unwrap().is_none());
        let huge = (0x8000_0000u32 | (MAX_RECORD as u32 + 1)).to_be_bytes();
        assert!(read_record(&mut &huge[..]).is_err());
        // A stream cut inside a record is an error, not a clean end.
        assert!(read_record(&mut &stream[..5]).is_err());
    }
}
