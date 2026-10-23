//! Expands a parsed format for one ref. `%(align)`, `%(if)`, `%(then)`, `%(else)` and `%(end)`
//! run on a stack of output buffers exactly as git's `ref_formatting_stack` does, including
//! which structure errors surface and when.

use super::Error;
use super::item::Item;
use super::parse::{Atom, IfCmp, Kind, Position, Token};
use super::quote::Quote;
use super::value::{Env, value};
use crate::cli::format::layout;

#[derive(Clone)]
struct IfState {
    cmp: IfCmp,
    then_seen: bool,
    else_seen: bool,
    satisfied: bool,
}

enum AtEnd {
    Align { width: usize, position: Position },
    If(IfState),
}

struct Layer {
    out: Vec<u8>,
    at_end: Option<AtEnd>,
}

fn fatal(text: &str) -> Error {
    Error::Fatal(text.to_owned())
}

fn pad(text: &[u8], width: usize, position: Position) -> Vec<u8> {
    let shown = layout::width(text);
    if shown >= width {
        return text.to_vec();
    }
    let gap = width - shown;
    let (left, right) = match position {
        Position::Left => (0, gap),
        Position::Middle => (gap / 2, gap - gap / 2),
        Position::Right => (gap, 0),
    };
    let mut out = vec![b' '; left];
    out.extend_from_slice(text);
    out.resize(out.len() + right, b' ');
    out
}

fn blank(text: &[u8]) -> bool {
    text.iter().all(u8::is_ascii_whitespace)
}

struct Stack {
    layers: Vec<Layer>,
    quote: Quote,
}

impl Stack {
    fn top(&mut self) -> &mut Layer {
        let last = self.layers.len() - 1;
        &mut self.layers[last]
    }

    fn open(&mut self, at_end: AtEnd) {
        self.layers.push(Layer {
            out: Vec::new(),
            at_end: Some(at_end),
        });
    }

    fn atom(&mut self, atom: &Atom, item: &Item, env: &Env) -> Result<(), Error> {
        match &atom.kind {
            Kind::Align { width, position } => self.open(AtEnd::Align {
                width: *width,
                position: *position,
            }),
            Kind::If(cmp) => self.open(AtEnd::If(IfState {
                cmp: cmp.clone(),
                then_seen: false,
                else_seen: false,
                satisfied: false,
            })),
            Kind::Then => self.then()?,
            Kind::Else => self.otherwise()?,
            Kind::End => self.end()?,
            _ => {
                let v = value(atom, item, env);
                if self.layers.len() == 1 {
                    self.quote.apply(&v.text, &mut self.top().out);
                } else {
                    self.top().out.extend_from_slice(&v.text);
                }
            }
        }
        Ok(())
    }

    fn then(&mut self) -> Result<(), Error> {
        let top = self.top();
        let Some(AtEnd::If(state)) = &mut top.at_end else {
            return Err(fatal("format: %(then) atom used without a %(if) atom"));
        };
        if state.then_seen {
            return Err(fatal("format: %(then) atom used more than once"));
        }
        if state.else_seen {
            return Err(fatal("format: %(then) atom used after %(else)"));
        }
        state.then_seen = true;
        state.satisfied = match &state.cmp {
            IfCmp::Equals(s) => s == &top.out,
            IfCmp::NotEquals(s) => s != &top.out,
            IfCmp::Nonempty => !top.out.is_empty() && !blank(&top.out),
        };
        top.out.clear();
        Ok(())
    }

    fn otherwise(&mut self) -> Result<(), Error> {
        let top = self.top();
        let Some(AtEnd::If(state)) = &mut top.at_end else {
            return Err(fatal("format: %(else) atom used without a %(if) atom"));
        };
        if !state.then_seen {
            return Err(fatal("format: %(else) atom used without a %(then) atom"));
        }
        if state.else_seen {
            return Err(fatal("format: %(else) atom used more than once"));
        }
        state.else_seen = true;
        let shared = state.clone();
        self.layers.push(Layer {
            out: Vec::new(),
            at_end: Some(AtEnd::If(shared)),
        });
        Ok(())
    }

