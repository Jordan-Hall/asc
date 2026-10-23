//! The first-page header of `CommitGraphResponse` (SPEC §24.8.2 A2, PLAN P57): `total` and
//! `ref_markers[]` for the minimap of the IDE commit graph.
//!
//! Only the first page carries them, and only what is cheap to know: the markers name the refs
//! that label a row of the first page (`row` is that row's zero-based index in the page), and
//! `total` is the row count when the first page is also the last. When more pages follow, the
//! total is not known without walking them, so it stays 0 ("omitted") and the client shows the
//! pages it has loaded. Later pages never carry either field.

use std::collections::HashMap;
use std::hash::Hash;

use asc_rpc::server::wire;

/// Most markers one response carries.
pub const MAX_MARKERS: usize = 5_000;

/// The `total` and `ref_markers` of a first page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub total: u64,
    pub ref_markers: Vec<wire::RefMarker>,
}

/// The header for the first page whose rows are `rows` (in order); `has_next` says that another
/// page follows. `labels` holds the ref names of each row, as for `GraphRow.refs`.
pub fn first_page<K: Hash + Eq>(
    rows: &[K],
    labels: &HashMap<K, Vec<String>>,
    has_next: bool,
) -> Header {
    let mut ref_markers = Vec::new();
    'rows: for (i, k) in rows.iter().enumerate() {
        for name in labels.get(k).into_iter().flatten() {
            if ref_markers.len() == MAX_MARKERS {
                break 'rows;
            }
            ref_markers.push(wire::RefMarker {
                name: name.clone(),
                row: i as u64,
            });
        }
    }
    Header {
        total: if has_next { 0 } else { rows.len() as u64 },
        ref_markers,
    }
}

/// Puts `header` on `msg`, the first message of a first page.
pub fn apply(msg: &mut wire::CommitGraphResponse, header: Header) {
    msg.total = header.total;
    msg.ref_markers = header.ref_markers;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(v: &[(u32, &[&str])]) -> HashMap<u32, Vec<String>> {
        v.iter()
            .map(|(k, n)| (*k, n.iter().map(|s| (*s).to_owned()).collect()))
            .collect()
    }

    #[test]
    fn markers_name_rows_by_index() {
        let l = labels(&[(7, &["master", "v1"]), (9, &["feat"])]);
        let h = first_page(&[9u32, 8, 7], &l, false);
        assert_eq!(h.total, 3);
        let got: Vec<_> = h
            .ref_markers
            .iter()
            .map(|m| (m.name.as_str(), m.row))
            .collect();
        assert_eq!(got, [("feat", 0), ("master", 2), ("v1", 2)]);
    }

    #[test]
    fn the_total_is_omitted_when_more_pages_follow() {
        let l = labels(&[(1, &["master"])]);
        let h = first_page(&[1u32, 2], &l, true);
        assert_eq!(h.total, 0);
        assert_eq!(h.ref_markers.len(), 1);
    }

    #[test]
    fn an_empty_page_has_an_empty_header() {
        assert_eq!(
            first_page::<u32>(&[], &HashMap::new(), false),
            Header::default()
        );
    }

    #[test]
    fn markers_are_capped() {
        let rows: Vec<u32> = (0..10).collect();
        let l: HashMap<u32, Vec<String>> = rows
            .iter()
            .map(|&k| (k, (0..MAX_MARKERS).map(|i| format!("r{i}")).collect()))
            .collect();
        assert_eq!(first_page(&rows, &l, false).ref_markers.len(), MAX_MARKERS);
    }

    #[test]
    fn apply_sets_both_fields() {
        let mut m = wire::CommitGraphResponse::default();
        apply(
            &mut m,
            Header {
                total: 4,
                ref_markers: vec![wire::RefMarker {
                    name: "a".into(),
                    row: 1,
                }],
            },
        );
        assert_eq!((m.total, m.ref_markers.len()), (4, 1));
    }
}
