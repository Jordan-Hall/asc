//! The operation vocabulary of the differential fuzz (PLAN Q04, docs/git-fidelity.md "The
//! checks"): a seeded generator that builds sequences of shell steps over a small random tree, and
//! the script that runs them.
//!
//! An [`Op`] is a few lines of shell for the compat runner's `g` function (`g status` runs `git
//! status` through `PATH` and marks the command and its exit code on both streams), so the same
//! script runs against the pinned oracle and against the asc shim. The generator keeps a rough model
//! of the repository (which files, branches and tags probably exist) so that most operations are
//! meaningful; an operation that fails does so on both sides, and the exit code and the message are
//! compared like everything else.

use std::collections::BTreeSet;

/// A deterministic generator (SplitMix64): one seed, one sequence, on every platform.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..n`; `n` must be positive.
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    /// True with probability `pct` percent.
    pub fn chance(&mut self, pct: u32) -> bool {
        self.below(100) < pct as usize
    }

    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}

/// One step of a sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Op {
    /// The kind of step, for the summary (`commit`, `rebase-i`, `write`, ...).
    pub label: &'static str,
    /// The shell lines of the step, as the runner's scripts have them.
    pub sh: Vec<String>,
    /// True when the step changes the repository, so the state is read back after it.
    pub mutating: bool,
}

impl Op {
    fn new(label: &'static str, mutating: bool, sh: Vec<String>) -> Self {
        Self {
            label,
            sh,
            mutating,
        }
    }

    /// A step that runs one git command.
    fn git(label: &'static str, mutating: bool, args: &[String]) -> Self {
        Self::new(label, mutating, vec![g(args)])
    }
}

/// `'s'` as one shell word.
pub fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The runner's `g` call for `args`, each quoted.
pub fn g(args: &[String]) -> String {
    let words: Vec<String> = args.iter().map(|a| q(a)).collect();
    format!("g {}", words.join(" "))
}

fn s(x: &str) -> String {
    x.to_owned()
}

fn args(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|x| (*x).to_owned()).collect()
}

/// `bytes` as one `printf` format word: printable ASCII as is, everything else as an octal escape.
pub fn printf_word(bytes: &[u8]) -> String {
    let mut out = String::from("'");
    for &b in bytes {
        match b {
            b'\n' => out.push_str("\\n"),
            b'\\' => out.push_str("\\\\"),
            b'%' => out.push_str("%%"),
            b'\'' => out.push_str("\\047"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\{b:03o}")),
        }
    }
    out.push('\'');
    out
}

const TEXT_PATHS: &[&str] = &[
    "a.txt",
    "b.txt",
    "c.txt",
    "d/e.txt",
    "d/f.txt",
    "x/y/z.txt",
    "sp ace.txt",
    "\u{e9}.txt",
    "\u{65e5}\u{672c}/\u{8a9e}.txt",
    "Case.txt",
];
const BINARY_PATHS: &[&str] = &["bin.dat", "d/img.png"];
const CRLF_PATHS: &[&str] = &["crlf.txt", "d/win.txt"];
const BRANCHES: &[&str] = &["b1", "b2", "b3", "feat/x"];
const TAGS: &[&str] = &["t1", "t2", "v1.0"];
const WORDS: &[&str] = &[
    "alpha",
    "beta",
    "gamma",
    "d\u{e9}j\u{e0} vu",
    "\u{4e16}\u{754c}",
];

/// What a file's content looks like, by its name.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Crlf,
    Binary,
}

fn kind_of(path: &str) -> Kind {
    let base = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    if base.ends_with(".dat") || base.ends_with(".png") {
        Kind::Binary
    } else if base.starts_with("crlf") || base.starts_with("win") {
        Kind::Crlf
    } else {
        Kind::Text
    }
}

/// The directory part of `path`, if any.
fn parent(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(d, _)| d)
}

