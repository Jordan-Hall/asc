//! Back-references for `grep` (`\1` to `\9` in basic and extended expressions), which the
//! linear-time `regex` crate cannot run. A pattern with one is parsed here and run by a
//! backtracking matcher with a step budget per line. The one-character pieces (literals, `.`,
//! bracket expressions, `\w` and the like) are still matched by `regex`, so classes and case
//! folding are the same as for every other pattern.

use regex::bytes::{Regex, RegexBuilder};

/// Steps one line may take; past it the file is given up with a warning.
const MAX_STEPS: u64 = 1 << 24;
/// The largest repetition count, POSIX's `RE_DUP_MAX`.
const DUP_MAX: u32 = 255;
/// The most instructions a program may have once intervals are expanded.
const MAX_PROGRAM: usize = 1 << 16;
/// The size limit of each one-character `regex`.
const MAX_ATOM_BYTES: usize = 1 << 20;

/// A parsed expression.
#[derive(Clone, Debug)]
enum Node {
    /// One character, in `regex` syntax.
    Atom(String),
    Assert(Assertion),
    /// A group and its number (1-based, counted over every pattern).
    Group(usize, Box<Node>),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat(Box<Node>, u32, Option<u32>),
    Backref(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Assertion {
    Start,
    End,
    WordStart,
    WordEnd,
    Boundary,
    NotBoundary,
    /// No word byte before (the left edge of a `-w` match).
    NoWordBefore,
    /// No word byte after (the right edge of a `-w` match).
    NoWordAfter,
}

#[derive(Clone, Copy, Debug)]
enum Inst {
    Atom(usize),
    /// Try the first target, then the second.
    Split(usize, usize),
    Jmp(usize),
    /// Records the position in a slot (captures and loop marks).
    Save(usize),
    /// Leaves the loop (to the second index) when the position equals the slot: an
    /// iteration that matched nothing is kept once, as POSIX matchers do, but not repeated.
    Progress(usize, usize),
    Assert(Assertion),
    Backref(usize),
    Match,
}

/// The patterns of one `grep`, compiled for the backtracking matcher.
#[derive(Debug)]
pub struct Backref {
    prog: Vec<Inst>,
    atoms: Vec<Regex>,
    slots: usize,
    ignore_case: bool,
    anchored: bool,
}

/// Turns the POSIX bracket expression at an index (`[`) into a `regex` class, returning the
/// index after its `]`.
pub type Bracket = dyn Fn(&[char], usize, &mut String) -> Result<usize, String>;

/// One pattern to compile.
pub struct Source<'a> {
    pub text: &'a str,
    pub fixed: bool,
}

/// Why a pattern does not compile, with the pattern.
#[derive(Debug)]
pub struct Invalid {
    pub pattern: String,
    pub why: String,
}

struct Parser<'a> {
    chars: Vec<char>,
    i: usize,
    extended: bool,
    /// Groups opened before this pattern.
    base: usize,
    /// Groups opened in this pattern so far.
    opened: usize,
    bracket: &'a Bracket,
}

fn literal(c: char) -> Node {
    let mut tmp = [0u8; 4];
    Node::Atom(regex::escape(c.encode_utf8(&mut tmp)))
}

