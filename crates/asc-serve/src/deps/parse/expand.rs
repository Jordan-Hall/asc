//! Property expansion for `pom.xml` (`${name}`) and MSBuild files (`$(Name)`), bounded.
//!
//! A property can name other properties, so a small file can describe a huge string (a property that
//! names itself ten times, ten levels deep). Expansion is therefore capped three ways: no expanded
//! string is longer than [`MAX_EXPANDED`] bytes, a file spends at most [`MAX_SUBSTITUTIONS`]
//! substitutions in all, and a property that names itself, directly or through others, is unresolved. Properties may name properties only
//! [`MAX_DEPTH`] deep, so the recursion cannot exhaust the stack.

use std::collections::HashMap;

/// Longest string an expansion may produce, and the longest property value it will read.
pub const MAX_EXPANDED: usize = 4096;
/// Deepest chain of properties naming properties.
pub const MAX_DEPTH: usize = 16;
/// Substitutions one file may spend.
pub const MAX_SUBSTITUTIONS: usize = 10_000;

/// What is left to spend on one file.
pub struct Budget {
    left: usize,
}

impl Budget {
    pub fn new() -> Self {
        Self {
            left: MAX_SUBSTITUTIONS,
        }
    }
}

/// `s` with each `<open>name<close>` replaced by the expansion of that property, or `None` when a name
/// has no value, a property names itself, or a bound is passed.
pub fn expand(
    s: &str,
    props: &HashMap<String, String>,
    open: &str,
    close: char,
    budget: &mut Budget,
) -> Option<String> {
    go(s.trim(), props, open, close, budget, &mut Vec::new())
}

fn go<'a>(
    s: &'a str,
    props: &'a HashMap<String, String>,
    open: &str,
    close: char,
    budget: &mut Budget,
    stack: &mut Vec<&'a str>,
) -> Option<String> {
    if s.len() > MAX_EXPANDED {
        return None;
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find(open) {
        out.push_str(&rest[..start]);
        let after = &rest[start + open.len()..];
        let end = after.find(close)?;
        let name = &after[..end];
        if stack.contains(&name) || stack.len() >= MAX_DEPTH || budget.left == 0 {
            return None;
        }
        budget.left -= 1;
        let value = props.get(name)?;
        stack.push(name);
        let expanded = go(value, props, open, close, budget, stack);
        stack.pop();
        out.push_str(&expanded?);
        if out.len() > MAX_EXPANDED {
            return None;
        }
        rest = &after[end + close.len_utf8()..];
    }
    out.push_str(rest);
    (out.len() <= MAX_EXPANDED).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn properties_expand_through_each_other() {
        let p = props(&[("a", "1.${b}"), ("b", "2.${c}"), ("c", "3")]);
        let mut b = Budget::new();
        assert_eq!(
            expand(" x-${a} ", &p, "${", '}', &mut b).as_deref(),
            Some("x-1.2.3")
        );
        assert_eq!(
            expand("plain", &p, "${", '}', &mut b).as_deref(),
            Some("plain")
        );
        assert_eq!(expand("$(c)", &p, "$(", ')', &mut b).as_deref(), Some("3"));
        assert_eq!(expand("${missing}", &p, "${", '}', &mut b), None);
        assert_eq!(expand("${unclosed", &p, "${", '}', &mut b), None);
    }

    #[test]
    fn a_property_that_names_itself_is_unresolved() {
        let p = props(&[("a", "${a}"), ("x", "${y}"), ("y", "${x}")]);
        let mut b = Budget::new();
        assert_eq!(expand("${a}", &p, "${", '}', &mut b), None);
        assert_eq!(expand("${x}", &p, "${", '}', &mut b), None);
    }

    #[test]
    fn a_long_chain_of_properties_is_refused_not_followed() {
        let mut p: HashMap<String, String> = (0..10_000)
            .map(|i| (format!("p{i}"), format!("${{p{}}}", i + 1)))
            .collect();
        p.insert("p10000".into(), "end".into());
        assert_eq!(expand("${p0}", &p, "${", '}', &mut Budget::new()), None);
        // A chain within the depth bound resolves.
        let ok: HashMap<String, String> = (0..MAX_DEPTH - 1)
            .map(|i| (format!("p{i}"), format!("${{p{}}}", i + 1)))
            .chain([(format!("p{}", MAX_DEPTH - 1), "end".to_owned())])
            .collect();
        assert_eq!(
            expand("${p0}", &ok, "${", '}', &mut Budget::new()).as_deref(),
            Some("end")
        );
    }

    #[test]
    fn a_billion_laughs_property_stops_at_the_cap() {
        // Each level names the next ten times: 10^9 bytes if it were expanded.
        let mut pairs: Vec<(String, String)> = (0..9)
            .map(|i| (format!("l{i}"), format!("${{l{}}}", i + 1).repeat(10)))
            .collect();
        pairs.push(("l9".into(), "lol".repeat(10)));
        let p: HashMap<String, String> = pairs.into_iter().collect();
        let mut b = Budget::new();
        assert_eq!(expand("${l0}", &p, "${", '}', &mut b), None);
        // A file may not spend more than its budget however the references are spread.
        let p = props(&[("a", "x")]);
        let mut b = Budget::new();
        let many = "${a}".repeat(MAX_SUBSTITUTIONS / 2);
        assert_eq!(
            expand(&many, &p, "${", '}', &mut b),
            None,
            "output over the cap"
        );
        let mut b = Budget::new();
        for _ in 0..MAX_SUBSTITUTIONS {
            assert!(expand("${a}", &p, "${", '}', &mut b).is_some());
        }
        assert_eq!(expand("${a}", &p, "${", '}', &mut b), None, "budget spent");
        // A long literal value is refused whole.
        let p = props(&[("big", &"x".repeat(MAX_EXPANDED + 1))]);
        assert_eq!(expand("${big}", &p, "${", '}', &mut Budget::new()), None);
    }
}
