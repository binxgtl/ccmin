//! Shape-agnostic reduction primitives, shared by the built-in models and the
//! schema-driven one.
//!
//! Nothing here knows what an input *means*; callers supply a predicate and are
//! responsible for only proposing candidates that are structurally legal.

/// Classic ddmin: try removing progressively finer chunks, restarting at a
/// coarser granularity whenever a removal sticks.
pub fn ddmin<T: Clone>(items: &[T], accept: impl FnMut(&[T]) -> bool) -> Vec<T> {
    ddmin_min_len(items, 1, accept)
}

/// ddmin that refuses to go below `min_len` items.
///
/// This is how a declared constraint such as `int N in 1..100` reaches the
/// structural pass: an array whose length field cannot legally be zero must
/// never be emptied, however tempting the oracle finds it.
pub fn ddmin_min_len<T: Clone>(
    items: &[T],
    min_len: usize,
    mut accept: impl FnMut(&[T]) -> bool,
) -> Vec<T> {
    let mut cur = items.to_vec();
    if cur.is_empty() {
        return cur;
    }
    let mut n = 2usize;
    while cur.len() >= 2 && cur.len() > min_len {
        let chunk = cur.len().div_ceil(n);
        let mut reduced = false;
        let mut start = 0usize;
        while start < cur.len() {
            let end = (start + chunk).min(cur.len());
            if cur.len() - (end - start) < min_len {
                start = end;
                continue;
            }
            let mut cand = Vec::with_capacity(cur.len() - (end - start));
            cand.extend_from_slice(&cur[..start]);
            cand.extend_from_slice(&cur[end..]);
            if accept(&cand) {
                cur = cand;
                n = n.saturating_sub(1).max(2);
                reduced = true;
                break;
            }
            start = end;
        }
        if !reduced {
            if n >= cur.len() {
                break;
            }
            n = (n * 2).min(cur.len());
        }
    }
    cur
}

/// ddmin honouring a declared floor: it will empty the list when the schema
/// allows a count of zero, and stop at `min_len` when it does not.
pub fn ddmin_floor<T: Clone>(
    items: &[T],
    min_len: usize,
    accept: impl FnMut(&[T]) -> bool,
) -> Vec<T> {
    if min_len == 0 {
        ddmin_allow_empty(items, accept)
    } else {
        ddmin_min_len(items, min_len, accept)
    }
}

/// ddmin that will also try discarding the final item.
pub fn ddmin_allow_empty<T: Clone>(items: &[T], mut accept: impl FnMut(&[T]) -> bool) -> Vec<T> {
    let current = ddmin(items, &mut accept);
    if !current.is_empty() && accept(&[]) {
        Vec::new()
    } else {
        current
    }
}

pub fn shrink_ints(vals: &[i64], accept: impl FnMut(&[i64]) -> bool) -> Vec<i64> {
    shrink_ints_toward(vals, 0, accept)
}

/// Pull every element toward `target`, repeating until a round changes nothing.
/// How a run of alternating passes ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fixpoint {
    /// A whole round changed nothing. This is the normal ending.
    Reached,
    /// The defensive budget ran out first, so the result is only partly
    /// reduced. The caller is expected to say so rather than pass it off as
    /// finished.
    Exhausted,
}

/// Run `round` until a whole round changes nothing.
///
/// Termination is by fixed point, not by the budget, and that distinction is
/// the point of this function. Every accepted edit strictly decreases the pair
/// (number of data elements, total distance from each value to its target):
/// deletion lowers the first and leaves the second no higher, and a value step
/// lowers the second and leaves the first alone. Both are non-negative
/// integers, so no-change is always reached and the budget is not needed for
/// termination.
///
/// The budget only stops a pass that violates that contract from hanging a
/// CLI. Hitting it means a bug here, not a large input, so it is reported
/// instead of being folded into a normal-looking result -- an earlier fixed
/// cap of sixteen silently returned partly reduced output on inputs that
/// needed more rounds.
pub fn to_fixed_point<T, E>(
    start: T,
    budget: usize,
    mut round: impl FnMut(&T) -> Result<T, E>,
) -> Result<(T, Fixpoint), E>
where
    T: Clone + PartialEq,
{
    let mut best = start;
    for _ in 0..budget {
        let next = round(&best)?;
        if next == best {
            return Ok((best, Fixpoint::Reached));
        }
        best = next;
    }
    Ok((best, Fixpoint::Exhausted))
}

