//! Canonical encoding of parsed TOML, JSON and XML values (SPEC §12A.4).
//!
//! Maps are sorted by key, so two documents that differ only in key order or
//! formatting encode to the same bytes. Fingerprints hash that encoding.

use std::io::Cursor;

use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use serde::{Deserialize, Serialize};

use crate::model::{Fingerprint, MAX_DEPTH};

/// Version byte of the canonical encoding.
const VERSION: u8 = 1;
/// `blake3::derive_key` context for project fingerprints.
const FP_CONTEXT: &str = "asc v1 project fp";

/// Why a value could not be canonicalised.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CanonError {
    #[error("nesting deeper than {limit}")]
    TooDeep { limit: usize },
    #[error("duplicate map key {0:?}")]
    DuplicateKey(String),
    #[error("map keys are not sorted")]
    Unsorted,
    #[error("integer above i64::MAX must use Uint")]
    NonCanonicalInt,
    #[error("xml: {0}")]
    Xml(String),
}

/// A parsed document value in canonical form.
///
/// Variant order is frozen: postcard encodes the variant index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Bool(bool),
    /// Every integer up to `i64::MAX`.
    Int(i64),
    /// Only integers above `i64::MAX`.
    Uint(u64),
    /// IEEE-754 bits, little-endian; NaN is normalised to one pattern.
    Float([u8; 8]),
    Str(String),
    Array(Vec<Value>),
    /// Sorted by key, keys unique.
    Map(Vec<(String, Value)>),
}

impl Value {
    /// A float with canonical NaN.
    pub fn float(f: f64) -> Self {
        Value::Float(
            if f.is_nan() { f64::NAN } else { f }
                .to_bits()
                .to_le_bytes(),
        )
    }

    /// A map from unordered pairs; sorts and rejects duplicate keys.
    pub fn map(pairs: impl IntoIterator<Item = (String, Value)>) -> Result<Self, CanonError> {
        let mut v: Vec<_> = pairs.into_iter().collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        if let Some(w) = v.windows(2).find(|w| w[0].0 == w[1].0) {
            return Err(CanonError::DuplicateKey(w[0].0.clone()));
        }
        Ok(Value::Map(v))
    }

    /// Checks the invariants decoding cannot: depth, key order, integer form.
    pub fn validate(&self) -> Result<(), CanonError> {
        self.validate_at(1)
    }

    fn validate_at(&self, depth: usize) -> Result<(), CanonError> {
        if depth > MAX_DEPTH {
            return Err(CanonError::TooDeep { limit: MAX_DEPTH });
        }
        match self {
            Value::Uint(u) if i64::try_from(*u).is_ok() => Err(CanonError::NonCanonicalInt),
            Value::Array(a) => a.iter().try_for_each(|v| v.validate_at(depth + 1)),
            Value::Map(m) => {
                if m.windows(2).any(|w| w[0].0 >= w[1].0) {
                    return Err(CanonError::Unsorted);
                }
                m.iter().try_for_each(|(_, v)| v.validate_at(depth + 1))
            }
            _ => Ok(()),
        }
    }

    /// Canonical bytes: the version byte, then the postcard encoding.
    pub fn encode(&self) -> Vec<u8> {
        asc_core::codec::encode_versioned(VERSION, self)
    }

    /// The fingerprint of this value.
    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::from(blake3::derive_key(FP_CONTEXT, &self.encode()))
    }
}

/// Converts parsed JSON.
pub fn from_json(v: &serde_json::Value) -> Result<Value, CanonError> {
    json_at(v, 1)
}

fn json_at(v: &serde_json::Value, depth: usize) -> Result<Value, CanonError> {
    if depth > MAX_DEPTH {
        return Err(CanonError::TooDeep { limit: MAX_DEPTH });
    }
    use serde_json::Value as J;
    Ok(match v {
        J::Null => Value::Null,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => Value::Int(i),
            (None, Some(u)) => Value::Uint(u),
            _ => Value::float(n.as_f64().unwrap_or(f64::NAN)),
        },
        J::String(s) => Value::Str(s.clone()),
        J::Array(a) => Value::Array(
            a.iter()
                .map(|x| json_at(x, depth + 1))
                .collect::<Result<_, _>>()?,
        ),
        J::Object(o) => Value::map(
            o.iter()
                .map(|(k, x)| Ok((k.clone(), json_at(x, depth + 1)?)))
                .collect::<Result<Vec<_>, CanonError>>()?,
        )?,
    })
}

/// Converts parsed TOML. Datetimes become their TOML text.
pub fn from_toml(v: &toml::Value) -> Result<Value, CanonError> {
    toml_at(v, 1)
}

