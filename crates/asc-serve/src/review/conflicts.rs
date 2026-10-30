//! `Review.GetConflicts`: the conflicts of a change with its target, hunk by hunk, both sides.
//!
//! A change that conflicts with main stays open and nothing about it is written. The merge the
//! mergeability check runs keeps a diff3-style marked file for each text conflict; this call reads
//! it back and splits it into hunks (`main`, `base` and `change` lines, with the line each side
//! starts at), so a person can see both sides and resolve in an editor or hand the task to an
//! agent. The changes stacked on this one are listed as held back; every other change is
//! unaffected. Reading needs repo read, like the mergeability it repeats.

use asc_core::id::FileId;
use asc_rpc::server::{CallCtx, wire};
use tonic::Status;

use super::{Reviews, change_of, sql};
use crate::changes::{mergeable, versions};

/// Files returned at most.
pub const MAX_FILES: usize = 50;
/// Hunks per file.
pub const MAX_HUNKS: usize = 20;
/// Lines per side of a hunk.
pub const MAX_LINES: usize = 200;
/// Largest marked file read.
const MAX_MARKED: u64 = 1 << 20;
/// Changes listed as held back.
const MAX_HELD: i64 = 100;

/// The hunks of `text`, a file with diff3-style conflict markers, and whether some were left out.
pub fn parse_markers(text: &[u8]) -> (Vec<wire::ConflictHunk>, bool) {
    #[derive(PartialEq)]
    enum At {
        Outside,
        Main,
        Base,
        Change,
    }
    let marker = |line: &[u8], c: u8| {
        line.len() >= 7
            && line[..7].iter().all(|b| *b == c)
            && (line.len() == 7 || line[7] == b' ' || line[7] == b'\r')
    };
    let lines = |v: &[Vec<u8>]| -> Vec<String> {
        v.iter()
            .take(MAX_LINES)
            .map(|l| String::from_utf8_lossy(l).trim_end_matches('\r').to_owned())
            .collect()
    };
    let mut hunks = Vec::new();
    let mut truncated = false;
    let (mut main_line, mut change_line) = (0u32, 0u32);
    let mut main: Vec<Vec<u8>> = Vec::new();
    let mut base: Vec<Vec<u8>> = Vec::new();
    let mut change: Vec<Vec<u8>> = Vec::new();
    let (mut main_start, mut change_start) = (0u32, 0u32);
    let mut at = At::Outside;
    for raw in text.split(|b| *b == b'\n') {
        match at {
            At::Outside => {
                if marker(raw, b'<') {
                    at = At::Main;
                    main_start = main_line + 1;
                    change_start = change_line + 1;
                    main.clear();
                    base.clear();
                    change.clear();
                } else {
                    main_line += 1;
                    change_line += 1;
                }
            }
            At::Main if marker(raw, b'|') => at = At::Base,
            At::Main | At::Base if marker(raw, b'=') => at = At::Change,
            At::Main => main.push(raw.to_vec()),
            At::Base => base.push(raw.to_vec()),
            At::Change if marker(raw, b'>') => {
                if hunks.len() < MAX_HUNKS {
                    truncated |= main.len() > MAX_LINES
                        || base.len() > MAX_LINES
                        || change.len() > MAX_LINES;
                    hunks.push(wire::ConflictHunk {
                        main_start,
                        change_start,
                        main: lines(&main),
                        base: lines(&base),
                        change: lines(&change),
                    });
                } else {
                    truncated = true;
                }
                main_line += u32::try_from(main.len()).unwrap_or(u32::MAX);
                change_line += u32::try_from(change.len()).unwrap_or(u32::MAX);
                at = At::Outside;
            }
            At::Change => change.push(raw.to_vec()),
        }
    }
    // A marker block that never closed is not a hunk; say that something was left out.
    truncated |= at != At::Outside;
    (hunks, truncated)
}

