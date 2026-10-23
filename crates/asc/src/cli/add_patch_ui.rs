//! The `add -p` dialogue for one file (SPEC §9.5): git's prompt, letters and messages. Answers
//! are read a line at a time, as git does when its input is not a terminal. The module neither
//! touches the staging nor starts a program: the editor is a callback, and the caller applies
//! the decisions the dialogue leaves in the [`FileDiff`].

use std::io::{self, BufRead, Write};

use regex::bytes::RegexBuilder;

use super::add_patch_diff::{Decision, EditResult, FileDiff, Part, Shape};

/// Where the answers come from.
pub trait Input {
    /// The next line without its line ending; `None` at the end of the input.
    fn line(&mut self) -> io::Result<Option<Vec<u8>>>;
}

impl<R: BufRead> Input for R {
    fn line(&mut self) -> io::Result<Option<Vec<u8>>> {
        read_line(self)
    }
}

/// Reads one line of at most [`MAX_ANSWER_BYTES`] bytes; the rest of a longer line is dropped.
pub fn read_line(r: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    let mut seen = false;
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        seen = true;
        let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        let room = MAX_ANSWER_BYTES.saturating_sub(buf.len());
        buf.extend_from_slice(&chunk[..take.min(room)]);
        r.consume(take);
        if done {
            break;
        }
    }
    if !seen {
        return Ok(None);
    }
    while matches!(buf.last(), Some(b'\n' | b'\r')) {
        buf.pop();
    }
    Ok(Some(buf))
}

/// Longest answer read; longer input is cut (a regex or a hunk number never needs more).
pub const MAX_ANSWER_BYTES: usize = 64 * 1024;

/// Largest compiled search expression (`/`), so a hostile pattern cannot take the memory.
const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// Hunks `g` lists at a time.
const LIST_PAGE: usize = 20;

/// What the editor made of a hunk's text: `None` when the editor failed.
pub type Editor<'a> = dyn FnMut(&[u8]) -> io::Result<Option<Vec<u8>>> + 'a;