/// `path` with the case of its first cased letter flipped (a case-only rename), if it has one.
fn flip_case(path: &str) -> Option<String> {
    let start = path.rfind('/').map_or(0, |i| i + 1);
    let (head, base) = path.split_at(start);
    let mut done = false;
    let flipped: String = base
        .chars()
        .map(|c| {
            if !done && c.is_ascii_alphabetic() {
                done = true;
                if c.is_ascii_lowercase() {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            } else {
                c
            }
        })
        .collect();
    done.then(|| format!("{head}{flipped}"))
}

/// The generator's guess at what exists.
#[derive(Default)]
struct Model {
    files: BTreeSet<String>,
    branches: BTreeSet<String>,
    tags: BTreeSet<String>,
    commits: usize,
    stashes: usize,
    /// The branch HEAD is probably on (empty when detached).
    cur: String,
}

struct Gen {
    rng: Rng,
    m: Model,
    symlinks: bool,
    /// The number of the next message, so commit subjects differ.
    msg: usize,
}

/// The sequence of `seed`: a short bootstrap (two files and a commit, so most later steps have
/// something to act on) followed by random steps, at most `max_ops` in all. `symlinks` allows
/// symbolic links (not on Windows, where the shell has none).
pub fn generate(seed: u64, max_ops: usize, symlinks: bool) -> Vec<Op> {
    let mut st = Gen {
        rng: Rng::new(seed),
        m: Model::default(),
        symlinks,
        msg: 0,
    };
    let max_ops = max_ops.max(4);
    let len = 6 + st.rng.below(max_ops - 5);
    let mut ops = vec![
        st.write_at("a.txt"),
        st.write_at("b.txt"),
        Op::git("add", true, &args(&["add", "-A"])),
        st.commit_plain(),
    ];
    st.m.branches.insert(s("master"));
    st.m.cur = s("master");
    while ops.len() < len.min(max_ops) {
        ops.push(st.random_op());
    }
    ops
}

impl Gen {
    fn message(&mut self) -> String {
        self.msg += 1;
        format!("m{}", self.msg)
    }

    /// Content for `path`, derived from its name so that rewrites of one file mostly agree and
    /// merges are sometimes clean and sometimes conflicting.
    fn content(&mut self, path: &str) -> Vec<u8> {
        match kind_of(path) {
            Kind::Binary => {
                let n = 4 + self.rng.below(36);
                let mut v: Vec<u8> = (0..n).map(|_| self.rng.below(256) as u8).collect();
                v[0] = 0;
                v
            }
            kind => {
                let eol: &[u8] = if kind == Kind::Crlf { b"\r\n" } else { b"\n" };
                let mut out = Vec::new();
                let lines = 3 + self.rng.below(4);
                for i in 0..lines {
                    if self.rng.chance(25) {
                        let w = *self.rng.pick(WORDS);
                        out.extend_from_slice(format!("line {i} {w}").as_bytes());
                    } else {
                        out.extend_from_slice(format!("line {i}").as_bytes());
                    }
                    if i + 1 < lines || self.rng.chance(85) {
                        out.extend_from_slice(eol);
                    }
                }
                out
            }
        }
    }

    fn write_at(&mut self, path: &str) -> Op {
        let data = self.content(path);
        self.m.files.insert(path.to_owned());
        let mut sh = Vec::new();
        if let Some(d) = parent(path) {
            sh.push(format!("mkdir -p {}", q(d)));
        }
        sh.push(format!("printf {} > {}", printf_word(&data), q(path)));
        Op::new("write", false, sh)
    }

    fn new_path(&mut self) -> String {
        let r = self.rng.below(100);
        let pool = if r < 70 {
            TEXT_PATHS
        } else if r < 85 {
            BINARY_PATHS
        } else {
            CRLF_PATHS
        };
        (*self.rng.pick(pool)).to_owned()
    }

    /// An existing file most of the time, a pool name otherwise.
    fn path(&mut self) -> String {
        if !self.m.files.is_empty() && self.rng.chance(75) {
            let i = self.rng.below(self.m.files.len());
            self.m.files.iter().nth(i).cloned().unwrap_or_default()
        } else {
            self.new_path()
        }
    }

    fn existing_path(&mut self) -> String {
        if self.m.files.is_empty() {
            return self.new_path();
        }
        let i = self.rng.below(self.m.files.len());
        self.m.files.iter().nth(i).cloned().unwrap_or_default()
    }

    fn branch_name(&mut self) -> String {
        (*self.rng.pick(BRANCHES)).to_owned()
    }

    fn existing_branch(&mut self) -> String {
        if self.m.branches.is_empty() {
            return s("master");
        }
        let i = self.rng.below(self.m.branches.len());
        self.m.branches.iter().nth(i).cloned().unwrap_or_default()
    }

    /// A branch other than the current one, if there is one.
    fn other_branch(&mut self) -> String {
        let others: Vec<String> = self
            .m
            .branches
            .iter()
            .filter(|b| **b != self.m.cur)
            .cloned()
            .collect();
        if others.is_empty() {
            return self.existing_branch();
        }
        others[self.rng.below(others.len())].clone()
    }

    /// A revision: HEAD, an ancestor, a branch or a tag.
    fn rev(&mut self) -> String {
        let r = self.rng.below(100);
        if r < 25 {
            s("HEAD")
        } else if r < 50 {
            let max = self.m.commits.clamp(1, 3);
            format!("HEAD~{}", self.rng.below(max))
        } else if r < 85 || self.m.tags.is_empty() {
            let b = self.existing_branch();
            if self.rng.chance(25) {
                format!("{b}~1")
            } else {
                b
            }
        } else {
            let i = self.rng.below(self.m.tags.len());
            self.m.tags.iter().nth(i).cloned().unwrap_or_default()
        }
    }

    fn commit_plain(&mut self) -> Op {
        let m = self.message();
        self.m.commits += 1;
        Op::git("commit", true, &args(&["commit", "-m", &m]))
    }

    fn random_op(&mut self) -> Op {
        // (weight, step); the cumulative weights pick one.
        const KINDS: &[(usize, u8)] = &[
            (10, 0),  // write / edit
            (3, 1),   // delete
            (3, 2),   // rename (file system)
            (2, 3),   // chmod
            (2, 4),   // symlink
            (10, 5),  // add
            (3, 6),   // rm
            (4, 7),   // mv
            (4, 8),   // restore
            (4, 9),   // reset
            (12, 10), // commit
            (4, 11),  // branch
            (6, 12),  // switch
            (3, 13),  // checkout
            (3, 14),  // tag
            (6, 15),  // merge
            (4, 16),  // cherry-pick
            (3, 17),  // revert
            (4, 18),  // rebase
            (3, 19),  // rebase -i
            (5, 20),  // stash
            (3, 21),  // continue / abort / skip
            (2, 22),  // config
            (2, 23),  // ignore and attributes
            (12, 24), // read
        ];
        let total: usize = KINDS.iter().map(|k| k.0).sum();
        let mut roll = self.rng.below(total);
        let mut kind = 0;
        for (w, k) in KINDS {
            if roll < *w {
                kind = *k;
                break;
            }
            roll -= w;
        }
        match kind {
            0 => {
                let p = self.path();
                self.write_at(&p)
            }
            1 => {
                let p = self.existing_path();
                self.m.files.remove(&p);
                Op::new("delete", false, vec![format!("rm -f {}", q(&p))])
            }
            2 => self.fs_rename(),
            3 => {
                let p = self.existing_path();
                let mode = if self.rng.chance(70) { "+x" } else { "-x" };
                Op::new("chmod", false, vec![format!("chmod {mode} {}", q(&p))])
            }
            4 => self.symlink(),
            5 => self.add(),
            6 => self.rm(),
            7 => self.mv(),
            8 => self.restore(),
            9 => self.reset(),
            10 => self.commit(),
            11 => self.branch(),
            12 => self.switch(),
            13 => self.checkout(),
            14 => self.tag(),
            15 => self.merge(),
            16 => self.cherry_pick(),
            17 => self.revert(),
            18 => self.rebase(),
            19 => self.rebase_i(),
            20 => self.stash(),
            21 => self.sequencer(),
            22 => self.config(),
            23 => self.ignore_attrs(),
            _ => self.read(),
        }
    }

    fn fs_rename(&mut self) -> Op {
        let from = self.existing_path();
        let to = if self.rng.chance(50) {
            flip_case(&from).unwrap_or_else(|| self.new_path())
        } else {
            self.new_path()
        };
        self.m.files.remove(&from);
        self.m.files.insert(to.clone());
        let mut sh = Vec::new();
        if let Some(d) = parent(&to) {
            sh.push(format!("mkdir -p {}", q(d)));
        }
        sh.push(format!("mv -f {} {} 2>/dev/null", q(&from), q(&to)));
        Op::new("rename", false, sh)
    }

    fn symlink(&mut self) -> Op {
        if !self.symlinks {
            let p = self.path();
            return self.write_at(&p);
        }
        let target = if self.rng.chance(70) {
            self.existing_path()
        } else {
            s("nowhere")
        };
        let name = (*self.rng.pick(&["link", "d/link", "ln.txt"])).to_owned();
        self.m.files.insert(name.clone());
        let mut sh = Vec::new();
        if let Some(d) = parent(&name) {
            sh.push(format!("mkdir -p {}", q(d)));
        }
        sh.push(format!(
            "rm -f {n}; ln -s {} {n} 2>/dev/null",
            q(&target),
            n = q(&name)
        ));
        Op::new("symlink", false, sh)
    }

    fn add(&mut self) -> Op {
        let a = match self.rng.below(10) {
            0..=3 => args(&["add", "-A"]),
            4 => args(&["add", "."]),
            5 => args(&["add", "-u"]),
            6 => {
                let p = self.existing_path();
                vec![s("add"), s("-N"), s("--"), p]
            }
            7 => {
                let p = self.existing_path();
                vec![s("add"), s("-f"), s("--"), p]
            }
            _ => {
                let p = self.path();
                vec![s("add"), s("--"), p]
            }
        };
        Op::git("add", true, &a)
    }

    fn rm(&mut self) -> Op {
        let p = self.existing_path();
        let a = match self.rng.below(5) {
            0 => vec![s("rm"), s("--cached"), s("--"), p.clone()],
            1 => vec![s("rm"), s("-f"), s("--"), p.clone()],
            2 => vec![
                s("rm"),
                s("-r"),
                s("--"),
                parent(&p).unwrap_or(&p).to_owned(),
            ],
            _ => vec![s("rm"), s("--"), p.clone()],
        };
        self.m.files.remove(&p);
        Op::git("rm", true, &a)
    }

    fn mv(&mut self) -> Op {
        let from = self.existing_path();
        let to = if self.rng.chance(50) {
            flip_case(&from).unwrap_or_else(|| self.new_path())
        } else {
            self.new_path()
        };
        self.m.files.remove(&from);
        self.m.files.insert(to.clone());
        let mut sh = Vec::new();
        if let Some(d) = parent(&to) {
            sh.push(format!("mkdir -p {}", q(d)));
        }
        let force = if self.rng.chance(20) { "-f" } else { "-k" };
        sh.push(g(&[s("mv"), s(force), s("--"), from, to]));
        Op::new("mv", true, sh)
    }

    fn restore(&mut self) -> Op {
        let p = self.existing_path();
        let mut a = vec![s("restore")];
        match self.rng.below(5) {
            0 => a.push(s("--staged")),
            1 => {
                a.push(s("--staged"));
                a.push(s("--worktree"));
            }
            2 => {
                let r = self.rev();
                a.push(format!("--source={r}"));
            }
            _ => {}
        }
        a.push(s("--"));
        a.push(p);
        Op::git("restore", true, &a)
    }

    fn reset(&mut self) -> Op {
        let mut a = vec![s("reset")];
        match self.rng.below(8) {
            0 => a.push(s("--soft")),
            1 | 2 => a.push(s("--mixed")),
            3 | 4 => a.push(s("--hard")),
            5 => {
                let p = self.existing_path();
                a.extend([s("--"), p]);
                return Op::git("reset", true, &a);
            }
            _ => {}
        }
        let r = self.rev();
        a.push(r);
        Op::git("reset", true, &a)
    }

    fn commit(&mut self) -> Op {
        let m = self.message();
        self.m.commits += 1;
        let a = match self.rng.below(10) {
            0..=3 => args(&["commit", "-m", &m]),
            4 | 5 => args(&["commit", "-a", "-m", &m]),
            6 => args(&["commit", "--amend", "-m", &m]),
            7 => args(&["commit", "--amend", "--no-edit"]),
            8 => args(&["commit", "--allow-empty", "-m", &m]),
            _ => {
                let p = self.existing_path();
                vec![s("commit"), s("-m"), m, s("--"), p]
            }
        };
        Op::git("commit", true, &a)
    }

    fn branch(&mut self) -> Op {
        let name = self.branch_name();
        let a = match self.rng.below(10) {
            0..=3 => {
                self.m.branches.insert(name.clone());
                vec![s("branch"), name]
            }
            4 => {
                self.m.branches.insert(name.clone());
                let r = self.rev();
                vec![s("branch"), name, r]
            }
            5 => {
                self.m.branches.remove(&name);
                vec![s("branch"), s("-D"), name]
            }
            6 => {
                self.m.branches.remove(&name);
                vec![s("branch"), s("-d"), name]
            }
            7 => {
                let to = self.branch_name();
                self.m.branches.remove(&name);
                self.m.branches.insert(to.clone());
                vec![s("branch"), s("-m"), name, to]
            }
            8 => {
                let r = self.rev();
                vec![s("branch"), s("-f"), name, r]
            }
            _ => args(&["branch", "-vv", "--list"]),
        };
        Op::git("branch", true, &a)
    }

    fn switch(&mut self) -> Op {
        let a = match self.rng.below(8) {
            0..=2 => {
                let b = self.existing_branch();
                self.m.cur = b.clone();
                vec![s("switch"), b]
            }
            3 | 4 => {
                let b = self.branch_name();
                self.m.branches.insert(b.clone());
                self.m.cur = b.clone();
                vec![s("switch"), s("-c"), b]
            }
            5 => {
                let r = self.rev();
                self.m.cur.clear();
                vec![s("switch"), s("--detach"), r]
            }
            6 => args(&["switch", "-"]),
            _ => {
                let b = self.branch_name();
                self.m.branches.insert(b.clone());
                let r = self.rev();
                vec![s("switch"), s("-C"), b, r]
            }
        };
        Op::git("switch", true, &a)
    }

    fn checkout(&mut self) -> Op {
        let a = match self.rng.below(6) {
            0 | 1 => {
                let b = self.existing_branch();
                self.m.cur = b.clone();
                vec![s("checkout"), b]
            }
            2 => {
                let b = self.branch_name();
                self.m.branches.insert(b.clone());
                self.m.cur = b.clone();
                vec![s("checkout"), s("-b"), b]
            }
            3 => {
                let r = self.rev();
                vec![s("checkout"), s("--detach"), r]
            }
            4 => {
                let p = self.existing_path();
                vec![s("checkout"), s("--"), p]
            }
            _ => {
                let r = self.rev();
                let p = self.existing_path();
                vec![s("checkout"), r, s("--"), p]
            }
        };
        Op::git("checkout", true, &a)
    }

    fn tag(&mut self) -> Op {
        let name = (*self.rng.pick(TAGS)).to_owned();
        let m = self.message();
        let a = match self.rng.below(8) {
            0..=2 => {
                self.m.tags.insert(name.clone());
                vec![s("tag"), name]
            }
            3 | 4 => {
                self.m.tags.insert(name.clone());
                let r = self.rev();
                vec![s("tag"), s("-a"), name, s("-m"), m, r]
            }
            5 => {
                self.m.tags.remove(&name);
                vec![s("tag"), s("-d"), name]
            }
            6 => {
                self.m.tags.insert(name.clone());
                let r = self.rev();
                vec![s("tag"), s("-f"), name, r]
            }
            _ => args(&["tag", "-n", "-l"]),
        };
        Op::git("tag", true, &a)
    }

    fn merge(&mut self) -> Op {
        let b = self.other_branch();
        let mut a = vec![s("merge")];
        match self.rng.below(7) {
            0 | 1 => a.push(s("--no-ff")),
            2 => a.push(s("--ff-only")),
            3 => a.push(s("--squash")),
            4 => {
                a.push(s("--no-commit"));
                a.push(s("--no-ff"));
            }
            _ => {}
        }
        a.push(s("--no-edit"));
        if self.rng.chance(10) {
            a.push(s("-m"));
            a.push(self.message());
        }
        a.push(b);
        self.m.commits += 1;
        Op::git("merge", true, &a)
    }

    fn cherry_pick(&mut self) -> Op {
        let r = self.rev();
        let mut a = vec![s("cherry-pick")];
        match self.rng.below(5) {
            0 => a.push(s("-x")),
            1 => a.push(s("-n")),
            2 => a.push(s("--allow-empty")),
            _ => {}
        }
        a.push(r);
        self.m.commits += 1;
        Op::git("cherry-pick", true, &a)
    }

    fn revert(&mut self) -> Op {
        let r = self.rev();
        let mut a = vec![s("revert")];
        if self.rng.chance(25) {
            a.push(s("-n"));
        }
        a.push(s("--no-edit"));
        a.push(r);
        self.m.commits += 1;
        Op::git("revert", true, &a)
    }

    fn rebase(&mut self) -> Op {
        let b = self.other_branch();
        let a = match self.rng.below(5) {
            0 => {
                let r = self.rev();
                vec![s("rebase"), s("--onto"), b, r]
            }
            1 => vec![s("rebase"), s("--no-ff"), b],
            2 => vec![s("rebase"), s("--keep-base"), b],
            _ => vec![s("rebase"), b],
        };
        Op::git("rebase", true, &a)
    }

    /// `rebase -i` with a generated todo: the sequence editor is a `sed` that rewrites one line.
    fn rebase_i(&mut self) -> Op {
        let b = self.other_branch();
        let n = 1 + self.rng.below(3);
        let expr = match self.rng.below(8) {
            0 => format!("-e '{n}s/^pick/drop/'"),
            1 => format!("-e '{}s/^pick/squash/'", n + 1),
            2 => format!("-e '{}s/^pick/fixup/'", n + 1),
            3 => format!("-e '{n}s/^pick/reword/'"),
            4 => format!("-e '{n}s/^pick/edit/'"),
            5 => s("-e '1{h;d}' -e '2{G}'"),
            6 => format!("-e '{n}s/^pick/break\\npick/'"),
            _ => s("-e 's/^pick/p/'"),
        };
        let mut sh = vec![format!("export GIT_SEQUENCE_EDITOR=\"sed -i {expr}\"")];
        let mut a = vec![s("rebase"), s("-i")];
        if self.rng.chance(20) {
            a.push(s("--root"));
        } else {
            a.push(b);
        }
        sh.push(g(&a));
        sh.push(s("unset GIT_SEQUENCE_EDITOR"));
        Op::new("rebase-i", true, sh)
    }

    fn stash(&mut self) -> Op {
        let a = match self.rng.below(10) {
            0..=2 => {
                self.m.stashes += 1;
                vec![s("stash"), s("push"), s("-m"), self.message()]
            }
            3 => {
                self.m.stashes += 1;
                args(&["stash", "push", "-u"])
            }
            4 => {
                self.m.stashes = self.m.stashes.saturating_sub(1);
                args(&["stash", "pop"])
            }
            5 => args(&["stash", "apply"]),
            6 => {
                self.m.stashes = self.m.stashes.saturating_sub(1);
                args(&["stash", "drop"])
            }
            7 => args(&["stash", "list"]),
            8 => args(&["stash", "show", "--stat"]),
            _ => {
                self.m.stashes += 1;
                args(&["stash", "push", "--keep-index"])
            }
        };
        Op::git("stash", true, &a)
    }

    /// Finishes or leaves a stopped merge, cherry-pick, revert or rebase.
    fn sequencer(&mut self) -> Op {
        let tool = *self.rng.pick(&["merge", "cherry-pick", "revert", "rebase"]);
        let action = match (tool, self.rng.below(8)) {
            ("merge", 0..=3) => "--abort",
            ("merge", _) => "--continue",
            (_, 0..=2) => "--abort",
            (_, 3 | 4) => "--skip",
            (_, 5) => "--quit",
            _ => "--continue",
        };
        let action = if tool == "merge" && action == "--skip" {
            "--abort"
        } else {
            action
        };
        let mut sh = Vec::new();
        if action == "--continue" {
            // Resolve by taking the file as it stands, as a user would after editing.
            sh.push(g(&args(&["add", "-A"])));
        }
        sh.push(g(&[s(tool), s(action)]));
        Op::new("sequencer", true, sh)
    }

    fn config(&mut self) -> Op {
        let (k, v) = match self.rng.below(5) {
            0 => ("core.autocrlf", *self.rng.pick(&["true", "input", "false"])),
            1 => ("core.quotepath", *self.rng.pick(&["true", "false"])),
            2 => ("core.eol", *self.rng.pick(&["lf", "crlf", "native"])),
            3 => ("core.safecrlf", *self.rng.pick(&["true", "false", "warn"])),
            _ => ("core.filemode", *self.rng.pick(&["true", "false"])),
        };
        Op::git("config", true, &args(&["config", k, v]))
    }

    /// `.gitignore` and `.gitattributes` content (written as a file, tracked as any other).
    fn ignore_attrs(&mut self) -> Op {
        let (path, text) = if self.rng.chance(50) {
            (
                ".gitignore",
                *self
                    .rng
                    .pick(&["*.dat\n", "d/\n", "!a.txt\n*.txt\n", "sp*\n", "/b.txt\n"]),
            )
        } else {
            (
                ".gitattributes",
                *self.rng.pick(&[
                    "* text=auto\n",
                    "*.txt text eol=crlf\n",
                    "*.dat binary\n",
                    "*.txt -text\n",
                    "crlf.txt text eol=lf\n",
                ]),
            )
        };
        self.m.files.insert(path.to_owned());
        Op::new(
            "attrs",
            false,
            vec![format!("printf {} > {path}", printf_word(text.as_bytes()))],
        )
    }

    fn read(&mut self) -> Op {
        let r = self.rev();
        let p = self.existing_path();
        let a: Vec<String> = match self.rng.below(24) {
            0 => args(&["status"]),
            1 => args(&["status", "-s", "-b"]),
            2 => args(&["status", "--porcelain=v2", "-z", "--branch", "--ignored"]),
            3 => args(&["status", "-uall", "--porcelain"]),
            4 => args(&["diff"]),
            5 => args(&["diff", "--cached", "--stat"]),
            6 => vec![s("diff"), s("--raw"), s("-M"), s("-C"), r],
            7 => vec![
                s("diff"),
                s("--name-status"),
                s("-M"),
                s("HEAD~1"),
                s("HEAD"),
            ],
            8 => args(&["diff", "--numstat", "HEAD"]),
            9 => args(&["log", "--oneline", "--graph", "--all"]),
            10 => vec![s("log"), s("--format=%H %T %P %an %ae %cn %s"), r],
            11 => vec![s("log"), s("--follow"), s("--name-status"), s("--"), p],
            12 => vec![s("show"), s("--stat"), s("--format=%H%n%B"), r],
            13 => vec![s("show"), format!("{r}:{p}")],
            14 => vec![s("blame"), s("--"), p],
            15 => args(&[
                "for-each-ref",
                "--format=%(refname) %(objectname) %(objecttype)",
            ]),
            16 => args(&["ls-files", "--eol", "-s"]),
            17 => args(&["ls-files", "-o", "-m", "-d", "--exclude-standard"]),
            18 => args(&["ls-tree", "-r", "-t", "HEAD"]),
            19 => vec![
                s("rev-parse"),
                r,
                s("HEAD^{tree}"),
                s("--abbrev-ref"),
                s("HEAD"),
            ],
            20 => vec![s("rev-parse"), s("--verify"), s("--quiet"), r],
            21 => args(&["cat-file", "-p", "HEAD"]),
            22 => args(&["describe", "--always", "--tags"]),
            _ => args(&["stash", "list"]),
        };
        Op::git("read", false, &a)
    }
}

/// The helper functions every generated script starts with (the runner adds `g` and `at`).
const HEADER: &str = r#"# differential fuzz script (tests/compat/src/fuzz)
op() { printf '[op %s]\n' "$1"; printf '[op %s]\n' "$1" >&2; }
walk() {
  for p in "$1"* "$1".[!.]* "$1"..?*; do
    case "$p" in .git) continue ;; esac
    if [ -L "$p" ]; then printf 'l %s -> %s\n' "$p" "$(readlink "$p")"
    elif [ -d "$p" ]; then walk "$p/"
    elif [ -f "$p" ]; then
      if [ -x "$p" ]; then m=x; else m=-; fi
      printf 'f %s %s\n' "$m" "$p"
      printf '%s\000' "$p" >> ../fuzz-files
    fi
  done
}
tree() {
  printf '[tree]\n'
  : > ../fuzz-files
  walk ''
  xargs -r -0 cksum < ../fuzz-files
}
unlink_dangling() {
  for p in "$1"* "$1".[!.]* "$1"..?*; do
    case "$p" in .git) continue ;; esac
    if [ -L "$p" ]; then [ -e "$p" ] || rm -f "$p"
    elif [ -d "$p" ]; then unlink_dangling "$p/"
    fi
  done
}
probe() {
  g status --porcelain=v2 --branch
  g log --all -n 40 --format='%H %T %P %D %s'
  g diff --raw
  g diff --cached --raw
  g ls-files -s
  g for-each-ref
  tree
}
"#;

