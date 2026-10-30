//! Why a search result matched (`MatchReason`, docs/SPEC.md 23.4.1).
//!
//! Everything here works on the text the renderer already sent, after redaction: a redacted file,
//! line or chunk has no text, so it has no spans and nothing about its content reaches the reason.

use std::collections::HashMap;

use asc_rpc::server::wire;

/// At most this many spans in one reason.
pub const MAX_SPANS: usize = 32;
/// At most this many terms in one reason.
pub const MAX_TERMS: usize = 16;

/// The byte ranges the executor reported for each line of each file, keyed by file id and line:
/// the renderer keeps only the starts, and a reason needs the ends.
pub type RawSpans = HashMap<([u8; 32], u32), Vec<(u32, u32)>>;

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The span `[start, end)` (0-based bytes) of `text` is not inside a longer identifier.
fn whole_word(text: &[u8], start: usize, end: usize) -> bool {
    let before = start == 0 || !is_ident(text[start - 1]);
    let after = end >= text.len() || !is_ident(text[end]);
    before && after
}

/// The reason of one `Search` hit. `pattern` is the request's pattern and `regex` says it is one.
/// Kind `TEXT`; the score is `0.4 + 0.3 * [some span is a whole word] + 0.3 * (1 - 1 / (1 + matched
/// lines))`, so it is in `[0.4, 1)` for a visible hit and `0.4` for a redacted file.
pub fn file_reason(
    pattern: &str,
    regex: bool,
    file: &wire::FileHit,
    raw: &RawSpans,
) -> wire::MatchReason {
    let mut spans = Vec::new();
    let mut lines = 0u32;
    let mut word = false;
    if !file.redacted {
        let id = <[u8; 32]>::try_from(&file.file_id[..]).ok();
        for m in file.matches.iter().filter(|m| !m.cols.is_empty()) {
            lines += 1;
            let known = id.and_then(|id| raw.get(&(id, m.line)));
            for &col in &m.cols {
                let start = col.saturating_sub(1) as usize;
                let len = known
                    .and_then(|v| v.iter().find(|(s, _)| *s as usize == start))
                    .map(|(s, e)| e.saturating_sub(*s) as usize)
                    .unwrap_or(if regex { 1 } else { pattern.len() })
                    .max(1);
                let end = (start + len).min(m.text.len().max(start + 1));
                word |= start < m.text.len() && whole_word(&m.text, start, end);
                if spans.len() < MAX_SPANS {
                    spans.push(wire::MatchSpan {
                        line: m.line,
                        start_col: col,
                        end_col: u32::try_from(end + 1).unwrap_or(u32::MAX),
                        term: pattern.to_owned(),
                    });
                }
            }
        }
    }
    let score =
        0.4 + 0.3 * f64::from(u8::from(word)) + 0.3 * (1.0 - 1.0 / (1.0 + f64::from(lines)));
    wire::MatchReason {
        kind: wire::MatchKind::Text as i32,
        score,
        terms: if spans.is_empty() {
            Vec::new()
        } else {
            vec![pattern.to_owned()]
        },
        spans,
    }
}

/// The identifier-like words of a query, lower case, two bytes or longer, in order and distinct.
pub fn query_terms(query: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in query.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        let w = w.to_ascii_lowercase();
        if w.len() >= 2 && !out.contains(&w) {
            out.push(w);
            if out.len() >= MAX_TERMS {
                break;
            }
        }
    }
    out
}

/// The kind of a `Semantic` chunk from the sources that found it: `MEANING` when the vector list
/// had it, else `SYMBOL` when the symbol list did, else `TEXT` (BM25 only).
pub fn chunk_kind(sources: &[String]) -> wire::MatchKind {
    let has = |s: &str| sources.iter().any(|x| x == s);
    if has("vector") {
        wire::MatchKind::Meaning
    } else if has("symbol") {
        wire::MatchKind::Symbol
    } else {
        wire::MatchKind::Text
    }
}

