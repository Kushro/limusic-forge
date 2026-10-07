//! The fewest moves that turn one playlist order into another.
//!
//! YouTube reorders a playlist one row at a time (`ACTION_MOVE_VIDEO_BEFORE`: put this row before
//! that one). Rows that already sit in the right order relative to each other never need to move,
//! and the largest such set is a longest increasing subsequence of their target positions. Every
//! other row moves exactly once. Ported from PlaylistForge's `pf-core/src/algo.rs`.

use std::collections::{HashMap, HashSet};

/// Indices of one longest strictly increasing subsequence of `seq`, ascending. O(n log n).
pub fn lis_indices(seq: &[usize]) -> Vec<usize> {
    // tails[k]: index into `seq` of the smallest tail of an increasing run of length k + 1.
    let mut tails: Vec<usize> = Vec::new();
    let mut prev: Vec<Option<usize>> = vec![None; seq.len()];
    for (i, &v) in seq.iter().enumerate() {
        let k = tails.partition_point(|&t| seq[t] < v);
        if k > 0 {
            prev[i] = Some(tails[k - 1]);
        }
        if k == tails.len() {
            tails.push(i);
        } else {
            tails[k] = i;
        }
    }
    let mut out = Vec::with_capacity(tails.len());
    let mut at = tails.last().copied();
    while let Some(i) = at {
        out.push(i);
        at = prev[i];
    }
    out.reverse();
    out
}

/// One `ACTION_MOVE_VIDEO_BEFORE`: `row` goes directly before `before`, or last on `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Move {
    pub row: String,
    pub before: Option<String>,
}

/// The moves, applied in order, that turn `current` into `target`. Both are row handles
/// (`setVideoId`s). Rows `target` doesn't name keep their order and follow the named ones (as
/// `Db::reorder_local_playlist` does); a name `current` doesn't hold is ignored, as is a repeat.
/// So an order built from a list that changed meanwhile still applies instead of failing.
///
/// Built back to front: each row that has to move goes directly before the row that follows it in
/// `target`, which by then already sits right. A row that stays is in order with everything after
/// it by construction (that is what the subsequence means), so the whole list ends up in order.
pub fn reorder_moves(current: &[String], target: &[String]) -> Vec<Move> {
    let present: HashSet<&str> = current.iter().map(String::as_str).collect();
    let mut seen = HashSet::new();
    let mut target: Vec<&str> = target
        .iter()
        .map(String::as_str)
        .filter(|r| present.contains(r) && seen.insert(*r))
        .collect();
    let named: HashSet<&str> = target.iter().copied().collect();
    target.extend(current.iter().map(String::as_str).filter(|r| !named.contains(r)));
    let pos: HashMap<&str, usize> = target.iter().enumerate().map(|(i, r)| (*r, i)).collect();
    // Target positions in current order, for the rows both lists hold.
    let seq: Vec<usize> = current.iter().filter_map(|r| pos.get(r.as_str()).copied()).collect();
    let keep: HashSet<usize> = lis_indices(&seq).into_iter().map(|i| seq[i]).collect();
    let mut moves = Vec::with_capacity(target.len() - keep.len());
    for i in (0..target.len()).rev() {
        if !keep.contains(&i) {
            moves.push(Move {
                row: target[i].to_owned(),
                before: target.get(i + 1).map(|s| (*s).to_owned()),
            });
        }
    }
    moves
}

/// What `moves` do to `list`: the same semantics YouTube applies, for tests and for the undo
/// journal, which needs to know the order an edit left behind without reading it back.
pub fn apply_moves(list: &mut Vec<String>, moves: &[Move]) {
    for m in moves {
        let Some(from) = list.iter().position(|r| *r == m.row) else { continue };
        let row = list.remove(from);
        let to = m.before.as_ref().and_then(|b| list.iter().position(|r| r == b));
        match to {
            Some(at) => list.insert(at, row),
            None => list.push(row),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(s: &str) -> Vec<String> {
        s.chars().map(|c| c.to_string()).collect()
    }

    #[test]
    fn lis_finds_a_longest_run() {
        assert_eq!(lis_indices(&[]), Vec::<usize>::new());
        assert_eq!(lis_indices(&[3, 1, 2]), vec![1, 2]);
        let seq = [5, 1, 6, 2, 3, 0, 4];
        let run: Vec<usize> = lis_indices(&seq).into_iter().map(|i| seq[i]).collect();
        assert_eq!(run, vec![1, 2, 3, 4]);
    }

    #[test]
    fn identical_orders_need_no_moves() {
        assert!(reorder_moves(&rows("abcde"), &rows("abcde")).is_empty());
    }

    #[test]
    fn one_row_dragged_is_one_move() {
        // e dragged to the top.
        let m = reorder_moves(&rows("abcde"), &rows("eabcd"));
        assert_eq!(m, vec![Move { row: "e".into(), before: Some("a".into()) }]);
        // a dragged to the bottom: no successor, so it goes last.
        let m = reorder_moves(&rows("abcde"), &rows("bcdea"));
        assert_eq!(m, vec![Move { row: "a".into(), before: None }]);
    }

    #[test]
    fn moves_reach_the_target_and_are_minimal() {
        let cases = [
            ("abcdefgh", "hgfedcba"),
            ("abcdefgh", "badcfehg"),
            ("abcdefgh", "cdefghab"),
            ("abcdefgh", "aebfcgdh"),
            ("abcdefgh", "abcdefgh"),
        ];
        for (from, to) in cases {
            let (cur, tgt) = (rows(from), rows(to));
            let moves = reorder_moves(&cur, &tgt);
            let mut list = cur.clone();
            apply_moves(&mut list, &moves);
            assert_eq!(list, tgt, "{from} -> {to}");
            let pos: Vec<usize> =
                cur.iter().map(|r| tgt.iter().position(|t| t == r).unwrap()).collect();
            assert_eq!(moves.len(), cur.len() - lis_indices(&pos).len(), "{from} -> {to}");
        }
    }

    #[test]
    fn a_stale_target_applies_as_far_as_it_can() {
        // x is gone from the playlist, d is new since the order was built, b is named twice.
        let cur = rows("abcd");
        let tgt = rows("cxbab");
        let mut list = cur.clone();
        apply_moves(&mut list, &reorder_moves(&cur, &tgt));
        // c, b, a in the asked order; d, which the target never named, follows them.
        assert_eq!(list, rows("cbad"));
    }
}