/// The script of `ops`: the header, `git init`, and each step with its number, its clock and (for a
/// step that changes the repository) the state read back after it.
pub fn script(ops: &[Op]) -> String {
    let mut out = String::from(HEADER);
    out.push_str("g init\n");
    for (i, op) in ops.iter().enumerate() {
        out.push_str(&format!("op {i}\nat {}\n", i + 1));
        for line in &op.sh {
            out.push_str(line);
            out.push('\n');
        }
        if op.mutating {
            out.push_str("probe\n");
        }
    }
    // The runner reads the work tree through symbolic links, so a link to nothing would end the
    // comparison with a read error (the per-step transcript has already compared it).
    out.push_str("unlink_dangling ''\n");
    out
}

/// A one-line description of step `i`, for a report.
pub fn describe(ops: &[Op], i: usize) -> String {
    ops.get(i).map_or_else(
        || format!("op {i}"),
        |op| format!("op {i} [{}]: {}", op.label, op.sh.join(" ; ")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic() {
        let (mut a, mut b) = (Rng::new(7), Rng::new(7));
        for _ in 0..50 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        assert_ne!(Rng::new(7).next_u64(), Rng::new(8).next_u64());
    }

    #[test]
    fn printf_word_escapes_bytes() {
        assert_eq!(printf_word(b"a\r\nb"), "'a\\015\\nb'");
        assert_eq!(
            printf_word(&[0, 0xff, b'%', b'\\', b'\'']),
            "'\\000\\377%%\\\\\\047'"
        );
    }

    #[test]
    fn case_flip_flips_the_base_name() {
        assert_eq!(flip_case("d/Case.txt").as_deref(), Some("d/case.txt"));
        assert_eq!(flip_case("a.txt").as_deref(), Some("A.txt"));
        assert_eq!(flip_case("123/456"), None);
    }
}
