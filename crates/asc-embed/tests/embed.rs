use asc_embed::protocol::{self, Request, Response};
use asc_embed::registry::{ModelFiles, ModelRecord, Pooling, sha256_hex as h};
use asc_embed::{Calibration, Calibrator, Embedder, Error, Registry};

struct Fake;
impl Embedder for Fake {
    fn name(&self) -> &str {
        "fake"
    }
    fn dims(&self) -> usize {
        2
    }
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
        Ok(texts.iter().map(|t| vec![t.len() as f32, 1.0]).collect())
    }
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn record(licence: &'static str, f: &ModelFiles) -> ModelRecord {
    ModelRecord {
        name: "t",
        licence,
        dims: 2,
        max_length: 8,
        pooling: Pooling::Mean,
        onnx_sha256: leak(h(&f.onnx)),
        tokenizer_sha256: leak(h(&f.tokenizer)),
        config_sha256: leak(h(&f.config)),
        special_tokens_sha256: leak(h(&f.special_tokens)),
        tokenizer_config_sha256: leak(h(&f.tokenizer_config)),
    }
}

fn files() -> ModelFiles {
    ModelFiles {
        onnx: b"a".to_vec(),
        tokenizer: b"b".to_vec(),
        config: b"c".to_vec(),
        special_tokens: b"d".to_vec(),
        tokenizer_config: b"e".to_vec(),
    }
}

#[test]
fn verify_accepts_pinned_and_rejects_tampered() {
    let f = files();
    record("MIT", &f).verify(&f).unwrap();
    let mut bad = f.clone();
    bad.onnx.push(0);
    assert!(matches!(
        record("MIT", &f).verify(&bad),
        Err(Error::Digest { name: "onnx", .. })
    ));
}

#[test]
fn licence_allow_list() {
    let f = files();
    assert!(matches!(
        record("CC-BY-NC-4.0", &f).verify(&f),
        Err(Error::Licence(_))
    ));
    assert!(Registry::new(vec![record("GPL-3.0", &f)]).get("t").is_err());
    assert!(
        Registry::builtin()
            .get("jina-embeddings-v2-base-code")
            .unwrap()
            .licence_allowed()
    );
}

#[test]
fn calibration_quantises_and_roundtrips() {
    let mut c = Calibrator::new(2).unwrap();
    c.observe(&[0.0, -1.0]).unwrap();
    c.observe(&[2.0, 1.0]).unwrap();
    assert!(c.observe(&[f32::NAN, 0.0]).is_err());
    let t = c.finish().unwrap();
    assert_eq!(t.quantise(&[0.0, 1.0]).unwrap(), vec![-127, 127]);
    assert_eq!(t.quantise(&[5.0, -9.0]).unwrap(), vec![127, -127]);
    let again = Calibration::from_verified_bytes(&t.to_bytes(), &t.digest()).unwrap();
    assert_eq!(again, t);
    let mut tampered = t.to_bytes();
    tampered[5] ^= 1;
    assert!(Calibration::from_verified_bytes(&tampered, &t.digest()).is_err());
    assert!(Calibration::from_bytes(&[0xff; 4]).is_err());
    assert!(Calibrator::new(2).unwrap().finish().is_err());
}

#[test]
fn protocol_roundtrip() {
    let mut input = Vec::new();
    protocol::write_frame(&mut input, &Request::Hello).unwrap();
    protocol::write_frame(
        &mut input,
        &Request::Embed {
            texts: vec!["abc".into()],
        },
    )
    .unwrap();
    protocol::write_frame(&mut input, &Request::Shutdown).unwrap();
    let mut out = Vec::new();
    protocol::serve(&mut Fake, &mut input.as_slice(), &mut out).unwrap();
    let mut r = out.as_slice();
    assert_eq!(
        protocol::read_frame::<Response>(&mut r).unwrap(),
        Some(Response::Hello {
            version: 1,
            model: "fake".into(),
            dims: 2
        })
    );
    assert_eq!(
        protocol::read_frame::<Response>(&mut r).unwrap(),
        Some(Response::Vectors {
            vectors: vec![vec![3.0, 1.0]]
        })
    );
    assert_eq!(protocol::read_frame::<Response>(&mut r).unwrap(), None);
}

#[test]
fn protocol_bounds() {
    let huge = ((protocol::MAX_FRAME + 1) as u32).to_le_bytes();
    assert!(protocol::read_frame::<Request>(&mut huge.as_slice()).is_err());
    assert!(protocol::read_frame::<Request>(&mut [1u8, 0].as_slice()).is_err());
    let mut short = 10u32.to_le_bytes().to_vec();
    short.extend_from_slice(b"{}");
    assert!(protocol::read_frame::<Request>(&mut short.as_slice()).is_err());
    let mut input = Vec::new();
    let texts = vec![String::new(); protocol::MAX_TEXTS + 1];
    protocol::write_frame(&mut input, &Request::Embed { texts }).unwrap();
    let mut out = Vec::new();
    protocol::serve(&mut Fake, &mut input.as_slice(), &mut out).unwrap();
    assert!(matches!(
        protocol::read_frame::<Response>(&mut out.as_slice()).unwrap(),
        Some(Response::Error { .. })
    ));
}

/// Needs the model files in `ASC_EMBED_MODEL_DIR`; run with `--ignored`.
#[test]
#[ignore]
fn real_model_embeds() {
    let dir = std::path::PathBuf::from(std::env::var("ASC_EMBED_MODEL_DIR").unwrap());
    let rd = |n: &str| std::fs::read(dir.join(n)).unwrap();
    let files = ModelFiles {
        onnx: rd("model.onnx"),
        tokenizer: rd("tokenizer.json"),
        config: rd("config.json"),
        special_tokens: rd("special_tokens_map.json"),
        tokenizer_config: rd("tokenizer_config.json"),
    };
    let rec = Registry::builtin()
        .get("jina-embeddings-v2-base-code")
        .unwrap()
        .clone();
    let mut e = asc_embed::FastEmbedder::load(&rec, files).unwrap();
    let v = e.embed(&["fn main() {}".to_owned()]).unwrap();
    assert_eq!(v[0].len(), 768);
}
