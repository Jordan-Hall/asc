//! JSON that keeps the order of an object's members.
//!
//! PHP arrays are ordered, and so are the files Composer writes: the hash it computes of a
//! `composer.json` and the layout of a `composer.lock` entry depend on the order the keys were written
//! in. `serde_json`'s maps do not promise that order, so the few places that need it read the text with
//! this reader. A number that is not an integer is carried as written and refused where it would be hashed.

/// A JSON value that keeps the order of an object's members, as PHP's arrays do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ordered {
    Null,
    Bool(bool),
    Int(String),
    /// Any other number, as written. It cannot be hashed (PHP would write a float differently).
    Num(String),
    Str(String),
    Arr(Vec<Ordered>),
    Obj(Vec<(String, Ordered)>),
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
    depth: usize,
}

impl P<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Result<(), String> {
        self.ws();
        if self.b.get(self.i) == Some(&c) {
            self.i += 1;
            Ok(())
        } else {
            Err("the JSON is malformed".into())
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.eat(b'"')?;
        let mut out: Vec<u16> = Vec::new();
        loop {
            let c = *self.b.get(self.i).ok_or("the JSON is malformed")?;
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = *self.b.get(self.i).ok_or("the JSON is malformed")?;
                    self.i += 1;
                    let unit = match e {
                        b'"' => u16::from(b'"'),
                        b'\\' => u16::from(b'\\'),
                        b'/' => u16::from(b'/'),
                        b'b' => 8,
                        b'f' => 12,
                        b'n' => 10,
                        b'r' => 13,
                        b't' => 9,
                        b'u' => {
                            let h = self
                                .b
                                .get(self.i..self.i + 4)
                                .ok_or("the JSON is malformed")?;
                            self.i += 4;
                            u16::from_str_radix(
                                std::str::from_utf8(h).map_err(|_| "bad escape")?,
                                16,
                            )
                            .map_err(|_| "the JSON has a bad escape".to_owned())?
                        }
                        _ => return Err("the JSON has a bad escape".into()),
                    };
                    out.push(unit);
                }
                _ => {
                    // A UTF-8 sequence: take it whole.
                    let start = self.i - 1;
                    let len = match c {
                        0..=0x7f => 1,
                        0xc0..=0xdf => 2,
                        0xe0..=0xef => 3,
                        _ => 4,
                    };
                    let s = std::str::from_utf8(self.b.get(start..start + len).ok_or("bad UTF-8")?)
                        .map_err(|_| "the JSON is not UTF-8".to_owned())?;
                    out.extend(s.encode_utf16());
                    self.i = start + len;
                }
            }
        }
        String::from_utf16(&out).map_err(|_| "the JSON has an unpaired surrogate".to_owned())
    }

    fn value(&mut self) -> Result<Ordered, String> {
        self.depth += 1;
        if self.depth > 64 {
            return Err("the JSON is nested too deeply".into());
        }
        self.ws();
        let v = match self.b.get(self.i).copied() {
            Some(b'{') => {
                self.i += 1;
                let mut m = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b'}') {
                    self.i += 1;
                } else {
                    loop {
                        self.ws();
                        let k = self.string()?;
                        self.eat(b':')?;
                        let v = self.value()?;
                        // PHP keeps the last of a repeated key, in the first one's place.
                        if let Some(slot) =
                            m.iter_mut().find(|(x, _): &&mut (String, Ordered)| *x == k)
                        {
                            slot.1 = v;
                        } else {
                            m.push((k, v));
                        }
                        self.ws();
                        match self.b.get(self.i) {
                            Some(b',') => self.i += 1,
                            Some(b'}') => {
                                self.i += 1;
                                break;
                            }
                            _ => return Err("the JSON is malformed".into()),
                        }
                    }
                }
                Ordered::Obj(m)
            }
            Some(b'[') => {
                self.i += 1;
                let mut a = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b']') {
                    self.i += 1;
                } else {
                    loop {
                        a.push(self.value()?);
                        self.ws();
                        match self.b.get(self.i) {
                            Some(b',') => self.i += 1,
                            Some(b']') => {
                                self.i += 1;
                                break;
                            }
                            _ => return Err("the JSON is malformed".into()),
                        }
                    }
                }
                Ordered::Arr(a)
            }
            Some(b'"') => Ordered::Str(self.string()?),
            Some(b't') if self.b[self.i..].starts_with(b"true") => {
                self.i += 4;
                Ordered::Bool(true)
            }
            Some(b'f') if self.b[self.i..].starts_with(b"false") => {
                self.i += 5;
                Ordered::Bool(false)
            }
            Some(b'n') if self.b[self.i..].starts_with(b"null") => {
                self.i += 4;
                Ordered::Null
            }
            Some(c) if c == b'-' || c.is_ascii_digit() => {
                let start = self.i;
                while self.i < self.b.len()
                    && matches!(
                        self.b[self.i],
                        b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9'
                    )
                {
                    self.i += 1;
                }
                let t = std::str::from_utf8(&self.b[start..self.i]).unwrap_or("");
                if t.parse::<i64>().is_ok() {
                    Ordered::Int(t.to_owned())
                } else if t.parse::<f64>().is_ok_and(f64::is_finite) {
                    Ordered::Num(t.to_owned())
                } else {
                    return Err("a number in the JSON is not usable".into());
                }
            }
            _ => return Err("the JSON is malformed".into()),
        };
        self.depth -= 1;
        Ok(v)
    }
}

pub fn parse_ordered(text: &str) -> Result<Ordered, String> {
    let mut p = P {
        b: text.as_bytes(),
        i: 0,
        depth: 0,
    };
    let v = p.value()?;
    p.ws();
    if p.i != p.b.len() {
        return Err("the JSON has text after the value".into());
    }
    Ok(v)
}

impl Ordered {
    /// The value with every object's keys sorted, as one string, for comparing two values that may
    /// list their keys in another order.
    pub fn canon(&self) -> String {
        match self {
            Self::Null => "null".into(),
            Self::Bool(b) => b.to_string(),
            Self::Int(n) | Self::Num(n) => n.clone(),
            Self::Str(s) => format!("{s:?}"),
            Self::Arr(a) => format!(
                "[{}]",
                a.iter().map(Self::canon).collect::<Vec<_>>().join(",")
            ),
            Self::Obj(m) => {
                let mut items: Vec<String> = m
                    .iter()
                    .map(|(k, v)| format!("{k:?}:{}", v.canon()))
                    .collect();
                items.sort();
                format!("{{{}}}", items.join(","))
            }
        }
    }

    /// Whether a number that is not an integer occurs anywhere inside.
    pub fn has_float(&self) -> bool {
        match self {
            Self::Num(_) => true,
            Self::Arr(a) => a.iter().any(Self::has_float),
            Self::Obj(m) => m.iter().any(|(_, v)| v.has_float()),
            _ => false,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Ordered> {
        match self {
            Self::Obj(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }
}
