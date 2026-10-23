//! `log --graph`: a port of git's `graph.c` lane algorithm. The caller feeds commits in output
//! order with [`Graph::update`]; the graph hands back the text to the left of each output line.

use asc_core::id::ChangesetId;

use crate::cli::format::color;

/// `column_colors_ansi`, then the reset that marks "uncoloured".
const COLORS: [&str; 12] = [
    "red", "green", "yellow", "blue", "magenta", "cyan", "bold red", "bold green", "bold yellow",
    "bold blue", "bold magenta", "bold cyan",
];
const COLORS_MAX: u16 = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Padding,
    Skip,
    PreCommit,
    Commit,
    PostMerge,
    Collapsing,
}

#[derive(Clone, Copy, Debug)]
struct Column {
    commit: ChangesetId,
    color: u16,
}

/// One line being built.
struct Line {
    text: Vec<u8>,
    width: usize,
    color: bool,
}

impl Line {
    fn new(color: bool) -> Self {
        Self { text: Vec::new(), width: 0, color }
    }

    fn ch(&mut self, c: char) {
        let mut b = [0u8; 4];
        self.text.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
        self.width += 1;
    }

    fn chars(&mut self, c: char, n: usize) {
        for _ in 0..n {
            self.ch(c);
        }
    }

    fn column(&mut self, col: &Column, c: char) {
        let painted = self.color && col.color < COLORS_MAX;
        if painted {
            self.text.extend_from_slice(color::sgr(COLORS[usize::from(col.color)]).as_bytes());
        }
        self.ch(c);
        if painted {
            self.text.extend_from_slice(color::RESET.as_bytes());
        }
    }
}

/// The lane state of the graph, between two commits.
pub struct Graph {
    color: bool,
    first_parent_only: bool,
    left_right: bool,
    commit: Option<ChangesetId>,
    parents: Vec<ChangesetId>,
    num_parents: usize,
    width: usize,
    expansion_row: usize,
    state: State,
    prev_state: State,
    commit_index: usize,
    prev_commit_index: usize,
    merge_layout: i32,
    edges_added: i32,
    columns: Vec<Column>,
    new_columns: Vec<Column>,
    mapping: Vec<i32>,
    old_mapping: Vec<i32>,
    mapping_size: usize,
    default_color: u16,
    left: bool,
}

impl Graph {
    pub fn new(color: bool, left_right: bool) -> Self {
        Self {
            color,
            first_parent_only: false,
            left_right,
            commit: None,
            parents: Vec::new(),
            num_parents: 0,
            width: 0,
            expansion_row: 0,
            state: State::Padding,
            prev_state: State::Padding,
            commit_index: 0,
            prev_commit_index: 0,
            merge_layout: 0,
            edges_added: 0,
            columns: Vec::new(),
            new_columns: Vec::new(),
            mapping: Vec::new(),
            old_mapping: Vec::new(),
            mapping_size: 0,
            // git starts on the last colour, so the first lane increments to red.
            default_color: COLORS_MAX - 1,
            left: false,
        }
    }

    /// Only the first parent is drawn (`--first-parent`).
    pub fn first_parent_only(&mut self, on: bool) {
        self.first_parent_only = on;
    }

    /// Columns the graph takes on the current line, for `%<|(…)`.
    pub fn width(&self) -> usize {
        self.width
    }

    fn current_color(&self) -> u16 {
        if self.color { self.default_color } else { COLORS_MAX }
    }

    fn increment_color(&mut self) {
        self.default_color = (self.default_color + 1) % COLORS_MAX;
    }

    fn find_commit_color(&self, commit: &ChangesetId) -> u16 {
        self.columns.iter().find(|c| &c.commit == commit).map_or_else(|| self.current_color(), |c| c.color)
    }

    fn find_new_column(&self, commit: &ChangesetId) -> Option<usize> {
        self.new_columns.iter().position(|c| &c.commit == commit)
    }

    fn set_state(&mut self, s: State) {
        self.prev_state = self.state;
        self.state = s;
    }

    /// The parents that count: the first only with `--first-parent`.
    fn interesting(&self) -> &[ChangesetId] {
        let n = if self.first_parent_only { self.parents.len().min(1) } else { self.parents.len() };
        &self.parents[..n]
    }