impl Parser<'_> {
    fn peek(&self, k: usize) -> Option<char> {
        self.chars.get(self.i + k).copied()
    }

    /// Whether the input is at `|` (or `\|`), `)` (or `\)`) or its end.
    fn at_branch_end(&self) -> bool {
        match (self.peek(0), self.peek(1)) {
            (None, _) => true,
            (Some('|' | ')'), _) if self.extended => true,
            (Some('\\'), Some('|' | ')')) if !self.extended => true,
            _ => false,
        }
    }

    fn alt(&mut self) -> Result<Node, String> {
        let mut branches = vec![self.concat()?];
        loop {
            match (self.peek(0), self.peek(1)) {
                (Some('|'), _) if self.extended => self.i += 1,
                (Some('\\'), Some('|')) if !self.extended => self.i += 2,
                _ => break,
            }
            branches.push(self.concat()?);
        }
        Ok(if branches.len() == 1 {
            branches.pop().unwrap_or(Node::Concat(Vec::new()))
        } else {
            Node::Alt(branches)
        })
    }

    fn concat(&mut self) -> Result<Node, String> {
        let mut items: Vec<Node> = Vec::new();
        let mut at_start = true;
        while !self.at_branch_end() {
            let atom = self.atom(at_start)?;
            at_start = false;
            let atom = self.postfix(atom)?;
            items.push(atom);
        }
        Ok(Node::Concat(items))
    }

    /// The repetitions after an atom.
    fn postfix(&mut self, mut node: Node) -> Result<Node, String> {
        loop {
            let (min, max, len) = match (self.peek(0), self.peek(1)) {
                (Some('*'), _) => (0, None, 1),
                (Some('+'), _) if self.extended => (1, None, 1),
                (Some('?'), _) if self.extended => (0, Some(1), 1),
                (Some('\\'), Some('+')) if !self.extended => (1, None, 2),
                (Some('\\'), Some('?')) if !self.extended => (0, Some(1), 2),
                (Some('{'), _) if self.extended => {
                    self.i += 1;
                    let (min, max) = self.interval()?;
                    node = Node::Repeat(Box::new(node), min, max);
                    continue;
                }
                (Some('\\'), Some('{')) if !self.extended => {
                    self.i += 2;
                    let (min, max) = self.interval()?;
                    node = Node::Repeat(Box::new(node), min, max);
                    continue;
                }
                _ => return Ok(node),
            };
            self.i += len;
            node = Node::Repeat(Box::new(node), min, max);
        }
    }

    fn number(&mut self) -> Option<u32> {
        let start = self.i;
        while self.peek(0).is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        let text: String = self.chars[start..self.i].iter().collect();
        text.parse().ok()
    }

    /// `m}`, `m,}` or `m,n}` (with `\}` in a basic expression), after the opening brace.
    fn interval(&mut self) -> Result<(u32, Option<u32>), String> {
        let bad = || "Invalid content of \\{\\}".to_owned();
        let min = self.number().ok_or_else(bad)?;
        let max = if self.peek(0) == Some(',') {
            self.i += 1;
            self.number()
        } else {
            Some(min)
        };
        let close = if self.extended {
            self.peek(0) == Some('}')
        } else {
            self.peek(0) == Some('\\') && self.peek(1) == Some('}')
        };
        if !close {
            return Err("Unmatched \\{".to_owned());
        }
        self.i += if self.extended { 1 } else { 2 };
        if max.is_some_and(|m| m < min) || min > DUP_MAX || max.is_some_and(|m| m > DUP_MAX) {
            return Err(bad());
        }
        Ok((min, max))
    }

    fn group(&mut self) -> Result<Node, String> {
        self.opened += 1;
        let n = self.base + self.opened;
        let inner = self.alt()?;
        let closed = if self.extended {
            self.peek(0) == Some(')')
        } else {
            self.peek(0) == Some('\\') && self.peek(1) == Some(')')
        };
        if !closed {
            return Err("Unmatched ( or \\(".to_owned());
        }
        self.i += if self.extended { 1 } else { 2 };
        Ok(Node::Group(n, Box::new(inner)))
    }

    fn atom(&mut self, at_start: bool) -> Result<Node, String> {
        let Some(c) = self.peek(0) else {
            return Err("Premature end of regular expression".to_owned());
        };
        self.i += 1;
        Ok(match c {
            '\\' => {
                let Some(e) = self.peek(0) else {
                    return Err("Trailing backslash".to_owned());
                };
                self.i += 1;
                match e {
                    '(' if !self.extended => self.group()?,
                    ')' if !self.extended => return Err("Unmatched ) or \\)".to_owned()),
                    '{' if !self.extended => {
                        if at_start {
                            literal('{')
                        } else {
                            return Err("Invalid preceding regular expression".to_owned());
                        }
                    }
                    '<' => Node::Assert(Assertion::WordStart),
                    '>' => Node::Assert(Assertion::WordEnd),
                    'b' => Node::Assert(Assertion::Boundary),
                    'B' => Node::Assert(Assertion::NotBoundary),
                    'w' | 'W' | 's' | 'S' => Node::Atom(format!("\\{e}")),
                    '1'..='9' => {
                        let n = e as usize - '0' as usize;
                        if n > self.opened {
                            return Err("Invalid back reference".to_owned());
                        }
                        Node::Backref(self.base + n)
                    }
                    other => literal(other),
                }
            }
            '[' => {
                let mut class = String::new();
                self.i = (self.bracket)(&self.chars, self.i - 1, &mut class)?;
                Node::Atom(class)
            }
            '(' if self.extended => self.group()?,
            ')' if self.extended => return Err("Unmatched ) or \\)".to_owned()),
            '*' if at_start && !self.extended => literal('*'),
            '*' | '+' | '?' | '{' if self.extended && at_start => {
                return Err("Invalid preceding regular expression".to_owned());
            }
            '^' if self.extended || at_start => Node::Assert(Assertion::Start),
            '$' if self.extended || self.at_branch_end() => Node::Assert(Assertion::End),
            '.' => Node::Atom(".".to_owned()),
            c => literal(c),
        })
    }
}