    fn end(&mut self) -> Result<(), Error> {
        let Some(at_end) = self.top().at_end.take() else {
            return Err(fatal("format: %(end) atom used without corresponding atom"));
        };
        let finished = match at_end {
            AtEnd::Align { width, position } => {
                let layer = self.layers.pop().expect("an open layer");
                pad(&layer.out, width, position)
            }
            AtEnd::If(state) => {
                if !state.then_seen {
                    return Err(fatal("format: %(if) atom used without a %(then) atom"));
                }
                let layer = self.layers.pop().expect("an open layer");
                if state.else_seen {
                    // The `%(then)` text sits in the layer below the `%(else)` layer.
                    let then_layer = self.layers.pop().expect("the layer of the %(if)");
                    if state.satisfied {
                        then_layer.out
                    } else {
                        layer.out
                    }
                } else if state.satisfied {
                    layer.out
                } else {
                    Vec::new()
                }
            }
        };
        // Only the outermost opening atom quotes: its parent is the base layer.
        let mut text = Vec::new();
        if self.layers.len() == 1 {
            self.quote.apply(&finished, &mut text);
        } else {
            text = finished;
        }
        self.top().out.extend_from_slice(&text);
        Ok(())
    }
}

/// The text of `tokens` for `item`.
pub fn render(tokens: &[Token], item: &Item, env: &Env, quote: Quote) -> Result<Vec<u8>, Error> {
    let mut stack = Stack {
        layers: vec![Layer {
            out: Vec::new(),
            at_end: None,
        }],
        quote,
    };
    for token in tokens {
        match token {
            Token::Lit(text) => stack.top().out.extend_from_slice(text),
            Token::Atom(atom) => stack.atom(atom, item, env)?,
        }
    }
    if stack.layers.len() > 1 {
        return Err(fatal("format: %(end) atom missing"));
    }
    Ok(stack.layers.pop().map(|l| l.out).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ref_filter::parse::parse_format;

    fn run(format: &str, quote: Quote) -> Result<String, Error> {
        let mut item = Item::new("refs/heads/master");
        item.head = true;
        let env = Env {
            abbrev: 7,
            now: 0,
            color: false,
            mailmap: None,
        };
        let tokens = parse_format(format.as_bytes())?;
        render(&tokens, &item, &env, quote).map(|b| String::from_utf8(b).unwrap())
    }

    #[test]
    fn align_pads_and_if_chooses() {
        assert_eq!(
            run("%(align:20,right)%(refname)%(end)|", Quote::None).unwrap(),
            "   refs/heads/master|"
        );
        assert_eq!(
            run(
                "%(align:width=22,position=middle)%(refname)%(end)|",
                Quote::None
            )
            .unwrap(),
            "  refs/heads/master   |"
        );
        assert_eq!(
            run("%(if)%(HEAD)%(then)on%(else)off%(end)", Quote::None).unwrap(),
            "on"
        );
        assert_eq!(
            run(
                "%(if:equals=x)%(refname)%(then)a%(else)b%(end)",
                Quote::None
            )
            .unwrap(),
            "b"
        );
        assert_eq!(
            run("%(if:notequals=x)%(refname)%(then)a%(end)z", Quote::None).unwrap(),
            "az"
        );
    }

    #[test]
    fn quoting_applies_once_at_the_top() {
        assert_eq!(
            run("%(refname) lit", Quote::Shell).unwrap(),
            "'refs/heads/master' lit"
        );
        assert_eq!(
            run("%(align:3)%(HEAD)%(refname:lstrip=2)%(end)", Quote::Shell).unwrap(),
            "'*master'"
        );
    }

    #[test]
    fn structure_errors() {
        let msg = |f: &str| match run(f, Quote::None).unwrap_err() {
            Error::Fatal(m) | Error::Malformed(m) => m,
        };
        assert_eq!(
            msg("%(end)"),
            "format: %(end) atom used without corresponding atom"
        );
        assert_eq!(
            msg("%(then)"),
            "format: %(then) atom used without a %(if) atom"
        );
        assert_eq!(msg("%(if)x"), "format: %(end) atom missing");
        assert_eq!(
            msg("%(if)x%(then)%(then)"),
            "format: %(then) atom used more than once"
        );
        assert_eq!(
            msg("%(if)x%(end)"),
            "format: %(if) atom used without a %(then) atom"
        );
        assert_eq!(
            msg("%(if)x%(then)%(else)%(else)%(end)"),
            "format: %(else) atom used more than once"
        );
    }
}