    /// Starts the lines of `commit`, whose parents (inside the range) are `parents`.
    pub fn update(&mut self, commit: ChangesetId, parents: Vec<ChangesetId>, left: bool) {
        self.commit = Some(commit);
        self.parents = parents;
        self.left = left;
        self.num_parents = self.interesting().len();
        self.prev_commit_index = self.commit_index;
        self.update_columns();
        self.expansion_row = 0;
        if self.state != State::Padding {
            self.state = State::Skip;
        } else if self.needs_pre_commit_line() {
            self.state = State::PreCommit;
        } else {
            self.state = State::Commit;
        }
    }

    fn insert_into_new_columns(&mut self, commit: ChangesetId, idx: Option<usize>) {
        let found = self.find_new_column(&commit);
        let i = match found {
            Some(i) => i,
            None => {
                let color = self.find_commit_color(&commit);
                self.new_columns.push(Column { commit, color });
                self.new_columns.len() - 1
            }
        };
        let mapping_idx: usize;
        if self.num_parents > 1 && idx.is_some() && self.merge_layout == -1 {
            // The first parent of a merge: choose the layout of the merge line by whether the
            // parent sits to the left of the merge.
            let dist = idx.unwrap_or(0) as i64 - i as i64;
            let shift = if dist > 1 { 2 * dist - 3 } else { 1 };
            self.merge_layout = if dist > 0 { 0 } else { 1 };
            self.edges_added = self.num_parents as i32 + self.merge_layout - 2;
            mapping_idx = (self.width as i64 + i64::from(self.merge_layout - 1) * shift) as usize;
            self.width += 2 * self.merge_layout as usize;
        } else if self.edges_added > 0 && self.width >= 2 && self.mapping.get(self.width - 2) == Some(&(i as i32)) {
            // Columns added by a merge: the commit was found in the last existing column, so
            // the two edges join at once.
            mapping_idx = self.width - 2;
            self.edges_added = -1;
        } else {
            mapping_idx = self.width;
            self.width += 2;
        }
        if mapping_idx >= self.mapping.len() {
            self.mapping.resize(mapping_idx + 1, -1);
        }
        self.mapping[mapping_idx] = i as i32;
    }

    fn update_columns(&mut self) {
        std::mem::swap(&mut self.columns, &mut self.new_columns);
        self.new_columns.clear();
        std::mem::swap(&mut self.mapping, &mut self.old_mapping);
        let max_new = self.columns.len() + self.num_parents;
        self.mapping_size = 2 * max_new + 2;
        self.mapping.clear();
        self.mapping.resize(self.mapping_size + 4, -1);
        self.width = 0;
        self.edges_added = 0;

        let commit = self.commit.unwrap_or_else(|| ChangesetId::from_bytes([0; 32]));
        let mut seen_this = false;
        let mut in_columns = true;
        let num_columns = self.columns.len();
        for i in 0..=num_columns {
            let col_commit = if i == num_columns {
                if seen_this {
                    break;
                }
                in_columns = false;
                commit
            } else {
                self.columns[i].commit
            };
            if col_commit == commit {
                seen_this = true;
                self.commit_index = i;
                self.merge_layout = -1;
                let parents: Vec<ChangesetId> = self.interesting().to_vec();
                for p in parents {
                    // A merge, or the start of a new childless column, takes a new colour.
                    if self.num_parents > 1 || !in_columns {
                        self.increment_color();
                    }
                    self.insert_into_new_columns(p, Some(i));
                }
                if self.num_parents == 0 {
                    // A root commit still keeps its slot, so the lanes to its right collapse.
                    self.width += 2;
                }
            } else {
                self.insert_into_new_columns(col_commit, None);
            }
        }
        while self.mapping_size > 1 && self.mapping.get(self.mapping_size - 1).copied().unwrap_or(-1) < 0 {
            self.mapping_size -= 1;
        }
    }

    fn num_dashed_parents(&self) -> i32 {
        self.num_parents as i32 + self.merge_layout - 3
    }

