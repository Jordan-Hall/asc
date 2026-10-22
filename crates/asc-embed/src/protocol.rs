//! Helper binary protocol: length-prefixed JSON frames over the helper's stdin and stdout.
//!
//! A frame is `u32 LE length` then that many bytes of JSON. The helper reads
//! [`Request`] frames and answers each with one [`Response`] frame, in order, until `Shutdown`
//! or end of input. Every bound below is checked before allocation.

use crate::error::Error;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

/// Protocol version, returned by `Hello`.
pub const VERSION: u32 = 1;
/// Largest frame in bytes.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
/// Most texts in one `Embed` request.
pub const MAX_TEXTS: usize = 256;

/// Request from the parent process.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Ask for version and model identity.
    Hello,
    /// Embed texts; the response holds one vector per text.
    Embed {
        /// Texts to embed.
        texts: Vec<String>,
    },
    /// Stop the helper.
    Shutdown,
}

/// Response from the helper.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Response {
    /// Answer to `Hello`.
    Hello {
        /// Protocol version.
        version: u32,
        /// Model name.
        model: String,
        /// Output dimensions.
        dims: usize,
    },
    /// Answer to `Embed`.
    Vectors {
        /// One f32 vector per input text.
        vectors: Vec<Vec<f32>>,
    },
    /// The request failed.
    Error {
        /// Message.
        message: String,
    },
}

/// Write one frame.
pub fn write_frame<T: Serialize>(w: &mut impl Write, msg: &T) -> Result<(), Error> {
    let body = serde_json::to_vec(msg).map_err(|e| Error::Protocol(e.to_string()))?;
    if body.len() > MAX_FRAME {
        return Err(Error::Protocol("frame too large".to_owned()));
    }
    w.write_all(&(body.len() as u32).to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()?;
    Ok(())
}

/// Read one frame; `None` on clean end of input before a header.
pub fn read_frame<T: for<'de> Deserialize<'de>>(r: &mut impl Read) -> Result<Option<T>, Error> {
    let mut head = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut head[got..])? {
            0 if got == 0 => return Ok(None),
            0 => return Err(Error::Protocol("truncated header".to_owned())),
            n => got += n,
        }
    }
    let len = u32::from_le_bytes(head) as usize;
    if len > MAX_FRAME {
        return Err(Error::Protocol(format!("frame of {len} bytes exceeds bound")));
    }
    let mut body = Vec::new();
    r.take(len as u64).read_to_end(&mut body)?;
    if body.len() != len {
        return Err(Error::Protocol("truncated body".to_owned()));
    }
    serde_json::from_slice(&body).map(Some).map_err(|e| Error::Protocol(e.to_string()))
}

/// Serve requests from `input` to `output` with `embedder` until `Shutdown` or end of input.
pub fn serve(
    embedder: &mut dyn crate::runtime::Embedder,
    input: &mut impl Read,
    output: &mut impl Write,
) -> Result<(), Error> {
    while let Some(req) = read_frame::<Request>(input)? {
        let resp = match req {
            Request::Shutdown => return Ok(()),
            Request::Hello => {
                Response::Hello { version: VERSION, model: embedder.name().to_owned(), dims: embedder.dims() }
            }
            Request::Embed { texts } if texts.len() > MAX_TEXTS => {
                Response::Error { message: format!("more than {MAX_TEXTS} texts") }
            }
            Request::Embed { texts } => match embedder.embed(&texts) {
                Ok(vectors) => Response::Vectors { vectors },
                Err(e) => Response::Error { message: e.to_string() },
            },
        };
        write_frame(output, &resp)?;
    }
    Ok(())
}
