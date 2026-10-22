use prost_reflect::{DescriptorPool, DynamicMessage, ReflectMessage as _, Value as PbValue};
use protox::Compiler;
use protox::file::{ChainFileResolver, File, FileResolver, GoogleFileResolver};
use serde_json::{Value, json};

use super::{PROFILE, decode, encode};

const SRC: &str = r#"
syntax = "proto3";
package asc.v1;
import "asc/v1/options.proto";
enum DiffFormat { DIFF_FORMAT_UNSPECIFIED = 0; DIFF_FORMAT_PATCH = 1; DIFF_FORMAT_STAT = 2; }
message Signature { bytes name = 1 [(asc.v1.json) = JSON_KIND_TEXT]; bytes email = 2 [(asc.v1.json) = JSON_KIND_TEXT]; int64 time = 3; int32 tz_min = 4; }
message Node { Node next = 1; }
message Inner { bytes id = 1 [(asc.v1.json) = JSON_KIND_HEX]; }
message T {
  bytes id = 1 [(asc.v1.json) = JSON_KIND_HEX];
  bytes change = 2 [(asc.v1.json) = JSON_KIND_CHANGE];
  bytes path = 3 [(asc.v1.json) = JSON_KIND_PATH];
  bytes text = 4 [(asc.v1.json) = JSON_KIND_TEXT];
  bytes data = 5 [(asc.v1.json) = JSON_KIND_DATA];
  bytes plain = 6;
  DiffFormat format = 7;
  uint64 big = 8;
  Signature who = 9;
  repeated Inner inner = 10;
  map<string, int32> counts = 11;
  repeated bytes ids = 12 [(asc.v1.json) = JSON_KIND_HEX];
  Inner nested = 13;
}
"#;

struct Mem;
impl FileResolver for Mem {
    fn open_file(&self, name: &str) -> Result<File, protox::Error> {
        match name {
            "t.proto" => File::from_source(name, SRC),
            "asc/v1/options.proto" => File::from_source(
                name,
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../proto/asc/v1/options.proto"
                )),
            ),
            _ => Err(protox::Error::file_not_found(name)),
        }
    }
}

fn pool() -> DescriptorPool {
    let mut chain = ChainFileResolver::new();
    chain.add(Mem);
    chain.add(GoogleFileResolver::new());
    let mut c = Compiler::with_file_resolver(chain);
    c.include_imports(true);
    c.open_file("t.proto").unwrap();
    c.descriptor_pool()
}

fn msg(name: &str) -> (DescriptorPool, DynamicMessage) {
    let p = pool();
    let m = DynamicMessage::new(p.get_message_by_name(name).unwrap());
    (p, m)
}

fn set(m: &mut DynamicMessage, f: &str, v: PbValue) {
    m.set_field_by_name(f, v);
}

fn bytes(b: &[u8]) -> PbValue {
    PbValue::Bytes(b.to_vec().into())
}

/// Encodes, checks the golden JSON, decodes and checks the round trip.
fn golden(m: &DynamicMessage, want: &Value) {
    let got = encode(m).unwrap();
    assert_eq!(&got, want);
    let back = decode(&m.descriptor(), &got).unwrap();
    assert_eq!(&back, m);
}

#[test]
fn json_profile_name() {
    assert_eq!(PROFILE, "json.profile.v1");
}

#[test]
fn json_every_kind_round_trips() {
    let (_p, mut m) = msg("asc.v1.T");
    set(&mut m, "id", bytes(&[0xab, 0x01, 0xff]));
    set(&mut m, "change", bytes(&[0x00, 0x1f, 0xf0]));
    set(&mut m, "path", bytes(b"src/lib.rs"));
    set(&mut m, "text", bytes("héllo".as_bytes()));
    set(&mut m, "data", bytes(b"fn main() {}\n"));
    set(&mut m, "plain", bytes(&[1, 2, 3]));
    set(&mut m, "format", PbValue::EnumNumber(1));
    set(&mut m, "big", PbValue::U64(1 << 40));
    golden(
        &m,
        &json!({
            "id": "ab01ff",
            "change": "zzykkz",
            "path": "src/lib.rs",
            "text": "héllo",
            "data": "fn main() {}\n",
            "plain": "AQID",
            "format": "patch",
            "big": 1u64 << 40,
        }),
    );
}

#[test]
fn json_change_id_alphabet() {
    let (_p, mut m) = msg("asc.v1.T");
    set(&mut m, "change", bytes(&[0x0f, 0xf0]));
    // hex digit d becomes 'z' - d
    assert_eq!(encode(&m).unwrap()["change"], "zkkz");
}

#[test]
fn json_non_utf8_path_is_b64() {
    let (_p, mut m) = msg("asc.v1.T");
    set(&mut m, "path", bytes(&[0x66, 0xff, 0xfe]));
    golden(&m, &json!({"path": {"b64": "Zv/+"}}));
}

