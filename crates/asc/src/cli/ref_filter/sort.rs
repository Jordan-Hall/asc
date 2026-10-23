//! `--sort=<key>`: git's `cmp_ref_sorting` (numeric for the numeric atoms, byte order for the
//! rest, `version:` for `versioncmp`), with the ref name as the last tie-break.

use std::cmp::Ordering;

use super::item::Item;
use super::parse::SortKey;
use super::value::{Env, Value, value};

const S_N: usize = 0;
const S_I: usize = 3;
const S_F: usize = 6;
const S_Z: usize = 9;
const CMP: i8 = 2;
const LEN: i8 = 3;

/// git's `versioncmp` (glibc's `strverscmp`): digit runs compare as numbers, a leading zero makes
/// a run fractional.
pub fn versioncmp(a: &[u8], b: &[u8]) -> Ordering {
    const NEXT: [usize; 12] = [S_N, S_I, S_Z, S_N, S_I, S_I, S_N, S_F, S_F, S_N, S_F, S_Z];
    const RESULT: [i8; 36] = [
        CMP, CMP, CMP, CMP, LEN, CMP, CMP, CMP, CMP, //
        CMP, -1, -1, 1, LEN, LEN, 1, LEN, LEN, //
        CMP, CMP, CMP, CMP, CMP, CMP, CMP, CMP, CMP, //
        CMP, 1, 1, -1, CMP, CMP, -1, CMP, CMP,
    ];
    let at = |s: &[u8], i: usize| s.get(i).copied().unwrap_or(0);
    let class = |c: u8| usize::from(c == b'0') + usize::from(c.is_ascii_digit());
    if a == b {
        return Ordering::Equal;
    }
    let (mut i, mut j) = (0, 0);
    let (mut c1, mut c2) = (at(a, 0), at(b, 0));
    i += 1;
    j += 1;
    let mut state = S_N + class(c1);
    while c1 == c2 {
        if c1 == 0 {
            return Ordering::Equal;
        }
        state = NEXT[state];
        c1 = at(a, i);
        c2 = at(b, j);
        i += 1;
        j += 1;
        state += class(c1);
    }
    let diff = i32::from(c1) - i32::from(c2);
    let verdict = RESULT[state * 3 + class(c2)];
    let order = |n: i32| n.cmp(&0);
    match verdict {
        CMP => order(diff),
        LEN => {
            // Both are inside digit runs: the longer run is the larger number.
            while at(a, i).is_ascii_digit() {
                i += 1;
                if !at(b, j).is_ascii_digit() {
                    return Ordering::Greater;
                }
                j += 1;
            }
            if at(b, j).is_ascii_digit() { Ordering::Less } else { order(diff) }
        }
        v => order(i32::from(v)),
    }
}

fn fold(bytes: &[u8]) -> Vec<u8> {
    bytes.to_ascii_lowercase()
}

fn compare_text(a: &[u8], b: &[u8], ignore_case: bool) -> Ordering {
    let cut = |s: &[u8]| s.iter().position(|&c| c == 0).map_or(s.len(), |p| p);
    let (a, b) = (&a[..cut(a)], &b[..cut(b)]);
    if ignore_case { fold(a).cmp(&fold(b)) } else { a.cmp(b) }
}

/// Sorts `items` by the keys (given in command-line order; the last one is the primary key).
pub fn sort(items: &mut Vec<Item>, keys: &[SortKey], env: &Env, ignore_case: bool) {
    let ordered: Vec<&SortKey> = keys.iter().rev().collect();
    let values: Vec<Vec<Value>> =
        items.iter().map(|item| ordered.iter().map(|k| value(&k.atom, item, env)).collect()).collect();
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|&x, &y| {
        for (n, key) in ordered.iter().enumerate() {
            let (a, b) = (&values[x][n], &values[y][n]);
            let cmp = if key.version {
                versioncmp(&a.text, &b.text)
            } else if key.atom.numeric() {
                a.num.cmp(&b.num)
            } else {
                compare_text(&a.text, &b.text, ignore_case)
            };
            let cmp = if key.reverse { cmp.reverse() } else { cmp };
            if cmp != Ordering::Equal {
                return cmp;
            }
        }
        compare_text(items[x].name.as_bytes(), items[y].name.as_bytes(), ignore_case)
    });
    let mut taken: Vec<Option<Item>> = std::mem::take(items).into_iter().map(Some).collect();
    items.extend(order.into_iter().filter_map(|i| taken[i].take()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &str, b: &str) -> Ordering {
        versioncmp(a.as_bytes(), b.as_bytes())
    }

    #[test]
    fn version_order() {
        assert_eq!(v("v1.2", "v1.10"), Ordering::Less);
        assert_eq!(v("v1.10", "v1.9"), Ordering::Greater);
        assert_eq!(v("a", "a"), Ordering::Equal);
        assert_eq!(v("1.01", "1.1"), Ordering::Less);
        assert_eq!(v("abc", "abd"), Ordering::Less);
        assert_eq!(v("file9", "file10"), Ordering::Less);
    }
}
