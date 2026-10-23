//! `.mailmap` (gitmailmap(5)): the identities `%aN %aE %aL %cN %cE %cL %gN %gE` print.

#[derive(Debug)]
struct Rule {
    old_name: Option<Vec<u8>>,
    old_email: Vec<u8>,
    new_name: Option<Vec<u8>>,
    new_email: Option<Vec<u8>>,
}

/// The parsed rules of a mailmap file.
#[derive(Debug, Default)]
pub struct Mailmap {
    rules: Vec<Rule>,
}

/// `name <email>` at the start of `text`: the name (empty when absent), the email, the rest.
fn take_ident(text: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
    let open = text.iter().position(|&b| b == b'<')?;
    let close = open + text[open..].iter().position(|&b| b == b'>')?;
    Some((
        text[..open].trim_ascii(),
        &text[open + 1..close],
        &text[close + 1..],
    ))
}

impl Mailmap {
    /// Parses mailmap `text`; unreadable lines are skipped like git does.
    pub fn parse(text: &[u8]) -> Self {
        let mut rules = Vec::new();
        for line in text.split(|&b| b == b'\n') {
            let line = line.trim_ascii();
            if line.is_empty() || line[0] == b'#' {
                continue;
            }
            let Some((name1, email1, rest)) = take_ident(line) else {
                continue;
            };
            let rule = match take_ident(rest.trim_ascii()) {
                Some((name2, email2, _)) => Rule {
                    old_name: (!name2.is_empty()).then(|| name2.to_vec()),
                    old_email: email2.to_vec(),
                    new_name: (!name1.is_empty()).then(|| name1.to_vec()),
                    new_email: (!email1.is_empty()).then(|| email1.to_vec()),
                },
                None => Rule {
                    old_name: None,
                    old_email: email1.to_vec(),
                    new_name: (!name1.is_empty()).then(|| name1.to_vec()),
                    new_email: None,
                },
            };
            rules.push(rule);
        }
        Self { rules }
    }

    /// The mapped `(name, email)`: a rule that names the commit's name wins over one that only
    /// names its email; later rules win over earlier ones.
    pub fn lookup(&self, name: &[u8], email: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let hit = |r: &&Rule| r.old_email.eq_ignore_ascii_case(email);
        let by_name = self.rules.iter().rev().filter(hit).find(|r| {
            r.old_name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        });
        let rule = by_name.or_else(|| {
            self.rules
                .iter()
                .rev()
                .filter(hit)
                .find(|r| r.old_name.is_none())
        });
        match rule {
            Some(r) => (
                r.new_name.clone().unwrap_or_else(|| name.to_vec()),
                r.new_email.clone().unwrap_or_else(|| email.to_vec()),
            ),
            None => (name.to_vec(), email.to_vec()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_by_email_and_name() {
        let m = Mailmap::parse(
            b"# comment\nProper <p@x> <old@x>\n<new@x> <typo@x>\nJo <jo@x> Jon <jon@x>\n",
        );
        assert_eq!(
            m.lookup(b"Whoever", b"OLD@x"),
            (b"Proper".to_vec(), b"p@x".to_vec())
        );
        assert_eq!(
            m.lookup(b"A", b"typo@x"),
            (b"A".to_vec(), b"new@x".to_vec())
        );
        assert_eq!(
            m.lookup(b"Jon", b"jon@x"),
            (b"Jo".to_vec(), b"jo@x".to_vec())
        );
        assert_eq!(
            m.lookup(b"Ann", b"jon@x"),
            (b"Ann".to_vec(), b"jon@x".to_vec())
        );
    }
}
