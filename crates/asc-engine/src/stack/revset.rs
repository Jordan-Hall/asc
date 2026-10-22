//! The `-r/--revset` subset (SPEC §20.3).
//!
//! Functions: `stack()`, `draft()`, `trunk()`, `mine()`, `mutable()` and `git(<spec>)`.
//! Operators: `|` (union), `&` (intersection) and `~` (difference); `&` and `~` bind tighter
//! than `|` and associate left; parentheses group. Input is untrusted: length, nesting and
//! result size are bounded.

use std::collections::BTreeSet;

use asc_core::id::ChangesetId;
use futures::future::BoxFuture;

use super::{Error, MAX_SET, Result, StackEnv, Stacks};
use crate::change::ChangeRows;
use crate::graph::{GraphSource, JumpIndex};

/// Longest revset text accepted.
pub const MAX_LEN: usize = 4096;

/// Deepest nesting of parentheses and operators accepted.
pub const MAX_DEPTH: usize = 32;

/// A parsed revset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Revset {
    /// The stack of the current `HEAD`.
    Stack,
    /// Every draft on the stack of any head.
    Draft,
    /// The trunk tips.
    Trunk,
    /// Drafts whose author email is the configured user's.
    Mine,
    /// Non-immutable changesets reachable from the heads.
    Mutable,
    /// A git revision spec, resolved by the environment.
    Git(String),
    Union(Box<Revset>, Box<Revset>),
    Intersect(Box<Revset>, Box<Revset>),
    Diff(Box<Revset>, Box<Revset>),
}

fn err(msg: impl Into<String>) -> Error {
    Error::Revset(msg.into())
}

/// Parses a revset.
pub fn parse(text: &str) -> Result<Revset> {
    if text.len() > MAX_LEN {
        return Err(err(format!("longer than {MAX_LEN} bytes")));
    }
    let mut p = Parser {
        s: text.as_bytes(),
        at: 0,
        text,
    };
    let r = p.union(0)?;
    p.skip_ws();
    if p.at != p.s.len() {
        return Err(err(format!("unexpected `{}` at {}", p.rest_char(), p.at)));
    }
    Ok(r)
}