    fn num_expansion_rows(&self) -> usize {
        (self.num_dashed_parents().max(0) * 2) as usize
    }

    fn needs_pre_commit_line(&self) -> bool {
        self.num_parents >= 3
            && self.commit_index + 1 < self.columns.len()
            && self.expansion_row < self.num_expansion_rows()
    }

    /// Whether all the lines of the current commit were handed out.
    pub fn is_commit_finished(&self) -> bool {
        self.state == State::Padding
    }

    fn is_mapping_correct(&self) -> bool {
        self.mapping[..self.mapping_size.min(self.mapping.len())]
            .iter()
            .enumerate()
            .all(|(i, &t)| t < 0 || t as usize == i / 2)
    }

    fn pad(&self, line: &mut Line) {
        if line.width < self.width {
            line.chars(' ', self.width - line.width);
        }
    }

    fn commit_char(&self) -> char {
        if self.left_right {
            if self.left { '<' } else { '>' }
        } else {
            '*'
        }
    }

    /// The next line of the graph, and whether it is the commit's own line.
    pub fn next_line(&mut self) -> (Vec<u8>, bool) {
        let mut line = Line::new(self.color);
        let is_commit = match self.state {
            State::Padding => {
                self.output_padding(&mut line);
                false
            }
            State::Skip => {
                line.text.extend_from_slice(b"...");
                line.width += 3;
                self.pad(&mut line);
                if self.needs_pre_commit_line() {
                    self.set_state(State::PreCommit);
                } else {
                    self.set_state(State::Commit);
                }
                false
            }
            State::PreCommit => {
                self.output_pre_commit(&mut line);
                false
            }
            State::Commit => {
                self.output_commit(&mut line);
                true
            }
            State::PostMerge => {
                self.output_post_merge(&mut line);
                false
            }
            State::Collapsing => {
                self.output_collapsing(&mut line);
                false
            }
        };
        (line.text, is_commit)
    }

    /// The prefix for a line that is not a graph line of its own (git's `graph_padding_line`).
    pub fn padding_line(&mut self) -> Vec<u8> {
        if self.state != State::Commit {
            return self.next_line().0;
        }
        let mut line = Line::new(self.color);
        for i in 0..self.columns.len() {
            let col = self.columns[i];
            line.column(&col, '|');
            if Some(col.commit) == self.commit && self.num_parents > 2 {
                line.chars(' ', (self.num_parents - 2) * 2);
            } else {
                line.ch(' ');
            }
        }
        self.pad(&mut line);
        self.prev_state = State::Padding;
        line.text
    }

    fn output_padding(&self, line: &mut Line) {
        for col in &self.new_columns {
            line.column(col, '|');
            line.ch(' ');
        }
        self.pad(line);
    }

    fn output_pre_commit(&mut self, line: &mut Line) {
        let mut seen_this = false;
        for i in 0..self.columns.len() {
            let col = self.columns[i];
            if Some(col.commit) == self.commit {
                seen_this = true;
                line.column(&col, '|');
                line.chars(' ', self.expansion_row);
            } else if seen_this && self.expansion_row == 0 {
                // The first expansion row keeps the "\" of a preceding post-merge row.
                if self.prev_state == State::PostMerge && self.prev_commit_index < i {
                    line.column(&col, '\\');
                } else {
                    line.column(&col, '|');
                }
            } else if seen_this && self.expansion_row > 0 {
                line.column(&col, '\\');
            } else {
                line.column(&col, '|');
            }
            line.ch(' ');
        }
        self.pad(line);
        self.expansion_row += 1;
        if !self.needs_pre_commit_line() {
            self.set_state(State::Commit);
        }
    }

    fn draw_octopus_merge(&self, line: &mut Line) {
        let dashless = 2usize;
        let dashful = self.num_parents.saturating_sub(dashless);
        let added_cols = self.new_columns.len() as i64 - self.columns.len() as i64;
        let parent_in_old = self.num_parents as i64 - added_cols;
        let first_col = (self.commit_index as i64 + dashless as i64 - parent_in_old).max(0) as usize;
        for i in 0..dashful {
            line.ch('-');
            let c = if i == dashful - 1 { '.' } else { '-' };
            if let Some(col) = self.new_columns.get(i + first_col) {
                line.column(col, c);
            } else {
                line.ch(c);
            }
        }
    }