/// The terminal side of the dialogue.
pub struct Io<R, W> {
    pub input: R,
    pub out: W,
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The line `n` hunks make in git's plural messages.
fn hunks(n: usize) -> String {
    format!("{n} hunk{}", if n == 1 { "" } else { "s" })
}

/// The next undecided hunk after `from` (rolling over), other than `from` itself.
fn undecided_after(file: &FileDiff, from: usize) -> Option<usize> {
    let n = file.hunks.len();
    (1..n).map(|d| (from + d) % n).find(|&i| file.hunks[i].decision == Decision::Undecided)
}

/// The previous undecided hunk before `from` (rolling over), other than `from` itself.
fn undecided_before(file: &FileDiff, from: usize) -> Option<usize> {
    let n = file.hunks.len();
    (1..n).map(|d| (from + n - d) % n).find(|&i| file.hunks[i].decision == Decision::Undecided)
}

impl<R: Input, W: Write> Io<R, W> {
    /// Shows `file` hunk by hunk and records the decisions in it. Returns `true` when the user
    /// quit (or the input ended): no later file is offered.
    pub fn review(&mut self, file: &mut FileDiff, edit: &mut Editor<'_>) -> io::Result<bool> {
        if !file.offered() {
            return Ok(false);
        }
        self.out.write_all(file.header())?;
        let (mut idx, mut rendered, mut quit) = (0usize, None::<usize>, false);
        loop {
            let count = file.hunks.len();
            if idx >= count {
                idx = 0;
            }
            let decision = if count > 0 { file.hunks[idx].decision } else { file.head };
            let (prev, next) = if count > 0 {
                (undecided_before(file, idx), undecided_after(file, idx))
            } else {
                (None, None)
            };
            if prev.is_none() && next.is_none() && decision != Decision::Undecided {
                break;
            }
            if count > 0 && rendered != Some(idx) {
                self.out.write_all(&file.render(idx))?;
                rendered = Some(idx);
            }
            let can_edit = count > 0 && file.shape != Shape::Deleted && !matches!(file.hunks[idx].part, Part::Mode { .. });
            let can_split = count > 0 && file.splittable(idx) > 1;
            let letters = letters(count, prev.is_some(), next.is_some(), can_split, can_edit);
            let label = match (&file.hunks.get(idx).map(|h| &h.part), file.shape) {
                (Some(Part::Mode { .. }), _) => "Stage mode change",
                (_, Shape::Deleted) => "Stage deletion",
                (_, Shape::Added) => "Stage addition",
                _ => "Stage this hunk",
            };
            let was = match decision {
                Decision::Use => " (was: y)",
                Decision::Skip => " (was: n)",
                Decision::Undecided => "",
            };
            write!(self.out, "({}/{}) {label}{was} [{letters}]? ", idx + 1, count.max(1))?;
            self.out.flush()?;
            let Some(answer) = self.input.line()? else {
                quit = true;
                break;
            };
            let Some(&c0) = answer.first() else { continue };
            if answer.len() != 1 && c0 != b'/' && c0 != b'g' {
                writeln!(self.out, "Only one letter is expected, got '{}'", text(&answer))?;
                continue;
            }
            let decide = |file: &mut FileDiff, idx: usize, d: Decision| {
                if count > 0 {
                    file.hunks[idx].decision = d;
                } else {
                    file.head = d;
                }
            };
            match c0 {
                b'y' | b'Y' | b'n' | b'N' => {
                    let d = if c0.eq_ignore_ascii_case(&b'y') { Decision::Use } else { Decision::Skip };
                    decide(file, idx, d);
                    idx = next.unwrap_or(count);
                }
                b'a' | b'A' | b'd' | b'D' => {
                    let d = if c0.eq_ignore_ascii_case(&b'a') { Decision::Use } else { Decision::Skip };
                    if count > 0 {
                        for h in &mut file.hunks[idx..] {
                            if h.decision == Decision::Undecided {
                                h.decision = d;
                            }
                        }
                        idx = count;
                    } else if file.head == Decision::Undecided {
                        file.head = d;
                    }
                }
                b'q' | b'Q' => {
                    quit = true;
                    break;
                }
                b'k' => match prev {
                    Some(p) => idx = p,
                    None => writeln!(self.out, "No other undecided hunk")?,
                },
                b'K' => {
                    if count > 1 {
                        idx = (idx + count - 1) % count;
                    } else {
                        writeln!(self.out, "No other hunk")?;
                    }
                }
                b'j' => match next {
                    Some(n) => idx = n,
                    None => writeln!(self.out, "No other undecided hunk")?,
                },
                b'J' => {
                    if count > 1 {
                        idx = (idx + 1) % count;
                    } else {
                        writeln!(self.out, "No other hunk")?;
                    }
                }
                b'g' => {
                    if count <= 1 {
                        writeln!(self.out, "No other hunks to goto")?;
                    } else {
                        match self.go_to(file, idx, &answer[1..])? {
                            Goto::Hunk(i) => idx = i,
                            Goto::Stay => {}
                            Goto::Eof => {
                                quit = true;
                                break;
                            }
                        }
                    }
                }
                b'/' => {
                    if count <= 1 {
                        writeln!(self.out, "No other hunks to search")?;
                    } else {
                        match self.search(file, idx, &answer[1..])? {
                            Goto::Hunk(i) => idx = i,
                            Goto::Stay => {}
                            Goto::Eof => {
                                quit = true;
                                break;
                            }
                        }
                    }
                }
                b's' if count > 0 && file.splittable(idx) > 1 => {
                    let k = file.split(idx);
                    writeln!(self.out, "Split into {k} hunks.")?;
                    rendered = None;
                }
                b's' => writeln!(self.out, "Sorry, cannot split this hunk")?,
                b'e' if can_edit => match self.edit(file, idx, edit)? {
                    Edit::Done => idx = next.unwrap_or(count),
                    Edit::Kept => {}
                    Edit::Eof => {
                        quit = true;
                        break;
                    }
                },
                b'e' => writeln!(self.out, "Sorry, cannot edit this hunk")?,
                b'p' | b'P' if count > 0 => rendered = None,
                b'?' => self.help(count, prev.is_some(), next.is_some(), can_split, can_edit)?,
                _ => writeln!(self.out, "Unknown command '{}' (use '?' for help)", text(&answer))?,
            }
        }
        // Git ends the prompt line of a file once the file is settled.
        writeln!(self.out)?;
        Ok(quit)
    }