struct Parser<'a> {
    s: &'a [u8],
    at: usize,
    text: &'a str,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while self.s.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.s.get(self.at).copied()
    }

    fn rest_char(&self) -> char {
        self.text[self.at..].chars().next().unwrap_or('?')
    }

    fn union(&mut self, depth: usize) -> Result<Revset> {
        let mut l = self.binary(depth)?;
        while self.peek() == Some(b'|') {
            self.at += 1;
            let r = self.binary(depth)?;
            l = Revset::Union(Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn binary(&mut self, depth: usize) -> Result<Revset> {
        let mut l = self.primary(depth)?;
        loop {
            let op = self.peek();
            if !matches!(op, Some(b'&' | b'~')) {
                return Ok(l);
            }
            self.at += 1;
            let r = self.primary(depth)?;
            l = if op == Some(b'&') {
                Revset::Intersect(Box::new(l), Box::new(r))
            } else {
                Revset::Diff(Box::new(l), Box::new(r))
            };
        }
    }

    fn primary(&mut self, depth: usize) -> Result<Revset> {
        if depth >= MAX_DEPTH {
            return Err(err(format!("nested deeper than {MAX_DEPTH}")));
        }
        match self.peek() {
            None => Err(err("unexpected end")),
            Some(b'(') => {
                self.at += 1;
                let r = self.union(depth + 1)?;
                self.expect(b')')?;
                Ok(r)
            }
            Some(c) if c.is_ascii_alphabetic() => self.call(),
            Some(_) => Err(err(format!(
                "unexpected `{}` at {}",
                self.rest_char(),
                self.at
            ))),
        }
    }

    fn expect(&mut self, c: u8) -> Result<()> {
        if self.peek() == Some(c) {
            self.at += 1;
            Ok(())
        } else {
            Err(err(format!("expected `{}` at {}", c as char, self.at)))
        }
    }

    fn call(&mut self) -> Result<Revset> {
        let start = self.at;
        while self.s.get(self.at).is_some_and(u8::is_ascii_alphanumeric) {
            self.at += 1;
        }
        let name = &self.text[start..self.at];
        self.expect(b'(')?;
        if name == "git" {
            return self.git_arg();
        }
        self.expect(b')')?;
        match name {
            "stack" => Ok(Revset::Stack),
            "draft" => Ok(Revset::Draft),
            "trunk" => Ok(Revset::Trunk),
            "mine" => Ok(Revset::Mine),
            "mutable" => Ok(Revset::Mutable),
            _ => Err(err(format!("unknown function `{name}`"))),
        }
    }

    /// The raw text up to the matching `)`; parentheses inside must balance.
    fn git_arg(&mut self) -> Result<Revset> {
        let start = self.at;
        let mut open = 0usize;
        while let Some(&c) = self.s.get(self.at) {
            match c {
                b'(' => open += 1,
                b')' if open == 0 => {
                    let spec = self.text[start..self.at].trim();
                    self.at += 1;
                    if spec.is_empty() || spec.contains('\0') {
                        return Err(err("git() needs a revision spec"));
                    }
                    return Ok(Revset::Git(spec.to_string()));
                }
                b')' => open -= 1,
                _ => {}
            }
            self.at += 1;
        }
        Err(err("unterminated git("))
    }
}

/// Evaluates `r` to a set of changesets.
pub async fn eval<S, J, C, E>(
    stacks: &Stacks<'_, S, J, C, E>,
    r: &Revset,
) -> Result<BTreeSet<ChangesetId>>
where
    S: GraphSource,
    J: JumpIndex,
    C: ChangeRows,
    E: StackEnv,
{
    eval_in(stacks, r).await
}

/// Parses and evaluates `text`.
pub async fn eval_str<S, J, C, E>(
    stacks: &Stacks<'_, S, J, C, E>,
    text: &str,
) -> Result<BTreeSet<ChangesetId>>
where
    S: GraphSource,
    J: JumpIndex,
    C: ChangeRows,
    E: StackEnv,
{
    eval(stacks, &parse(text)?).await
}

fn bounded(set: BTreeSet<ChangesetId>) -> Result<BTreeSet<ChangesetId>> {
    if set.len() > MAX_SET {
        Err(Error::TooLarge("revset result", MAX_SET))
    } else {
        Ok(set)
    }
}

fn eval_in<'a, S, J, C, E>(
    st: &'a Stacks<'_, S, J, C, E>,
    r: &'a Revset,
) -> BoxFuture<'a, Result<BTreeSet<ChangesetId>>>
where
    S: GraphSource,
    J: JumpIndex,
    C: ChangeRows,
    E: StackEnv,
{
    Box::pin(async move {
        match r {
            Revset::Stack => match st.current_head() {
                Some(h) => Ok(st.stack_of(h).await?.changes.into_iter().collect()),
                None => Ok(BTreeSet::new()),
            },
            Revset::Draft => st.draft_set().await,
            Revset::Trunk => bounded(st.trunk_tips().iter().copied().collect()),
            Revset::Mutable => st.mutable_set().await,
            Revset::Mine => {
                let Some(me) = st.env().user_email() else {
                    return Ok(BTreeSet::new());
                };
                let mut out = BTreeSet::new();
                for cs in st.draft_set().await? {
                    if st.env().author_email(cs).await? == me {
                        out.insert(cs);
                    }
                }
                Ok(out)
            }
            Revset::Git(spec) => {
                let ids = st.env().resolve_git(spec).await?;
                bounded(ids.into_iter().collect())
            }
            Revset::Union(a, b) => {
                let mut l = eval_in(st, a).await?;
                l.extend(eval_in(st, b).await?);
                bounded(l)
            }
            Revset::Intersect(a, b) => {
                let l = eval_in(st, a).await?;
                let r = eval_in(st, b).await?;
                Ok(l.intersection(&r).copied().collect())
            }
            Revset::Diff(a, b) => {
                let l = eval_in(st, a).await?;
                let r = eval_in(st, b).await?;
                Ok(l.difference(&r).copied().collect())
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::testutil::Fx;

    fn u(a: Revset, b: Revset) -> Revset {
        Revset::Union(Box::new(a), Box::new(b))
    }

    #[test]
    fn parses_precedence_and_grouping() {
        use Revset::{Diff, Draft, Intersect, Mutable, Stack, Trunk};
        assert_eq!(
            parse("stack() | draft() & trunk()").unwrap(),
            u(Stack, Intersect(Box::new(Draft), Box::new(Trunk)))
        );
        assert_eq!(
            parse("(stack() | draft()) ~ mutable()").unwrap(),
            Diff(Box::new(u(Stack, Draft)), Box::new(Mutable))
        );
        assert_eq!(
            parse("draft() ~ stack() & trunk()").unwrap(),
            Intersect(
                Box::new(Diff(Box::new(Draft), Box::new(Stack))),
                Box::new(Trunk)
            )
        );
    }

    #[test]
    fn git_takes_raw_spec_with_balanced_parens() {
        assert_eq!(parse("git(HEAD~2)").unwrap(), Revset::Git("HEAD~2".into()));
        assert_eq!(
            parse(" git( a..b^{(x)} ) ").unwrap(),
            Revset::Git("a..b^{(x)}".into())
        );
    }

    #[test]
    fn rejects_bad_input() {
        for bad in [
            "",
            "stack(",
            "stack",
            "stack() |",
            "| stack()",
            "nope()",
            "git()",
            "git(  )",
            "git(a",
            "stack() stack()",
            "stack(x)",
            "(stack()",
            "stack())",
            "~",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn bounds_length_and_depth() {
        let deep = format!(
            "{}stack(){}",
            "(".repeat(MAX_DEPTH + 1),
            ")".repeat(MAX_DEPTH + 1)
        );
        assert!(parse(&deep).is_err());
        let ok = format!(
            "{}stack(){}",
            "(".repeat(MAX_DEPTH - 1),
            ")".repeat(MAX_DEPTH - 1)
        );
        assert!(parse(&ok).is_ok());
        let long = vec!["stack()"; MAX_LEN].join("|");
        assert!(parse(&long).is_err());
    }

    /// 0 trunk, 1-2 drafts on it (head 2); 3 draft by someone else on 2 (head 3).
    fn fx() -> Fx {
        let mut fx = Fx::new_with(
            &[&[], &[0], &[1], &[2]],
            &[1, 2, 3],
            &[0],
            &[3],
            &[(3, b"other@x")],
        );
        fx.set_current(Some(2));
        fx.env.email = Some(b"me@x".to_vec());
        fx
    }

    async fn run(fx: &Fx, text: &str) -> Vec<usize> {
        let st = fx.stacks().await;
        let set = eval_str(&st, text).await.unwrap();
        fx.indexes(&set)
    }

    #[tokio::test]
    async fn evaluates_functions_and_operators() {
        let fx = fx();
        assert_eq!(run(&fx, "trunk()").await, vec![0]);
        assert_eq!(run(&fx, "stack()").await, vec![1, 2]);
        assert_eq!(run(&fx, "draft()").await, vec![1, 2, 3]);
        assert_eq!(run(&fx, "mutable()").await, vec![1, 2, 3]);
        assert_eq!(run(&fx, "mine()").await, vec![1, 2]);
        assert_eq!(run(&fx, "draft() ~ stack()").await, vec![3]);
        assert_eq!(run(&fx, "draft() & mine()").await, vec![1, 2]);
        assert_eq!(run(&fx, "trunk() | stack()").await, vec![0, 1, 2]);
        fx.env
            .git
            .lock()
            .unwrap()
            .insert("HEAD~2".into(), vec![fx.ids[1]]);
        assert_eq!(run(&fx, "git(HEAD~2)").await, vec![1]);
        assert_eq!(run(&fx, "git(HEAD~2) & stack()").await, vec![1]);
    }

    #[tokio::test]
    async fn mine_without_identity_is_empty() {
        let mut fx = fx();
        fx.env.email = None;
        assert!(run(&fx, "mine()").await.is_empty());
    }
}