fn toml_at(v: &toml::Value, depth: usize) -> Result<Value, CanonError> {
    if depth > MAX_DEPTH {
        return Err(CanonError::TooDeep { limit: MAX_DEPTH });
    }
    use toml::Value as T;
    Ok(match v {
        T::String(s) => Value::Str(s.clone()),
        T::Integer(i) => Value::Int(*i),
        T::Float(f) => Value::float(*f),
        T::Boolean(b) => Value::Bool(*b),
        T::Datetime(d) => Value::Str(d.to_string()),
        T::Array(a) => Value::Array(
            a.iter()
                .map(|x| toml_at(x, depth + 1))
                .collect::<Result<_, _>>()?,
        ),
        T::Table(t) => Value::map(
            t.iter()
                .map(|(k, x)| Ok((k.clone(), toml_at(x, depth + 1)?)))
                .collect::<Result<Vec<_>, CanonError>>()?,
        )?,
    })
}

/// Converts an XML document to its root element.
///
/// An element is a map with `name`, `attrs` (a map), `children` (an array of
/// elements) and `text` (trimmed text runs joined by a space). Declarations,
/// comments and processing instructions are dropped.
pub fn from_xml(bytes: &[u8]) -> Result<Value, CanonError> {
    let mut reader = Reader::from_reader(Cursor::new(bytes));
    let mut buf = Vec::new();
    let mut stack: Vec<Elem> = Vec::new();
    let mut root = None;
    loop {
        let ev = reader
            .read_event_into(&mut buf)
            .map_err(|e| CanonError::Xml(e.to_string()))?;
        match ev {
            Event::Start(e) => {
                if stack.len() + 1 > MAX_DEPTH {
                    return Err(CanonError::TooDeep { limit: MAX_DEPTH });
                }
                stack.push(Elem::open(&e)?);
            }
            Event::Empty(e) => {
                if stack.len() + 1 > MAX_DEPTH {
                    return Err(CanonError::TooDeep { limit: MAX_DEPTH });
                }
                close(Elem::open(&e)?, &mut stack, &mut root)?;
            }
            Event::End(_) => {
                let e = stack
                    .pop()
                    .ok_or_else(|| CanonError::Xml("unbalanced end tag".into()))?;
                close(e, &mut stack, &mut root)?;
            }
            Event::Text(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&t.xml10_content());
                }
            }
            Event::CData(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&t.xml10_content());
                }
            }
            Event::GeneralRef(r) => {
                let resolved = match r.resolve_char_ref() {
                    Ok(Some(c)) => c.to_string(),
                    Ok(None) => resolve_predefined_entity(&r)
                        .map(str::to_owned)
                        .ok_or_else(|| CanonError::Xml(format!("unknown entity &{};", &*r)))?,
                    Err(e) => return Err(CanonError::Xml(e.to_string())),
                };
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&resolved);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    if !stack.is_empty() {
        return Err(CanonError::Xml("unclosed element".into()));
    }
    root.ok_or_else(|| CanonError::Xml("no root element".into()))
}

struct Elem {
    name: String,
    attrs: Vec<(String, Value)>,
    children: Vec<Value>,
    text: String,
}

impl Elem {
    fn open(e: &BytesStart<'_>) -> Result<Self, CanonError> {
        let xml = |e: &dyn ToString| CanonError::Xml(e.to_string());
        let mut attrs = Vec::new();
        for a in e.attributes() {
            let a = a.map_err(|e| xml(&e))?;
            let key = a.key.as_ref().to_owned();
            let val = a
                .normalized_value(XmlVersion::Implicit1_0)
                .map_err(|e| xml(&e))?
                .into_owned();
            attrs.push((key, Value::Str(val)));
        }
        Ok(Self {
            name: e.name().as_ref().to_owned(),
            attrs,
            children: Vec::new(),
            text: String::new(),
        })
    }

    fn finish(self) -> Result<Value, CanonError> {
        Value::map([
            ("name".to_owned(), Value::Str(self.name)),
            ("attrs".to_owned(), Value::map(self.attrs)?),
            ("children".to_owned(), Value::Array(self.children)),
            (
                "text".to_owned(),
                Value::Str(self.text.split_whitespace().collect::<Vec<_>>().join(" ")),
            ),
        ])
    }
}