    fn output_commit(&mut self, line: &mut Line) {
        let mut seen_this = false;
        let num_columns = self.columns.len();
        for i in 0..=num_columns {
            let col_commit = if i == num_columns {
                if seen_this {
                    break;
                }
                self.commit
            } else {
                Some(self.columns[i].commit)
            };
            let col = self.columns.get(i).copied().unwrap_or(Column { commit: col_commit.unwrap_or(ChangesetId::from_bytes([0; 32])), color: self.current_color() });
            if col_commit == self.commit {
                seen_this = true;
                line.ch(self.commit_char());
                if self.num_parents > 2 {
                    self.draw_octopus_merge(line);
                }
            } else if seen_this && self.edges_added > 1 {
                line.column(&col, '\\');
            } else if seen_this && self.edges_added == 1 {
                // Right after a merge the lanes to its right still lean (observed in git 2.55).
                let lean = self.prev_state == State::PostMerge && self.prev_commit_index < i;
                line.column(&col, if lean { '\\' } else { '|' });
            } else if self.prev_state == State::Collapsing
                && self.old_mapping.get(2 * i + 1).copied() == Some(i as i32)
                && self.mapping.get(2 * i).is_some_and(|&m| m < i as i32)
            {
                line.column(&col, '/');
            } else {
                line.column(&col, '|');
            }
            line.ch(' ');
        }
        self.pad(line);
        if self.num_parents > 1 {
            self.set_state(State::PostMerge);
        } else if self.is_mapping_correct() {
            self.set_state(State::Padding);
        } else {
            self.set_state(State::Collapsing);
        }
    }

    fn output_post_merge(&mut self, line: &mut Line) {
        const MERGE_CHARS: [char; 3] = ['/', '|', '\\'];
        let mut seen_this = false;
        let parents: Vec<ChangesetId> = self.interesting().to_vec();
        let num_columns = self.columns.len();
        // The column that held the first parent: the "_" run to its right takes its colour.
        let mut parent_col: Option<Column> = None;
        for i in 0..=num_columns {
            let col_commit = if i == num_columns {
                if seen_this {
                    break;
                }
                self.commit
            } else {
                Some(self.columns[i].commit)
            };
            let col = self.columns.get(i).copied();
            if col_commit == self.commit {
                seen_this = true;
                let mut idx = self.merge_layout.max(0) as usize;
                for (n, p) in parents.iter().enumerate() {
                    let Some(pc) = self.find_new_column(p).and_then(|k| self.new_columns.get(k).copied()) else { continue };
                    line.column(&pc, MERGE_CHARS[idx]);
                    if idx == 2 {
                        if self.edges_added > 0 || n + 1 < parents.len() {
                            line.ch(' ');
                        }
                    } else {
                        idx += 1;
                    }
                }
                if self.edges_added == 0 {
                    line.ch(' ');
                }
            } else if seen_this {
                let c = col.unwrap_or(Column { commit: ChangesetId::from_bytes([0; 32]), color: COLORS_MAX });
                line.column(&c, if self.edges_added > 0 { '\\' } else { '|' });
                line.ch(' ');
            } else {
                let c = col.unwrap_or(Column { commit: ChangesetId::from_bytes([0; 32]), color: COLORS_MAX });
                line.column(&c, '|');
                if self.merge_layout != 0 || i + 1 != self.commit_index {
                    match &parent_col {
                        Some(pc) => line.column(pc, '_'),
                        None => line.ch(' '),
                    }
                }
            }
            if col_commit.is_some() && col_commit == parents.first().copied() {
                parent_col = col;
            }
        }
        self.pad(line);
        if self.is_mapping_correct() {
            self.set_state(State::Padding);
        } else {
            self.set_state(State::Collapsing);
        }
    }

