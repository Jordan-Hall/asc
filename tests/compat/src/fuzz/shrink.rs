//! Shrinking a failing sequence (PLAN Q04): remove steps while the difference persists.

use super::ops::Op;

/// The most `persists` calls one shrink makes; each is a full run on both sides.
pub const DEFAULT_BUDGET: usize = 40;

/// Removes steps from `ops` while `persists` says the same difference remains: first runs of
/// steps (halving the run length), then single steps, until a pass removes nothing or `budget`
/// calls are used. The result still satisfies `persists` if `ops` did.
pub fn shrink(mut ops: Vec<Op>, budget: usize, mut persists: impl FnMut(&[Op]) -> bool) -> Vec<Op> {
    let mut used = 0;
    let mut chunk = ops.len().div_ceil(2).max(1);
    loop {
        let mut removed_any = false;
        let mut start = 0;
        while start < ops.len() {
            if used >= budget {
                return ops;
            }
            let end = (start + chunk).min(ops.len());
            let mut trial = ops.clone();
            trial.drain(start..end);
            used += 1;
            if persists(&trial) {
                ops = trial;
                removed_any = true;
                // The next run now starts at `start` again.
            } else {
                start += chunk;
            }
        }
        if chunk == 1 {
            if !removed_any {
                return ops;
            }
        } else {
            chunk = chunk.div_ceil(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(n: usize) -> Op {
        Op {
            label: "write",
            sh: vec![format!("step {n}")],
            mutating: false,
        }
    }

    fn has(ops: &[Op], n: usize) -> bool {
        ops.iter().any(|o| o.sh[0] == format!("step {n}"))
    }

    #[test]
    fn shrinks_to_the_steps_the_difference_needs() {
        let ops: Vec<Op> = (0..30).map(op).collect();
        let mut calls = 0;
        let out = shrink(ops, 500, |t| {
            calls += 1;
            has(t, 7) && has(t, 19)
        });
        let kept: Vec<&str> = out.iter().map(|o| o.sh[0].as_str()).collect();
        assert_eq!(kept, ["step 7", "step 19"]);
        assert!(calls < 120, "{calls}");
    }

    #[test]
    fn stops_at_the_budget_and_keeps_a_failing_sequence() {
        let ops: Vec<Op> = (0..30).map(op).collect();
        let out = shrink(ops, 3, |t| has(t, 7));
        assert!(has(&out, 7));
        assert!(out.len() < 30);
    }

    #[test]
    fn a_sequence_that_cannot_shrink_is_returned_whole() {
        let ops: Vec<Op> = (0..5).map(op).collect();
        let out = shrink(ops.clone(), 100, |t| t.len() == 5);
        assert_eq!(out, ops);
    }
}
