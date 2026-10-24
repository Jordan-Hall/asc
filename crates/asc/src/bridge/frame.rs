//! `BridgeFrame` framing (SPEC §24.2): `u32 little-endian length ‖ BridgeFrame`, at most 16 MiB
//! per frame. A longer length is a protocol error and ends the bridge: the reader never
//! allocates for a length it refuses.

use asc_rpc::server::wire::BridgeFrame;
use prost_reflect::prost::Message as _;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest frame body.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {0} bytes is larger than the 16 MiB limit")]
    TooLarge(u64),
    #[error("frame does not decode: {0}")]
    Decode(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// The next frame, or `None` at a clean end of input (EOF between frames).
pub async fn read_frame<R: AsyncRead + Unpin>(
    r: &mut R,
) -> Result<Option<BridgeFrame>, FrameError> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < len.len() {
        let n = r.read(&mut len[got..]).await?;
        if n == 0 {
            return if got == 0 {
                Ok(None)
            } else {
                Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into())
            };
        }
        got += n;
    }
    let len = u64::from(u32::from_le_bytes(len));
    if len > MAX_FRAME as u64 {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    BridgeFrame::decode(body.as_slice())
        .map(Some)
        .map_err(|e| FrameError::Decode(e.to_string()))
}

/// The wire form of `frame`; fails when it would exceed the limit.
pub fn encode_frame(frame: &BridgeFrame) -> Result<Vec<u8>, FrameError> {
    let n = frame.encoded_len();
    if n > MAX_FRAME {
        return Err(FrameError::TooLarge(n as u64));
    }
    let mut out = Vec::with_capacity(4 + n);
    out.extend_from_slice(&(n as u32).to_le_bytes());
    frame
        .encode(&mut out)
        .map_err(|e| FrameError::Decode(e.to_string()))?;
    Ok(out)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    frame: &BridgeFrame,
) -> Result<(), FrameError> {
    w.write_all(&encode_frame(frame)?).await?;
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use asc_rpc::server::wire::bridge_frame::F;

    fn reply(id: u32, n: usize) -> BridgeFrame {
        BridgeFrame {
            id,
            f: Some(F::Reply(vec![7u8; n].into())),
        }
    }

    #[tokio::test]
    async fn frames_round_trip_and_eof_is_clean() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &reply(1, 10)).await.unwrap();
        write_frame(&mut buf, &reply(2, 0)).await.unwrap();
        let mut r = buf.as_slice();
        assert_eq!(read_frame(&mut r).await.unwrap(), Some(reply(1, 10)));
        assert_eq!(read_frame(&mut r).await.unwrap(), Some(reply(2, 0)));
        assert_eq!(read_frame(&mut r).await.unwrap(), None);
    }

    #[tokio::test]
    async fn length_is_little_endian() {
        let wire = encode_frame(&reply(1, 3)).unwrap();
        let n = u32::from_le_bytes(wire[..4].try_into().unwrap()) as usize;
        assert_eq!(n, wire.len() - 4);
    }

    #[tokio::test]
    async fn an_oversized_length_is_refused_before_allocating() {
        let wire = ((MAX_FRAME as u32) + 1).to_le_bytes();
        let mut r = wire.as_slice();
        assert!(matches!(
            read_frame(&mut r).await,
            Err(FrameError::TooLarge(_))
        ));
        let huge = u32::MAX.to_le_bytes();
        let mut r = huge.as_slice();
        assert!(matches!(
            read_frame(&mut r).await,
            Err(FrameError::TooLarge(_))
        ));
    }

    #[tokio::test]
    async fn the_limit_itself_is_accepted_on_write() {
        // A reply whose encoding is exactly the limit is refused only above it.
        assert!(encode_frame(&reply(1, MAX_FRAME)).is_err());
        assert!(encode_frame(&reply(1, MAX_FRAME - 16)).is_ok());
    }

    #[tokio::test]
    async fn a_truncated_frame_is_an_error() {
        let mut wire = encode_frame(&reply(1, 10)).unwrap();
        wire.truncate(wire.len() - 1);
        let mut r = wire.as_slice();
        assert!(matches!(read_frame(&mut r).await, Err(FrameError::Io(_))));
        let mut r = &wire[..2];
        assert!(matches!(read_frame(&mut r).await, Err(FrameError::Io(_))));
    }

    #[tokio::test]
    async fn garbage_does_not_decode() {
        let mut wire = 3u32.to_le_bytes().to_vec();
        wire.extend_from_slice(&[0xff, 0xff, 0xff]);
        let mut r = wire.as_slice();
        assert!(matches!(
            read_frame(&mut r).await,
            Err(FrameError::Decode(_))
        ));
    }
}