/// Whether `pattern` uses a back-reference outside a bracket expression.
pub fn uses_backref(pattern: &str) -> bool {
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                if chars.get(i + 1).is_some_and(|c| ('1'..='9').contains(c)) {
                    return true;
                }
                i += 2;
            }
            '[' => {
                // Skip to the closing bracket: `]` first (or after `^`) is a member.
                i += 1;
                if chars.get(i) == Some(&'^') {
                    i += 1;
                }
                if chars.get(i) == Some(&']') {
                    i += 1;
                }
                while i < chars.len() && chars[i] != ']' {
                    if chars[i] == '[' && matches!(chars.get(i + 1), Some(':' | '=' | '.')) {
                        let kind = chars[i + 1];
                        i += 2;
                        while i + 1 < chars.len() && !(chars[i] == kind && chars[i + 1] == ']') {
                            i += 1;
                        }
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    false
}

struct Compiler {
    prog: Vec<Inst>,
    atoms: Vec<String>,
    marks: usize,
    groups: usize,
}

impl Compiler {
    fn push(&mut self, inst: Inst) -> Result<usize, String> {
        if self.prog.len() >= MAX_PROGRAM {
            return Err("Regular expression too big".to_owned());
        }
        self.prog.push(inst);
        Ok(self.prog.len() - 1)
    }

    fn node(&mut self, n: &Node) -> Result<(), String> {
        match n {
            Node::Atom(text) => {
                let idx = match self.atoms.iter().position(|a| a == text) {
                    Some(i) => i,
                    None => {
                        self.atoms.push(text.clone());
                        self.atoms.len() - 1
                    }
                };
                self.push(Inst::Atom(idx))?;
            }
            Node::Assert(a) => {
                self.push(Inst::Assert(*a))?;
            }
            Node::Backref(g) => {
                self.push(Inst::Backref(*g))?;
            }
            Node::Group(g, inner) => {
                self.groups = self.groups.max(*g);
                self.push(Inst::Save(2 * g))?;
                self.node(inner)?;
                self.push(Inst::Save(2 * g + 1))?;
            }
            Node::Concat(items) => {
                for i in items {
                    self.node(i)?;
                }
            }
            Node::Alt(branches) => {
                let mut ends = Vec::new();
                for (k, b) in branches.iter().enumerate() {
                    if k + 1 == branches.len() {
                        self.node(b)?;
                    } else {
                        let split = self.push(Inst::Split(0, 0))?;
                        self.node(b)?;
                        ends.push(self.push(Inst::Jmp(0))?);
                        let next = self.prog.len();
                        self.prog[split] = Inst::Split(split + 1, next);
                    }
                }
                let end = self.prog.len();
                for e in ends {
                    self.prog[e] = Inst::Jmp(end);
                }
            }
            Node::Repeat(inner, min, max) => {
                for _ in 0..*min {
                    self.node(inner)?;
                }
                match max {
                    None => {
                        let mark = self.marks;
                        self.marks += 1;
                        let split = self.push(Inst::Split(0, 0))?;
                        self.push(Inst::Save(usize::MAX - mark))?;
                        self.node(inner)?;
                        let progress = self.push(Inst::Progress(usize::MAX - mark, 0))?;
                        self.push(Inst::Jmp(split))?;
                        let out = self.prog.len();
                        self.prog[split] = Inst::Split(split + 1, out);
                        self.prog[progress] = Inst::Progress(usize::MAX - mark, out);
                    }
                    Some(max) => {
                        let mut splits = Vec::new();
                        for _ in *min..*max {
                            splits.push(self.push(Inst::Split(0, 0))?);
                            self.node(inner)?;
                        }
                        let out = self.prog.len();
                        for s in splits {
                            self.prog[s] = Inst::Split(s + 1, out);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Compiles `patterns` (any one matching selects a line). `bracket` turns a POSIX bracket
/// expression into a `regex` class, as for the other patterns.
pub fn compile(
    patterns: &[Source<'_>],
    extended: bool,
    ignore_case: bool,
    word: bool,
    bracket: &Bracket,
) -> Result<Backref, Invalid> {
    let mut branches = Vec::new();
    let mut base = 0;
    for p in patterns {
        let node = if p.fixed {
            Node::Concat(p.text.chars().map(literal).collect())
        } else {
            let mut parser = Parser {
                chars: p.text.chars().collect(),
                i: 0,
                extended,
                base,
                opened: 0,
                bracket,
            };
            let node = parser.alt().map_err(|why| Invalid {
                pattern: p.text.to_owned(),
                why,
            })?;
            if parser.i < parser.chars.len() {
                return Err(Invalid {
                    pattern: p.text.to_owned(),
                    why: "Unmatched ) or \\)".to_owned(),
                });
            }
            base += parser.opened;
            node
        };
        branches.push(node);
    }
    let anchored = branches.iter().all(starts_anchored);
    let mut body = Node::Alt(branches);
    if word {
        body = Node::Concat(vec![
            Node::Assert(Assertion::NoWordBefore),
            body,
            Node::Assert(Assertion::NoWordAfter),
        ]);
    }
    let mut c = Compiler {
        prog: Vec::new(),
        atoms: Vec::new(),
        marks: 0,
        groups: base,
    };
    let fail = |why: String| Invalid {
        pattern: patterns
            .first()
            .map_or_else(String::new, |p| p.text.to_owned()),
        why,
    };
    c.node(&body).map_err(fail)?;
    c.push(Inst::Match).map_err(fail)?;
    let mut atoms = Vec::with_capacity(c.atoms.len());
    for a in &c.atoms {
        let re = RegexBuilder::new(&format!("^(?:{a})"))
            .case_insensitive(ignore_case)
            .size_limit(MAX_ATOM_BYTES)
            .build()
            .map_err(|e| fail(format!("Invalid regular expression ({e})")))?;
        atoms.push(re);
    }
    // Capture slots come first; loop marks are addressed from the top and remapped below.
    let slots = 2 * (c.groups + 1);
    let prog = c
        .prog
        .into_iter()
        .map(|i| match i {
            Inst::Save(s) if s > usize::MAX / 2 => Inst::Save(slots + (usize::MAX - s)),
            Inst::Progress(s, out) => Inst::Progress(slots + (usize::MAX - s), out),
            other => other,
        })
        .collect();
    Ok(Backref {
        prog,
        atoms,
        slots: slots + c.marks,
        ignore_case,
        anchored,
    })
}

/// Whether every match of `n` must start at the start of the line.
fn starts_anchored(n: &Node) -> bool {
    match n {
        Node::Assert(Assertion::Start) => true,
        Node::Concat(items) => items.first().is_some_and(starts_anchored),
        Node::Group(_, inner) => starts_anchored(inner),
        Node::Alt(bs) => !bs.is_empty() && bs.iter().all(starts_anchored),
        _ => false,
    }
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

fn is_ascii_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

enum Frame {
    Try(usize, usize),
    Restore(usize, usize),
}

const UNSET: usize = usize::MAX;

impl Backref {
    fn assertion(a: Assertion, line: &[u8], pos: usize) -> bool {
        let before = pos > 0 && is_word(line[pos - 1]);
        let after = pos < line.len() && is_word(line[pos]);
        match a {
            Assertion::Start => pos == 0,
            Assertion::End => pos == line.len(),
            Assertion::WordStart => !before && after,
            Assertion::WordEnd => before && !after,
            Assertion::Boundary => before != after,
            Assertion::NotBoundary => before == after,
            Assertion::NoWordBefore => pos == 0 || !is_ascii_word(line[pos - 1]),
            Assertion::NoWordAfter => pos == line.len() || !is_ascii_word(line[pos]),
        }
    }

    fn same(&self, a: &[u8], b: &[u8]) -> bool {
        if !self.ignore_case {
            return a == b;
        }
        if a.is_ascii() && b.is_ascii() {
            return a.eq_ignore_ascii_case(b);
        }
        String::from_utf8_lossy(a).to_lowercase() == String::from_utf8_lossy(b).to_lowercase()
    }

    /// Runs the program from `start`; `Err` when the budget runs out.
    fn run(&self, line: &[u8], start: usize, steps: &mut u64) -> Result<bool, ()> {
        let mut slots = vec![UNSET; self.slots];
        let mut stack = vec![Frame::Try(0, start)];
        while let Some(frame) = stack.pop() {
            let (mut pc, mut pos) = match frame {
                Frame::Restore(s, old) => {
                    slots[s] = old;
                    continue;
                }
                Frame::Try(pc, pos) => (pc, pos),
            };
            loop {
                *steps += 1;
                if *steps > MAX_STEPS {
                    return Err(());
                }
                match self.prog[pc] {
                    Inst::Atom(a) => match self.atoms[a].find(&line[pos..]) {
                        Some(m) if m.start() == 0 && m.end() > 0 => {
                            pos += m.end();
                            pc += 1;
                        }
                        _ => break,
                    },
                    Inst::Split(x, y) => {
                        stack.push(Frame::Try(y, pos));
                        pc = x;
                    }
                    Inst::Jmp(x) => pc = x,
                    Inst::Save(s) => {
                        stack.push(Frame::Restore(s, slots[s]));
                        slots[s] = pos;
                        pc += 1;
                    }
                    Inst::Progress(s, out) => {
                        pc = if slots[s] == pos { out } else { pc + 1 };
                    }
                    Inst::Assert(a) => {
                        if !Self::assertion(a, line, pos) {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::Backref(g) => {
                        let (s, e) = (slots[2 * g], slots[2 * g + 1]);
                        if s == UNSET || e == UNSET || e < s {
                            break;
                        }
                        let len = e - s;
                        if pos + len > line.len() || !self.same(&line[s..e], &line[pos..pos + len])
                        {
                            break;
                        }
                        pos += len;
                        pc += 1;
                    }
                    Inst::Match => return Ok(true),
                }
            }
        }
        Ok(false)
    }

    /// Whether some match is in `line`; `None` when the step budget ran out first.
    pub fn is_match(&self, line: &[u8]) -> Option<bool> {
        let mut steps = 0;
        let last = if self.anchored { 0 } else { line.len() };
        for start in 0..=last {
            match self.run(line, start, &mut steps) {
                Ok(true) => return Some(true),
                Ok(false) => {}
                Err(()) => return None,
            }
        }
        Some(false)
    }
}
