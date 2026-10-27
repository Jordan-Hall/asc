//! The price table: what a model costs, in micro-USD per million tokens.
//!
//! The operator supplies it (`[aikeys] price_file`), because prices change and a table baked into
//! a release would be wrong on the day it ships. The file is a JSON object from a model pattern
//! to `[input, output]`:
//!
//! ```json
//! { "claude-sonnet-*": [3000000, 15000000], "gpt-4o": [2500000, 10000000], "*": [0, 0] }
//! ```
//!
//! A pattern is an exact model id or has `*` for any run of characters; the longest matching
//! pattern wins. A model with no entry has no price: a call that must respect a cost limit
//! refuses it (a budget that cannot be measured is not enforced by guessing), and a call with
//! no cost limit is recorded at cost 0 and flagged as unpriced in the usage.

use std::path::Path;

use serde_json::Value;

use super::model::Usage;

/// Largest price file.
const MAX_FILE: u64 = 256 * 1024;
/// Most entries.
const MAX_ENTRIES: usize = 2000;
/// Highest price accepted per million tokens: 100,000 USD.
const MAX_PRICE: u64 = 100_000_000_000;

/// The table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prices {
    /// `(pattern, input, output)`.
    entries: Vec<(String, u64, u64)>,
}

fn glob(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

impl Prices {
    /// A table from JSON text.
    pub fn parse(text: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("price file: {e}"))?;
        let obj = v.as_object().ok_or("price file: expected a JSON object")?;
        if obj.len() > MAX_ENTRIES {
            return Err("price file: too many entries".into());
        }
        let mut entries = Vec::with_capacity(obj.len());
        for (k, v) in obj {
            let pair = v
                .as_array()
                .filter(|a| a.len() == 2)
                .ok_or_else(|| format!("price file: {k:?} is not [input, output]"))?;
            let n = |i: usize| {
                pair[i]
                    .as_u64()
                    .filter(|n| *n <= MAX_PRICE)
                    .ok_or_else(|| format!("price file: {k:?} has a bad price"))
            };
            if k.is_empty() || k.len() > 200 || k.chars().any(char::is_control) {
                return Err(
                    "price file: a model pattern is empty, too long or holds a control character"
                        .into(),
                );
            }
            entries.push((k.clone(), n(0)?, n(1)?));
        }
        // Longest pattern first, so the most specific entry wins.
        entries.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        Ok(Self { entries })
    }

    /// Reads the file at `path`.
    pub fn load(path: &Path) -> Result<Self, String> {
        let meta = std::fs::metadata(path)
            .map_err(|e| format!("price file {}: {}", path.display(), e.kind()))?;
        if meta.len() > MAX_FILE {
            return Err(format!("price file {} is too large", path.display()));
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("price file {}: {}", path.display(), e.kind()))?;
        Self::parse(&text)
    }

    /// The `(input, output)` price of `model`.
    pub fn price(&self, model: &str) -> Option<(u64, u64)> {
        self.entries
            .iter()
            .find(|(p, _, _)| glob(p, model))
            .map(|(_, i, o)| (*i, *o))
    }

    /// The cost of `usage` on `model` in micro-USD, rounded up; `None` when the model has no price.
    pub fn cost(&self, model: &str, usage: Usage) -> Option<u64> {
        let (i, o) = self.price(model)?;
        let micro = u128::from(usage.input_tokens) * u128::from(i)
            + u128::from(usage.output_tokens) * u128::from(o);
        Some(u64::try_from(micro.div_ceil(1_000_000)).unwrap_or(u64::MAX))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_longest_pattern_wins_and_costs_round_up() {
        let p = Prices::parse(r#"{"*": [1, 1], "claude-*": [3000000, 15000000], "claude-haiku-*": [1000000, 5000000]}"#).unwrap();
        assert_eq!(p.price("claude-sonnet-5"), Some((3_000_000, 15_000_000)));
        assert_eq!(p.price("claude-haiku-5-5"), Some((1_000_000, 5_000_000)));
        assert_eq!(p.price("gpt-x"), Some((1, 1)));
        let u = Usage {
            input_tokens: 1000,
            output_tokens: 500,
        };
        // 1000 * 3 + 500 * 15 = 10,500 micro-USD.
        assert_eq!(p.cost("claude-sonnet-5", u), Some(10_500));
        // Rounds up: one token at 1 micro per million is still 1 micro.
        assert_eq!(
            p.cost(
                "gpt-x",
                Usage {
                    input_tokens: 1,
                    output_tokens: 0
                }
            ),
            Some(1)
        );
        assert_eq!(p.cost("gpt-x", Usage::default()), Some(0));
    }

    #[test]
    fn a_model_without_a_price_has_none() {
        let p = Prices::parse(r#"{"gpt-4o": [1, 2]}"#).unwrap();
        assert_eq!(p.price("other"), None);
        assert_eq!(
            p.cost(
                "other",
                Usage {
                    input_tokens: 5,
                    output_tokens: 5
                }
            ),
            None
        );
        assert!(Prices::default().price("x").is_none());
    }

    #[test]
    fn bad_files_are_refused() {
        for bad in [
            "[]",
            "{\"a\": 1}",
            "{\"a\": [1]}",
            "{\"a\": [1, -2]}",
            "{\"a\": [1, \"x\"]}",
            "{\"\": [1, 2]}",
            "not json",
            "{\"a\": [1, 100000000001]}",
        ] {
            assert!(Prices::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn huge_token_counts_do_not_overflow() {
        let p = Prices::parse(r#"{"m": [100000000000, 100000000000]}"#).unwrap();
        assert_eq!(
            p.cost(
                "m",
                Usage {
                    input_tokens: u64::MAX,
                    output_tokens: u64::MAX
                }
            ),
            Some(u64::MAX)
        );
    }
}