impl Reviews {
    /// `Review.GetConflicts`.
    pub async fn get_conflicts(
        &self,
        ctx: &CallCtx,
        req: wire::GetChangeConflictsRequest,
    ) -> Result<wire::GetChangeConflictsResponse, Status> {
        let change = change_of(&req.change)?;
        if req.target.len() > 200 || req.target.chars().any(char::is_control) {
            return Err(Status::invalid_argument("invalid target branch"));
        }
        let entry = self.open(ctx).await?;
        let head = {
            let mut conn = self.registry.pool().acquire().await.map_err(sql)?;
            versions::latest(&mut conn, entry.ns, change)
                .await
                .map_err(|e| super::internal("versions", &e))?
                .ok_or_else(|| Status::not_found("unknown change"))?
        };
        let target = req
            .target
            .strip_prefix("refs/heads/")
            .unwrap_or(&req.target);
        let target = if target.is_empty() {
            entry.default_branch.as_str()
        } else {
            target
        };
        let tip =
            mergeable::trunk_tip(&self.registry, &entry, &format!("refs/heads/{target}")).await?;
        let engine = entry.backend.engine();
        let (state, found) = mergeable::compute(engine, head.cs, tip).await?;
        let mut out = wire::GetChangeConflictsResponse {
            state: match state {
                wire::mergeable::State::Conflicts => wire::ConflictCheck::Conflicts,
                wire::mergeable::State::NeedsRestack => wire::ConflictCheck::NeedsRestack,
                _ => wire::ConflictCheck::Clean,
            } as i32,
            ..Default::default()
        };
        for c in &found {
            if out.files.len() >= MAX_FILES {
                out.truncated = true;
                break;
            }
            let kind = c.kind;
            let mut file = wire::ConflictFile {
                path: c.path.clone(),
                kind,
                binary: kind == wire::ConflictKind::Binary as i32,
                ..Default::default()
            };
            let texty = kind == wire::ConflictKind::Content as i32
                || kind == wire::ConflictKind::AddAdd as i32;
            if texty && !file.binary && c.marked_id.len() == 32 {
                match <[u8; 32]>::try_from(c.marked_id.as_ref()).map(FileId::from_bytes) {
                    Ok(id) => match engine.read_file(id, MAX_MARKED).await {
                        Ok(bytes) => {
                            let (hunks, cut) = parse_markers(&bytes);
                            file.hunks = hunks;
                            file.truncated = cut;
                        }
                        // Larger than the bound or gone: the paths are still listed.
                        Err(_) => file.truncated = true,
                    },
                    Err(_) => file.truncated = true,
                }
            } else if texty {
                // A conflict the merge could not mark (binary content): pick a side.
                file.binary = true;
            }
            out.files.push(file);
        }
        let held: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT DISTINCT change FROM change_versions WHERE ns = $1 AND parent_change = $2 \
             AND change <> $2 ORDER BY change LIMIT $3",
        )
        .bind(entry.ns.as_bytes().to_vec())
        .bind(change.as_bytes().to_vec())
        .bind(MAX_HELD)
        .fetch_all(self.registry.pool())
        .await
        .map_err(sql)?;
        out.held_back = held.into_iter().map(bytes::Bytes::from).collect();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Shown<'a> = (u32, u32, Vec<&'a str>, Vec<&'a str>, Vec<&'a str>);

    fn text(hunks: &[wire::ConflictHunk]) -> Vec<Shown<'_>> {
        hunks
            .iter()
            .map(|h| {
                (
                    h.main_start,
                    h.change_start,
                    h.main.iter().map(String::as_str).collect(),
                    h.base.iter().map(String::as_str).collect(),
                    h.change.iter().map(String::as_str).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn a_marked_file_splits_into_hunks_with_the_line_each_side_starts_at() {
        let marked = b"a\n<<<<<<< ours\nB1\nB1b\n||||||| base\nB\n=======\nB2\n>>>>>>> theirs\nc\n<<<<<<< ours\nX\n=======\nY\nZ\n>>>>>>> theirs\n";
        let (hunks, cut) = parse_markers(marked);
        assert!(!cut);
        assert_eq!(
            text(&hunks),
            [
                (2, 2, vec!["B1", "B1b"], vec!["B"], vec!["B2"]),
                // After "a", two main lines, "c": main is at line 5 and the change, with one line, at 4.
                (5, 4, vec!["X"], vec![], vec!["Y", "Z"]),
            ]
        );
    }

    #[test]
    fn a_file_without_base_markers_and_crlf_lines_parse() {
        let (hunks, cut) =
            parse_markers(b"<<<<<<< ours\r\nx\r\n=======\r\ny\r\n>>>>>>> theirs\r\n");
        assert!(!cut);
        assert_eq!(text(&hunks), [(1, 1, vec!["x"], vec![], vec!["y"])]);
    }

    #[test]
    fn text_that_only_looks_like_a_marker_is_text() {
        let (hunks, cut) = parse_markers(b"<<<<<< six\n<<<<<<<x\n=======\n>>>>>>>\n");
        assert!(hunks.is_empty() && !cut, "{hunks:?}");
    }

    #[test]
    fn bounds_cut_lines_and_hunks_and_an_open_block_is_not_a_hunk() {
        let mut big = b"<<<<<<< ours\n".to_vec();
        for i in 0..(MAX_LINES + 5) {
            big.extend_from_slice(format!("l{i}\n").as_bytes());
        }
        big.extend_from_slice(b"=======\ny\n>>>>>>> theirs\n");
        let (hunks, cut) = parse_markers(&big);
        assert!(cut);
        assert_eq!(hunks[0].main.len(), MAX_LINES);
        let mut many = Vec::new();
        for _ in 0..(MAX_HUNKS + 3) {
            many.extend_from_slice(b"<<<<<<< a\nx\n=======\ny\n>>>>>>> b\n");
        }
        let (hunks, cut) = parse_markers(&many);
        assert!(cut);
        assert_eq!(hunks.len(), MAX_HUNKS);
        let (hunks, cut) = parse_markers(b"ok\n<<<<<<< a\nx\n=======\ny\n");
        assert!(hunks.is_empty() && cut);
    }
}