fn close(e: Elem, stack: &mut [Elem], root: &mut Option<Value>) -> Result<(), CanonError> {
    let v = e.finish()?;
    match stack.last_mut() {
        Some(parent) => parent.children.push(v),
        None if root.is_none() => *root = Some(v),
        None => return Err(CanonError::Xml("multiple root elements".into())),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn key_order_does_not_matter() {
        let a = from_json(&serde_json::json!({"b": 1, "a": [true, null]})).unwrap();
        let b = from_json(&serde_json::json!({"a": [true, null], "b": 1})).unwrap();
        assert_eq!(a.encode(), b.encode());
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn toml_and_json_agree() {
        let t: toml::Value = toml::from_str(
            "b = 1
[a]
x = \"y\"
",
        )
        .unwrap();
        let j = serde_json::json!({"a": {"x": "y"}, "b": 1});
        assert_eq!(from_toml(&t).unwrap(), from_json(&j).unwrap());
    }

    #[test]
    fn golden_encoding() {
        let v = from_json(&serde_json::json!({"b": [1, "x"], "a": null})).unwrap();
        // version 1, Map(2): "a" Null, "b" Array(Int 1 -> zigzag 2, Str "x")
        assert_eq!(hex(&v.encode()), "010702016100016206020202050178");
    }

    #[test]
    fn golden_fingerprint() {
        let v = Value::Array(vec![
            Value::Int(-1),
            Value::Uint(u64::MAX),
            Value::float(1.5),
        ]);
        assert_eq!(
            hex(&v.encode()),
            "010603020103ffffffffffffffffff0104000000000000f83f"
        );
        assert_eq!(
            hex(v.fingerprint().as_bytes()),
            "9db6c60e98f8215b183472b5bd8d56ef6d7739c3d6d6da18c8acd8b1e18669b9"
        );
    }

    #[test]
    fn integers_and_floats() {
        let big = from_json(&serde_json::json!(u64::MAX)).unwrap();
        assert_eq!(big, Value::Uint(u64::MAX));
        assert!(big.validate().is_ok());
        assert_eq!(Value::Uint(5).validate(), Err(CanonError::NonCanonicalInt));
        assert_eq!(Value::float(f64::NAN), Value::float(-f64::NAN));
    }

    #[test]
    fn duplicate_and_unsorted_keys() {
        let dup = Value::map([("a".into(), Value::Null), ("a".into(), Value::Null)]);
        assert_eq!(dup, Err(CanonError::DuplicateKey("a".into())));
        let bad = Value::Map(vec![("b".into(), Value::Null), ("a".into(), Value::Null)]);
        assert_eq!(bad.validate(), Err(CanonError::Unsorted));
    }

    fn nested(n: usize) -> serde_json::Value {
        let mut v = serde_json::Value::Null;
        for _ in 1..n {
            v = serde_json::Value::Array(vec![v]);
        }
        v
    }

    #[test]
    fn depth_limit() {
        assert!(from_json(&nested(MAX_DEPTH)).is_ok());
        assert_eq!(
            from_json(&nested(MAX_DEPTH + 1)),
            Err(CanonError::TooDeep { limit: MAX_DEPTH })
        );
        let mut v = Value::Null;
        for _ in 0..MAX_DEPTH {
            v = Value::Array(vec![v]);
        }
        assert_eq!(v.validate(), Err(CanonError::TooDeep { limit: MAX_DEPTH }));
    }

    #[test]
    fn xml_depth_limit() {
        let ok = format!("{}{}", "<a>".repeat(MAX_DEPTH), "</a>".repeat(MAX_DEPTH));
        assert!(from_xml(ok.as_bytes()).is_ok());
        let deep = format!(
            "{}{}",
            "<a>".repeat(MAX_DEPTH + 1),
            "</a>".repeat(MAX_DEPTH + 1)
        );
        assert_eq!(
            from_xml(deep.as_bytes()),
            Err(CanonError::TooDeep { limit: MAX_DEPTH })
        );
    }

    #[test]
    fn xml_attribute_order_does_not_matter() {
        let a = from_xml(br#"<?xml version="1.0"?><Project Sdk="x" a="1"><!-- c --><P Include="p">t</P><E/></Project>"#).unwrap();
        let b =
            from_xml(br#"<Project a="1" Sdk="x"><P Include="p"> t </P><E></E></Project>"#).unwrap();
        assert_eq!(a, b);
        let Value::Map(m) = &a else { panic!() };
        assert_eq!(m[2].0, "name");
        assert_eq!(m[2].1, Value::Str("Project".into()));
        let Value::Array(kids) = &m[1].1 else {
            panic!()
        };
        assert_eq!(kids.len(), 2);
    }

    #[test]
    fn xml_errors() {
        assert!(from_xml(b"").is_err());
        assert!(from_xml(b"<a><b></a>").is_err());
        assert!(from_xml(b"<a/><b/>").is_err());
    }
}