#[test]
fn json_signature_prints_tz() {
    let (_p, mut m) = msg("asc.v1.Signature");
    set(&mut m, "name", bytes(b"Ada"));
    set(&mut m, "email", bytes(b"ada@example.com"));
    set(&mut m, "time", PbValue::I64(1_700_000_000));
    set(&mut m, "tz_min", PbValue::I32(330));
    golden(
        &m,
        &json!({"name": "Ada", "email": "ada@example.com", "time": 1_700_000_000, "tz": "+0530"}),
    );
    set(&mut m, "tz_min", PbValue::I32(-480));
    assert_eq!(encode(&m).unwrap()["tz"], "-0800");
    set(&mut m, "tz_min", PbValue::I32(0));
    assert_eq!(encode(&m).unwrap()["tz"], "+0000");
}

#[test]
fn json_binary_data_prints_flag_and_no_data() {
    let (_p, mut m) = msg("asc.v1.T");
    set(&mut m, "data", bytes(&[0, 1, 2, 0xff]));
    let j = encode(&m).unwrap();
    assert_eq!(j, json!({"binary": true}));
    // decoding accepts what encoding prints; the data itself is not carried
    let back = decode(&m.descriptor(), &j).unwrap();
    assert!(!back.has_field_by_name("data"));
}

#[test]
fn json_nested_repeated_and_map() {
    let (p, mut m) = msg("asc.v1.T");
    let inner = p.get_message_by_name("asc.v1.Inner").unwrap();
    let mut i = DynamicMessage::new(inner);
    i.set_field_by_name("id", bytes(&[0xde, 0xad]));
    set(
        &mut m,
        "inner",
        PbValue::List(vec![PbValue::Message(i.clone())]),
    );
    set(&mut m, "nested", PbValue::Message(i));
    set(&mut m, "ids", PbValue::List(vec![bytes(&[1]), bytes(&[2])]));
    set(
        &mut m,
        "counts",
        PbValue::Map([(prost_reflect::MapKey::String("a".into()), PbValue::I32(3))].into()),
    );
    golden(
        &m,
        &json!({
            "inner": [{"id": "dead"}],
            "nested": {"id": "dead"},
            "ids": ["01", "02"],
            "counts": {"a": 3},
        }),
    );
}

#[test]
fn json_enum_accepts_full_name() {
    let (_p, m) = msg("asc.v1.T");
    let d = m.descriptor();
    let a = decode(&d, &json!({"format": "DIFF_FORMAT_STAT"})).unwrap();
    let b = decode(&d, &json!({"format": "stat"})).unwrap();
    assert_eq!(a, b);
    assert_eq!(encode(&a).unwrap(), json!({"format": "stat"}));
    assert!(decode(&d, &json!({"format": "nope"})).is_err());
}

#[test]
fn json_rejects_bad_input() {
    let (_p, m) = msg("asc.v1.T");
    let d = m.descriptor();
    for bad in [
        json!({"unknown": 1}),
        json!({"id": "abc"}),
        json!({"id": "zz"}),
        json!({"change": "abcd"}),
        json!({"big": -1}),
        json!({"big": 1.5}),
        json!({"who": {"tz": "0530"}}),
        json!({"who": {"tz": "+0560"}}),
        json!({"who": {"tz_min": 5}}),
        json!([]),
    ] {
        assert!(decode(&d, &bad).is_err(), "{bad}");
    }
}

#[test]
fn json_error_names_the_path() {
    let (_p, m) = msg("asc.v1.T");
    let e = decode(&m.descriptor(), &json!({"inner": [{"id": "x"}]})).unwrap_err();
    assert!(e.to_string().starts_with("inner.0.id:"), "{e}");
}

#[test]
fn json_integer_above_2_pow_53_is_refused() {
    let (_p, mut m) = msg("asc.v1.T");
    set(&mut m, "big", PbValue::U64((1 << 53) + 1));
    assert!(encode(&m).is_err());
}

#[test]
fn json_descriptor_set_carries_the_option() {
    let pool = DescriptorPool::decode(crate::DESCRIPTOR_SET).unwrap();
    assert!(pool.get_extension_by_name("asc.v1.json").is_some());
    assert!(pool.get_message_by_name("asc.v1.Signature").is_some());
}

#[test]
fn json_depth_is_bounded() {
    let (_p, m) = msg("asc.v1.Node");
    let mut v = json!({});
    for _ in 0..80 {
        v = json!({"next": v});
    }
    assert!(matches!(
        decode(&m.descriptor(), &v),
        Err(super::JsonError::TooDeep)
    ));
    let mut ok = json!({});
    for _ in 0..10 {
        ok = json!({"next": ok});
    }
    assert!(decode(&m.descriptor(), &ok).is_ok());
}
