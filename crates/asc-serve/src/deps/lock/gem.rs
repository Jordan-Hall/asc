//! `Gemfile.lock`: who depends on a gem.

use super::Requester;
use crate::deps::rewrite::Pin;

/// The specs of the lock that list the pin among their dependencies, with the requirement written there.
pub fn requesters(text: &str, pin: &Pin<'_>) -> Result<Vec<Requester>, String> {
    let mut out = Vec::new();
    let mut spec: Option<(String, String)> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("      ") {
            if rest.starts_with(' ') {
                continue;
            }
            let name = rest.split(' ').next().unwrap_or("");
            if name == pin.name
                && let Some((n, v)) = &spec
            {
                let range = rest
                    .split_once('(')
                    .and_then(|(_, r)| r.strip_suffix(')'))
                    .unwrap_or(">= 0")
                    .to_owned();
                out.push(Requester {
                    name: n.clone(),
                    version: v.clone(),
                    range: Some(range),
                });
            }
        } else if let Some(rest) = line.strip_prefix("    ") {
            spec = rest
                .split_once(" (")
                .map(|(n, v)| (n.to_owned(), v.trim_end_matches(')').to_owned()));
        } else {
            spec = None;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::model::Ecosystem;

    #[test]
    fn dependents_come_with_the_requirement_the_lock_records() {
        let lock = "GEM\n  remote: https://rubygems.org/\n  specs:\n    actionpack (7.0.4)\n      rack (~> 2.0, >= 2.2.4)\n    rack (2.2.4)\n    rails (7.0.4)\n      actionpack (= 7.0.4)\n      rack\n\nPLATFORMS\n  ruby\n";
        let pin = Pin {
            eco: Ecosystem::RubyGems,
            name: "rack",
            from: "2.2.4",
            to: "2.2.5",
        };
        let r = requesters(lock, &pin).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].name, "actionpack");
        assert_eq!(r[0].range.as_deref(), Some("~> 2.0, >= 2.2.4"));
        assert_eq!(r[1].name, "rails");
        assert_eq!(r[1].range.as_deref(), Some(">= 0"));
    }
}