    /// `g`: choose a hunk by number, from a list that shows `LIST_PAGE` hunks at a time.
    fn go_to(&mut self, file: &FileDiff, idx: usize, given: &[u8]) -> io::Result<Goto> {
        let count = file.hunks.len();
        let mut answer = given.to_vec();
        if answer.is_empty() {
            let mut i = idx.saturating_sub(10);
            loop {
                for _ in 0..LIST_PAGE {
                    if i >= count {
                        break;
                    }
                    if !(i == 0 && file.mode_change()) {
                        let c = match file.hunks[i].decision {
                            Decision::Use => '+',
                            Decision::Skip => '-',
                            Decision::Undecided => ' ',
                        };
                        write!(self.out, "{c}{:>2}: ", i + 1)?;
                        self.out.write_all(&file.summary(i))?;
                        writeln!(self.out)?;
                    }
                    i += 1;
                }
                let prompt = if i < count { "go to which hunk (<ret> to see more)? " } else { "go to which hunk? " };
                write!(self.out, "{prompt}")?;
                self.out.flush()?;
                match self.input.line()? {
                    None => return Ok(Goto::Eof),
                    Some(line) if !line.is_empty() => {
                        answer = line;
                        break;
                    }
                    Some(_) => {}
                }
            }
        }
        let shown = text(&answer);
        let digits = shown.trim_start();
        let number = (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .then(|| digits.parse::<usize>().ok())
            .flatten();
        let Some(n) = number else {
            writeln!(self.out, "Invalid number: '{shown}'")?;
            return Ok(Goto::Stay);
        };
        if n == 0 || n > count {
            writeln!(self.out, "Sorry, only {} available.", hunks(count))?;
            return Ok(Goto::Stay);
        }
        Ok(Goto::Hunk(n - 1))
    }

    /// `/`: the next hunk whose text matches an expression; the current one comes last.
    /// `None` is the end of the input.
    fn search(&mut self, file: &FileDiff, idx: usize, given: &[u8]) -> io::Result<Goto> {
        let count = file.hunks.len();
        let mut pattern = given.to_vec();
        if pattern.is_empty() {
            write!(self.out, "search for regex? ")?;
            self.out.flush()?;
            match self.input.line()? {
                None => return Ok(Goto::Eof),
                Some(line) => pattern = line,
            }
            if pattern.is_empty() {
                return Ok(Goto::Stay);
            }
        }
        let re = match RegexBuilder::new(&text(&pattern))
            .multi_line(true)
            .size_limit(REGEX_SIZE_LIMIT)
            .dfa_size_limit(REGEX_SIZE_LIMIT)
            .build()
        {
            Ok(re) => re,
            Err(e) => {
                let why = e.to_string().lines().last().unwrap_or("").trim().to_owned();
                writeln!(self.out, "Malformed search regexp {}: {why}", text(&pattern))?;
                return Ok(Goto::Stay);
            }
        };
        for d in 1..=count {
            let i = (idx + d) % count;
            if re.is_match(&file.render(i)) {
                return Ok(Goto::Hunk(i));
            }
        }
        writeln!(self.out, "No hunk matches the given pattern")?;
        Ok(Goto::Stay)
    }

    /// `e`: edit the hunk until it applies, or the user gives up.
    fn edit(&mut self, file: &mut FileDiff, idx: usize, editor: &mut Editor<'_>) -> io::Result<Edit> {
        loop {
            if let Some(edited) = editor(&file.edit_text(idx))? {
                match file.set_edit(idx, &edited) {
                    Ok(EditResult::Applied) => {
                        file.hunks[idx].decision = Decision::Use;
                        return Ok(Edit::Done);
                    }
                    Ok(EditResult::Abandoned) => return Ok(Edit::Kept),
                    Err(_) => {
                        self.out.flush()?;
                        let at = file.old_line(idx);
                        let path = text(&file.path);
                        eprintln!("error: patch failed: {path}:{at}\nerror: {path}: patch does not apply");
                    }
                }
            }
            loop {
                write!(self.out, "Your edited hunk does not apply. Edit again (saying \"no\" discards!) [y/n]? ")?;
                self.out.flush()?;
                match self.input.line()? {
                    None => return Ok(Edit::Eof),
                    Some(a) if a.first() == Some(&b'y') => break,
                    Some(a) if a.first() == Some(&b'n') => return Ok(Edit::Kept),
                    Some(_) => {}
                }
            }
        }
    }

    /// `?`: the commands the prompt offers.
    fn help(&mut self, count: usize, prev: bool, next: bool, split: bool, edit: bool) -> io::Result<()> {
        let hunk = count > 0;
        let lines: [(bool, &str); 16] = [
            (true, "y - stage this hunk"),
            (true, "n - do not stage this hunk"),
            (true, "q - quit; do not stage this hunk or any of the remaining ones"),
            (true, "a - stage this hunk and all later hunks in the file"),
            (true, "d - do not stage this hunk or any of the later hunks in the file"),
            (next, "j - go to the next undecided hunk, roll over at the bottom"),
            (count > 1, "J - go to the next hunk, roll over at the bottom"),
            (prev, "k - go to the previous undecided hunk, roll over at the top"),
            (count > 1, "K - go to the previous hunk, roll over at the top"),
            (count > 1, "g - select a hunk to go to"),
            (count > 1, "/ - search for a hunk matching the given regex"),
            (split, "s - split the current hunk into smaller hunks"),
            (edit, "e - manually edit the current hunk"),
            (hunk, "p - print the current hunk"),
            (hunk, "P - print the current hunk using the pager"),
            (true, "? - print help"),
        ];
        for (shown, line) in lines {
            if shown {
                writeln!(self.out, "{line}")?;
            }
        }
        Ok(())
    }
}

enum Goto {
    Hunk(usize),
    Stay,
    Eof,
}

enum Edit {
    /// The edited hunk applies and is chosen.
    Done,
    /// The hunk is as it was.
    Kept,
    Eof,
}

/// The letters the prompt offers, in git's order.
fn letters(count: usize, prev: bool, next: bool, split: bool, edit: bool) -> String {
    let mut s = String::from("y,n,q,a,d");
    let mut add = |on: bool, l: &str| {
        if on {
            s.push(',');
            s.push_str(l);
        }
    };
    add(prev, "k");
    add(count > 1, "K");
    add(next, "j");
    add(count > 1, "J");
    add(count > 1, "g,/");
    add(split, "s");
    add(edit, "e");
    add(count > 0, "p,P");
    add(true, "?");
    s
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use asc_engine::diff::line::Algorithm;

    use super::super::add_patch_diff::{Settings, Side, build};
    use super::*;

    fn numbered(n: usize, change: &[usize]) -> String {
        (1..=n).map(|i| if change.contains(&i) { format!("new{i}\n") } else { format!("{i}\n") }).collect()
    }

    fn file(old: &str, new: &str) -> FileDiff {
        let side = |t: &str| Some(Side { content: t.as_bytes().to_vec(), mode: 0o100644, oid: Some(asc_core::git::blob_oid(t.as_bytes())) });
        let s = Settings { context: 3, inter_hunk: 0, algorithm: Algorithm::Myers, indent_heuristic: true };
        let mut f = build(b"a/f", b"b/f", side(old), side(new), 7, &s);
        f.path = b"f".to_vec();
        f
    }

    /// Four hunks apart.
    fn four() -> FileDiff {
        file(&numbered(60, &[]), &numbered(60, &[5, 20, 35, 50]))
    }

    fn run(f: &mut FileDiff, answers: &str) -> (bool, String) {
        let mut io = Io { input: Cursor::new(answers.as_bytes().to_vec()), out: Vec::new() };
        let quit = io.review(f, &mut |_| Ok(None)).unwrap();
        (quit, String::from_utf8(io.out).unwrap())
    }

    fn used(f: &FileDiff) -> Vec<bool> {
        f.hunks.iter().map(|h| h.decision == Decision::Use).collect()
    }

    #[test]
    fn y_n_walk_the_hunks_and_finish() {
        let mut f = four();
        let (quit, out) = run(&mut f, "y\nn\ny\nn\n");
        assert!(!quit);
        assert_eq!(used(&f), [true, false, true, false]);
        assert!(out.contains("(1/4) Stage this hunk [y,n,q,a,d,k,K,j,J,g,/,e,p,P,?]? "), "{out}");
        assert!(out.contains("(4/4) Stage this hunk [y,n,q,a,d,K,J,g,/,e,p,P,?]? "), "{out}");
    }

    #[test]
    fn a_takes_the_rest_and_comes_back_for_earlier_hunks() {
        let mut f = four();
        let (_, out) = run(&mut f, "J\nJ\na\nn\nn\n");
        assert_eq!(used(&f), [false, false, true, true]);
        assert!(out.matches("(1/4)").count() >= 2, "{out}");
    }

    #[test]
    fn d_skips_the_rest() {
        let mut f = four();
        run(&mut f, "y\nd\n");
        assert_eq!(used(&f), [true, false, false, false]);
    }

    #[test]
    fn q_and_the_end_of_input_leave_the_rest_undecided() {
        let mut f = four();
        let (quit, _) = run(&mut f, "y\nq\n");
        assert!(quit);
        assert_eq!(f.hunks[1].decision, Decision::Undecided);
        let mut f = four();
        let (quit, out) = run(&mut f, "y\n");
        assert!(quit && out.ends_with("? \n"), "{out}");
    }

    #[test]
    fn moves_roll_over_and_report_what_is_missing() {
        let mut f = four();
        let (_, out) = run(&mut f, "K\nq\n");
        assert!(out.contains("(4/4) Stage this hunk"), "{out}");
        let mut f = file(&numbered(8, &[]), &numbered(8, &[4]));
        let (_, out) = run(&mut f, "j\nJ\nk\nK\ng\n/x\ns\nq\n");
        for want in [
            "No other undecided hunk",
            "No other hunk",
            "No other hunks to goto",
            "No other hunks to search",
            "Sorry, cannot split this hunk",
        ] {
            assert!(out.contains(want), "{want}: {out}");
        }
    }

    #[test]
    fn decided_hunks_are_marked_when_shown_again() {
        let mut f = four();
        let (_, out) = run(&mut f, "y\nK\nq\n");
        assert!(out.contains("(1/4) Stage this hunk (was: y) ["), "{out}");
    }

    #[test]
    fn g_lists_hunks_and_goes_to_one() {
        let mut f = four();
        let (_, out) = run(&mut f, "n\ng\n3\nq\n");
        assert!(out.contains("- 1:  -2,7 +2,7          -5\n"), "{out}");
        assert!(out.contains("  2:  -17,7 +17,7        -20\n"), "{out}");
        assert!(out.contains("go to which hunk? "), "{out}");
        assert!(out.contains("(3/4)"), "{out}");
        let (_, out) = run(&mut four(), "g\n9\ng\nx\nq\n");
        assert!(out.contains("Sorry, only 4 hunks available."), "{out}");
        assert!(out.contains("Invalid number: 'x'"), "{out}");
        let (_, out) = run(&mut four(), "g3\nq\n");
        assert!(out.contains("(3/4)"), "{out}");
    }

    #[test]
    fn slash_finds_the_next_matching_hunk_and_reports_a_miss() {
        let mut f = four();
        let (_, out) = run(&mut f, "/new35\n/nothing here\n/(\nq\n");
        assert!(out.contains("(3/4)"), "{out}");
        assert!(out.contains("No hunk matches the given pattern"), "{out}");
        assert!(out.contains("Malformed search regexp ("), "{out}");
    }

    #[test]
    fn s_splits_the_hunk() {
        let mut f = file(&numbered(14, &[]), &numbered(14, &[4, 8]));
        let (_, out) = run(&mut f, "s\ny\nn\n");
        assert!(out.contains("Split into 2 hunks."), "{out}");
        assert_eq!(used(&f), [true, false]);
    }

    #[test]
    fn unknown_and_long_answers_are_refused() {
        let mut f = four();
        let (_, out) = run(&mut f, "z\nyes\n\nq\n");
        assert!(out.contains("Unknown command 'z' (use '?' for help)"), "{out}");
        assert!(out.contains("Only one letter is expected, got 'yes'"), "{out}");
    }

    #[test]
    fn help_lists_what_the_prompt_offers() {
        let mut f = four();
        let (_, out) = run(&mut f, "?\nq\n");
        for want in ["y - stage this hunk", "J - go to the next hunk", "g - select a hunk", "? - print help"] {
            assert!(out.contains(want), "{want}: {out}");
        }
        assert!(!out.contains("s - split"), "{out}");
    }

    #[test]
    fn edit_applies_a_good_edit_and_asks_again_for_a_bad_one() {
        let mut f = file(&numbered(12, &[]), &numbered(12, &[5]));
        let mut tries = 0;
        let mut io = Io { input: Cursor::new(b"e\ny\nq\n".to_vec()), out: Vec::new() };
        let quit = io
            .review(&mut f, &mut |t| {
                tries += 1;
                let t = String::from_utf8_lossy(t).into_owned();
                Ok(Some(if tries == 1 { t.replace(" 3\n", " three\n") } else { t.replace("+new5\n", "+five\n") }.into_bytes()))
            })
            .unwrap();
        let out = String::from_utf8(io.out).unwrap();
        assert!(!quit && tries == 2, "{out}");
        assert!(out.contains("Your edited hunk does not apply. Edit again (saying \"no\" discards!) [y/n]? "), "{out}");
        assert_eq!(f.hunks[0].decision, Decision::Use);
        assert_eq!(String::from_utf8(f.assemble().unwrap()).unwrap(), numbered(12, &[]).replace("5\n", "five\n"));
    }

    #[test]
    fn edit_is_refused_for_a_mode_change_and_a_deletion() {
        let side = |mode| Some(Side { content: b"x\n".to_vec(), mode, oid: Some(asc_core::git::blob_oid(b"x\n")) });
        let s = Settings { context: 3, inter_hunk: 0, algorithm: Algorithm::Myers, indent_heuristic: true };
        let mut f = build(b"a/f", b"b/f", side(0o100755), side(0o100644), 7, &s);
        let (_, out) = run(&mut f, "e\ny\n");
        assert!(out.contains("Stage mode change [y,n,q,a,d,p,P,?]? "), "{out}");
        assert!(out.contains("Sorry, cannot edit this hunk"), "{out}");
        let mut f = build(b"a/f", b"b/f", side(0o100644), None, 7, &s);
        let (_, out) = run(&mut f, "e\ny\n");
        assert!(out.contains("Stage deletion [y,n,q,a,d,p,P,?]? "), "{out}");
        assert_eq!(f.hunks[0].decision, Decision::Use);
    }

    #[test]
    fn an_empty_added_file_is_one_prompt_without_hunks() {
        let s = Settings { context: 3, inter_hunk: 0, algorithm: Algorithm::Myers, indent_heuristic: true };
        let side = Some(Side { content: Vec::new(), mode: 0o100644, oid: Some(asc_core::git::blob_oid(b"")) });
        let mut f = build(b"a/f", b"b/f", None, side, 7, &s);
        let (_, out) = run(&mut f, "?\ny\n");
        assert!(out.contains("(1/1) Stage addition [y,n,q,a,d,?]? "), "{out}");
        assert_eq!(f.head, Decision::Use);
    }

    #[test]
    fn long_lines_are_cut() {
        let mut r = Cursor::new(vec![b'x'; MAX_ANSWER_BYTES + 100]);
        let got = read_line(&mut r).unwrap().unwrap();
        assert_eq!(got.len(), MAX_ANSWER_BYTES);
        assert_eq!(read_line(&mut r).unwrap(), None);
        let mut r = Cursor::new(b"a\r\n\nb".to_vec());
        assert_eq!(read_line(&mut r).unwrap().unwrap(), b"a");
        assert_eq!(read_line(&mut r).unwrap().unwrap(), b"");
        assert_eq!(read_line(&mut r).unwrap().unwrap(), b"b");
        assert_eq!(read_line(&mut r).unwrap(), None);
    }
}