/// A budget generous enough that reaching it means a bug, scaled by the only
/// quantity that bounds productive rounds: each round must delete at least one
/// element or shrink at least one value.
pub fn fixed_point_budget(size: usize) -> usize {
    4 * size + 64
}

pub fn shrink_ints_toward(
    vals: &[i64],
    target: i64,
    mut accept: impl FnMut(&[i64]) -> bool,
) -> Vec<i64> {
    let mut cur = vals.to_vec();
    let mut improved = true;
    let mut rounds = 0;
    while improved && rounds < 16 {
        improved = false;
        rounds += 1;
        for i in 0..cur.len() {
            let original = cur[i];
            let candidate = shrink_value_toward(original, target, |cand| {
                let mut next = cur.clone();
                next[i] = cand;
                accept(&next)
            });
            if candidate != original {
                cur[i] = candidate;
                improved = true;
            }
        }
    }
    cur
}

/// Find the smallest accepted magnitude between zero and `x`.
pub fn shrink_value(x: i64, accept: impl FnMut(i64) -> bool) -> i64 {
    shrink_value_toward(x, 0, accept)
}

/// Find the accepted value closest to `target` on the interval between `target`
/// and `x`.
///
/// `target` is where the caller would like the value to end up: zero for an
/// unconstrained integer, or the in-range value nearest zero when the schema
/// declares bounds. The predicate is expected to have a boundary along the
/// interval, which is the usual shape of numeric bugs (`x >= limit`, overflow
/// thresholds, negative bounds). The returned value is always one the predicate
/// actually accepted.
///
/// Arithmetic is done in `i128` so a full-domain interval such as
/// `i64::MIN ..= i64::MAX` cannot overflow.
pub fn shrink_value_toward(x: i64, target: i64, mut accept: impl FnMut(i64) -> bool) -> i64 {
    if x == target {
        return x;
    }
    if accept(target) {
        return target;
    }

    let span = (x as i128 - target as i128).abs();
    let step: i128 = if x > target { 1 } else { -1 };
    let at = |offset: i128| -> i64 { (target as i128 + step * offset) as i64 };

    // One step away from the target: the analogue of trying +/-1 before a full
    // search, and the common answer for sign-sensitive bugs.
    if span > 1 && accept(at(1)) {
        return at(1);
    }

    let mut low = 1i128;
    let mut high = span;
    let mut best = x;
    while low < high {
        let mid = low + (high - low) / 2;
        let candidate = at(mid);
        if accept(candidate) {
            best = candidate;
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    if low < span {
        let candidate = at(low);
        if accept(candidate) {
            best = candidate;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 7. A first round that changes nothing ends it, with no further calls.
    #[test]
    fn a_round_that_changes_nothing_terminates() {
        let mut calls = 0;
        let out = to_fixed_point(5i32, 100, |v| {
            calls += 1;
            Ok::<_, ()>(*v)
        });
        assert_eq!(out, Ok((5, Fixpoint::Reached)));
        assert_eq!(calls, 1, "no round runs after the fixed point is seen");
    }

    /// The budget is a backstop, not the terminator: a chain far longer than
    /// any fixed cap still finishes on its own.
    #[test]
    fn a_long_chain_ends_by_fixed_point_not_by_budget() {
        let mut rounds = 0;
        let out = to_fixed_point(500i32, fixed_point_budget(500), |v| {
            rounds += 1;
            Ok::<_, ()>((*v - 1).max(0))
        });
        assert_eq!(out, Ok((0, Fixpoint::Reached)));
        assert_eq!(rounds, 501, "500 steps down, then one no-op round");
    }

    /// 8. When the budget does run out, the caller is told rather than handed
    ///    a partial result that looks finished.
    #[test]
    fn an_exhausted_budget_is_reported() {
        let out = to_fixed_point(500i32, 10, |v| Ok::<_, ()>((*v - 1).max(0)));
        assert_eq!(out, Ok((490, Fixpoint::Exhausted)));

        // A budget of zero runs nothing at all and still reports honestly.
        assert_eq!(
            to_fixed_point(7i32, 0, |v| Ok::<_, ()>(*v)),
            Ok((7, Fixpoint::Exhausted))
        );
    }

    /// An error from a pass stops the run and propagates unchanged.
    #[test]
    fn a_failing_round_propagates_its_error() {
        let out = to_fixed_point(3i32, 100, |v| match v {
            3 => Ok(2),
            _ => Err("oracle died"),
        });
        assert_eq!(out, Err("oracle died"));
    }

    /// The budget scales with the input, because the number of productive
    /// rounds does: each one deletes an element or shrinks a value.
    #[test]
    fn the_budget_scales_with_the_input() {
        assert!(fixed_point_budget(1000) > fixed_point_budget(10));
        assert!(fixed_point_budget(0) >= 64, "a tiny input still gets slack");
    }

    #[test]
    fn ddmin_finds_single_required_element() {
        let mut items: Vec<i64> = (0..64).collect();
        items[30] = 42;
        let out = ddmin(&items, |c| c.contains(&42));
        assert_eq!(out, vec![42]);
    }

    #[test]
    fn ddmin_keeps_two_required_elements() {
        let items: Vec<i64> = (0..32).collect();
        let out = ddmin(&items, |c| c.contains(&5) && c.contains(&20));
        assert_eq!(out, vec![5, 20]);
    }

    #[test]
    fn ddmin_respects_a_minimum_length() {
        let items: Vec<i64> = (0..32).collect();
        // The oracle would happily accept a single element; the floor must win.
        let out = ddmin_min_len(&items, 4, |_| true);
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn ddmin_allow_empty_can_remove_the_final_item() {
        let out = ddmin_allow_empty(&[42], |_| true);
        assert!(out.is_empty());
    }

    #[test]
    fn boundary_search_reaches_large_threshold_in_logarithmic_calls() {
        let mut calls = 0;
        let out = shrink_value(1_000_000_000_000_000_000, |candidate| {
            calls += 1;
            candidate >= 1_000_000_000
        });
        assert_eq!(out, 1_000_000_000);
        assert!(calls <= 66, "used {calls} predicate calls");
    }

    #[test]
    fn boundary_search_handles_i64_min_without_overflow() {
        let out = shrink_value(i64::MIN, |candidate| candidate <= -1_000_000_000);
        assert_eq!(out, -1_000_000_000);
    }

    #[test]
    fn boundary_search_spanning_the_whole_domain_does_not_overflow() {
        let out = shrink_value_toward(i64::MAX, i64::MIN, |candidate| candidate >= 0);
        assert_eq!(out, 0);
    }

    #[test]
    fn shrink_ints_pulls_to_minimum() {
        let out = shrink_ints(&[500, -900_000, 12], |c| c.iter().any(|v| *v < 0));
        assert_eq!(out, vec![0, -1, 0]);
    }

    #[test]
    fn shrinking_toward_a_lower_bound_never_goes_below_it() {
        // Declared `1 <= a_i`, so 0 is not a legal value however much the
        // oracle would accept it.
        let out = shrink_ints_toward(&[900_000, 4], 1, |_| true);
        assert_eq!(out, vec![1, 1]);
    }

    #[test]
    fn shrinking_toward_a_target_stops_at_the_boundary() {
        let out = shrink_value_toward(1_000_000, 1, |candidate| candidate >= 500);
        assert_eq!(out, 500);
    }
}