    fn output_collapsing(&mut self, line: &mut Line) {
        let mut used_horizontal = false;
        let mut horizontal_edge: i64 = -1;
        let mut horizontal_edge_target: i64 = -1;
        std::mem::swap(&mut self.mapping, &mut self.old_mapping);
        let size = self.mapping_size;
        self.mapping.clear();
        self.mapping.resize(self.old_mapping.len().max(size + 4), -1);
        for i in 0..size {
            let target = self.old_mapping.get(i).copied().unwrap_or(-1);
            if target < 0 {
                continue;
            }
            let t = target as usize;
            if t * 2 == i {
                self.mapping[i] = target;
            } else if i > 0 && self.mapping[i - 1] < 0 {
                // Nothing to the left: move one to the left.
                self.mapping[i - 1] = target;
                if horizontal_edge == -1 {
                    horizontal_edge = i as i64;
                    horizontal_edge_target = i64::from(target);
                    let mut j = t * 2 + 3;
                    while j + 2 < i {
                        self.mapping[j] = target;
                        j += 2;
                    }
                }
            } else if i > 0 && self.mapping[i - 1] == target {
                // A branch line to the left already leads to the same parent.
            } else if i >= 2 {
                // Cross over the branch to the left.
                self.mapping[i - 2] = target;
                if horizontal_edge == -1 {
                    horizontal_edge = i as i64;
                }
            }
        }
        if self.mapping_size > 0 && self.mapping[self.mapping_size - 1] < 0 {
            self.mapping_size -= 1;
        }
        for i in 0..self.mapping_size {
            let target = self.mapping[i];
            let col = (target >= 0).then(|| self.new_columns.get(target as usize).copied()).flatten();
            let paint = |line: &mut Line, ch: char| match &col {
                Some(c) => line.column(c, ch),
                None => line.ch(ch),
            };
            if target < 0 {
                line.ch(' ');
            } else if target as usize * 2 == i {
                paint(line, '|');
            } else if i64::from(target) == horizontal_edge_target && i as i64 != horizontal_edge - 1 {
                // Only the first segment of a horizontal run continues into the next line.
                if i != target as usize * 2 + 3 {
                    self.mapping[i] = -1;
                }
                used_horizontal = true;
                paint(line, '_');
            } else {
                if used_horizontal && (i as i64) < horizontal_edge {
                    self.mapping[i] = -1;
                }
                paint(line, '/');
            }
        }
        self.pad(line);
        if self.is_mapping_correct() {
            self.set_state(State::Padding);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> ChangesetId {
        ChangesetId::from_bytes([n; 32])
    }

    /// Renders `commits` (id, parents) as `--graph --oneline` lines with the id as the text.
    fn render(commits: &[(u8, &[u8])]) -> Vec<String> {
        let mut g = Graph::new(false, false);
        let mut out = Vec::new();
        for (c, ps) in commits {
            g.update(id(*c), ps.iter().map(|p| id(*p)).collect(), false);
            loop {
                let (line, is_commit) = g.next_line();
                let mut s = String::from_utf8(line).unwrap();
                if is_commit {
                    s.push_str(&format!("c{c}"));
                }
                out.push(s);
                if is_commit {
                    break;
                }
            }
            while !g.is_commit_finished() {
                out.push(String::from_utf8(g.next_line().0).unwrap());
            }
        }
        out
    }

    #[test]
    fn linear_history_is_a_star_per_line() {
        assert_eq!(render(&[(3, &[2]), (2, &[1]), (1, &[])]), vec!["* c3", "* c2", "* c1"]);
    }

    #[test]
    fn a_merge_forks_and_collapses() {
        // 4 merges 3 and 2; both lead to 1.
        let lines = render(&[(4, &[3, 2]), (3, &[1]), (2, &[1]), (1, &[])]);
        assert_eq!(lines, vec!["*   c4", "|\\  ", "* | c3", "| * c2", "|/  ", "* c1"]);
    }

    #[test]
    fn an_octopus_collapses_its_lanes() {
        let lines = render(&[(5, &[4, 3, 1]), (1, &[0]), (3, &[2]), (2, &[0]), (4, &[0]), (0, &[])]);
        assert_eq!(
            lines,
            vec![
                "*-.   c5", "|\\ \\  ", "| | * c1", "| * | c3", "| * | c2", "| |/  ", "* / c4", "|/  ", "* c0"
            ]
        );
    }
}