/// The reason of one `Semantic` chunk. `terms` are the query's words (see [`query_terms`]); `top`
/// is the largest chunk score of the response, so the relevance is the score relative to the best.
pub fn chunk_reason(terms: &[String], top: f64, chunk: &wire::Chunk) -> wire::MatchReason {
    let score = if top > 0.0 && chunk.score.is_finite() {
        (chunk.score / top).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let mut spans: Vec<wire::MatchSpan> = Vec::new();
    let mut matched: Vec<String> = Vec::new();
    if !chunk.redacted {
        let lower = chunk.text.to_ascii_lowercase();
        // Byte offset of the start of each line, so an offset maps to a line and a column.
        let mut starts = vec![0usize];
        starts.extend(
            lower
                .iter()
                .enumerate()
                .filter(|(_, b)| **b == b'\n')
                .map(|(i, _)| i + 1),
        );
        for term in terms {
            let t = term.as_bytes();
            let mut found = false;
            let mut at = 0usize;
            while let Some(p) = lower
                .get(at..)
                .and_then(|rest| rest.windows(t.len()).position(|w| w == t))
                .map(|p| p + at)
            {
                at = p + 1;
                if !whole_word(&lower, p, p + t.len()) {
                    continue;
                }
                found = true;
                let row = starts.partition_point(|s| *s <= p) - 1;
                let col = p - starts[row] + 1;
                spans.push(wire::MatchSpan {
                    line: chunk
                        .start_line
                        .saturating_add(u32::try_from(row).unwrap_or(u32::MAX)),
                    start_col: u32::try_from(col).unwrap_or(u32::MAX),
                    end_col: u32::try_from(col + t.len()).unwrap_or(u32::MAX),
                    term: term.clone(),
                });
            }
            if found {
                matched.push(term.clone());
            }
        }
        spans.sort_by_key(|a| (a.line, a.start_col));
        spans.truncate(MAX_SPANS);
    }
    wire::MatchReason {
        kind: chunk_kind(&chunk.sources) as i32,
        score,
        spans,
        terms: matched,
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn hit(redacted: bool, line: u32, text: &str, cols: &[u32]) -> wire::FileHit {
        wire::FileHit {
            file_id: Bytes::from(vec![7u8; 32]),
            redacted,
            matches: if redacted {
                Vec::new()
            } else {
                vec![wire::LineMatch {
                    line,
                    cols: cols.to_vec(),
                    text: Bytes::from(text.to_owned()),
                    ..Default::default()
                }]
            },
            ..Default::default()
        }
    }

    #[test]
    fn a_text_hit_names_its_spans_and_scores_a_whole_word_higher() {
        let word = file_reason(
            "foo",
            false,
            &hit(false, 3, "let foo = 1;", &[5]),
            &HashMap::new(),
        );
        assert_eq!(word.kind, wire::MatchKind::Text as i32);
        assert_eq!(word.terms, ["foo"]);
        let s = &word.spans[0];
        assert_eq!(
            (s.line, s.start_col, s.end_col, s.term.as_str()),
            (3, 5, 8, "foo")
        );
        let inside = file_reason(
            "foo",
            false,
            &hit(false, 3, "let foobar = 1;", &[5]),
            &HashMap::new(),
        );
        assert!(word.score > inside.score);
        for r in [&word, &inside] {
            assert!((0.0..=1.0).contains(&r.score));
        }
    }

    #[test]
    fn a_regex_span_takes_its_length_from_the_executor() {
        let mut raw = RawSpans::new();
        raw.insert(([7u8; 32], 1), vec![(4, 11)]);
        let r = file_reason("fo+", true, &hit(false, 1, "let fooooo = 1;", &[5]), &raw);
        assert_eq!((r.spans[0].start_col, r.spans[0].end_col), (5, 12));
        let guessed = file_reason(
            "fo+",
            true,
            &hit(false, 1, "let fooooo = 1;", &[5]),
            &HashMap::new(),
        );
        assert_eq!(guessed.spans[0].end_col, 6);
    }

    #[test]
    fn more_matched_lines_score_higher_and_a_redacted_file_has_no_spans() {
        let one = file_reason("a", false, &hit(false, 1, "a", &[1]), &HashMap::new());
        let mut two = hit(false, 1, "a", &[1]);
        two.matches.push(two.matches[0].clone());
        let two = file_reason("a", false, &two, &HashMap::new());
        assert!(two.score > one.score);
        let red = file_reason("a", false, &hit(true, 1, "a", &[1]), &HashMap::new());
        assert!(red.spans.is_empty() && red.terms.is_empty());
        assert!((red.score - 0.4).abs() < 1e-9);
    }

    fn chunk(text: &str, score: f64, sources: &[&str], redacted: bool) -> wire::Chunk {
        wire::Chunk {
            start_line: 10,
            end_line: 12,
            score,
            sources: sources.iter().map(|s| (*s).to_owned()).collect(),
            text: Bytes::from(text.to_owned()),
            redacted,
            ..Default::default()
        }
    }

    #[test]
    fn query_terms_are_distinct_lower_case_words() {
        assert_eq!(
            query_terms("Parse the parse_Config, a b"),
            ["parse", "the", "parse_config"]
        );
        assert_eq!(query_terms("x ? !").len(), 0);
        assert_eq!(query_terms(&"word ".repeat(40)).len(), 1);
    }

    #[test]
    fn a_chunk_reason_finds_whole_words_on_the_right_lines() {
        let terms = query_terms("parse config");
        let c = chunk(
            "fn parse(c: Config) {\n    reparse(c)\n    PARSE\n}",
            0.5,
            &["vector", "bm25"],
            false,
        );
        let r = chunk_reason(&terms, 1.0, &c);
        assert_eq!(r.kind, wire::MatchKind::Meaning as i32);
        assert!((r.score - 0.5).abs() < 1e-9);
        assert_eq!(r.terms, ["parse", "config"]);
        let at: Vec<(u32, u32, u32, &str)> = r
            .spans
            .iter()
            .map(|s| (s.line, s.start_col, s.end_col, s.term.as_str()))
            .collect();
        assert_eq!(
            at,
            [
                (10, 4, 9, "parse"),
                (10, 13, 19, "config"),
                (12, 5, 10, "parse")
            ]
        );
    }

    #[test]
    fn a_chunk_reason_has_its_kind_from_its_sources_and_hides_redacted_text() {
        let terms = query_terms("parse");
        for (src, kind) in [
            (vec!["bm25"], wire::MatchKind::Text),
            (vec!["bm25", "symbol"], wire::MatchKind::Symbol),
            (vec!["symbol", "vector"], wire::MatchKind::Meaning),
        ] {
            assert_eq!(
                chunk_reason(&terms, 1.0, &chunk("parse", 1.0, &src, false)).kind,
                kind as i32
            );
        }
        let r = chunk_reason(&terms, 1.0, &chunk("parse", 1.0, &["vector"], true));
        assert!(r.spans.is_empty() && r.terms.is_empty());
        assert_eq!(
            chunk_reason(&terms, 0.0, &chunk("x", 3.0, &[], false)).score,
            0.0
        );
    }
}
