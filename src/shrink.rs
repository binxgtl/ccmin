//! Delta debugging over the parsed model.
//!
//! Two alternating passes, run to a fixpoint:
//!   1. structural — delete elements (and whole test cases) via ddmin
//!   2. value      — pull each remaining integer toward zero
//!
//! Every candidate is rendered from the model, so the declared length prefix
//! always matches the data. See `model.rs` for why that matters.

use crate::model::{ArrayCase, GraphCase, Model};
use crate::oracle::{FailKind, Oracle};

pub struct Shrinker<'a> {
    oracle: &'a mut Oracle,
    target: FailKind,
    on_step: &'a mut dyn FnMut(&Model),
    error: Option<String>,
}

impl<'a> Shrinker<'a> {
    pub fn new(
        oracle: &'a mut Oracle,
        target: FailKind,
        on_step: &'a mut dyn FnMut(&Model),
    ) -> Self {
        Shrinker {
            oracle,
            target,
            on_step,
            error: None,
        }
    }

    fn accept(&mut self, m: &Model) -> bool {
        if self.error.is_some() {
            return false;
        }
        let ok = match self.oracle.preserves(&m.render(), self.target) {
            Ok(ok) => ok,
            Err(e) => {
                self.error = Some(e.to_string());
                false
            }
        };
        if ok {
            (self.on_step)(m);
        }
        ok
    }

    pub fn run(&mut self, start: &Model) -> Result<Model, String> {
        let mut best = start.clone();
        // Structural and value passes can unlock one another. Keep a generous
        // safety bound, while still stopping immediately at a fixpoint.
        for _ in 0..16 {
            let before = best.clone();
            best = self.structural(&best);
            best = self.values(&best);
            if let Some(error) = self.error.take() {
                return Err(error);
            }
            if best == before {
                break;
            }
        }
        Ok(best)
    }

    // ---- structural ------------------------------------------------------

    fn structural(&mut self, m: &Model) -> Model {
        match m {
            Model::Array(c) => {
                let arr = self.ddmin_arr(c, &c.arr.clone());
                Model::Array(c.with_arr(arr))
            }
            Model::MultiTest(tests) => {
                // First drop whole test cases, then shrink the survivors.
                let kept = self.ddmin_tests(tests);
                let mut out = kept.clone();
                for i in 0..out.len() {
                    let base = out.clone();
                    let case = out[i].clone();
                    let arr = self.ddmin_in_multi(&base, i, &case.arr);
                    out[i] = case.with_arr(arr);
                }
                Model::MultiTest(out)
            }
            Model::Tree(tree) => Model::Tree(self.shrink_tree(tree)),
            Model::Graph(graph) => Model::Graph(self.shrink_graph(graph)),
            Model::Raw(lines) => {
                // Drop whole lines first, then thin out the tokens on each
                // surviving line. Without the second step a single-line input
                // would not shrink at all.
                let mut kept = self.ddmin_lines(lines);
                for i in 0..kept.len() {
                    let base = kept.clone();
                    let tokens = kept[i].clone();
                    kept[i] = self.ddmin_tokens(&base, i, &tokens);
                }
                kept.retain(|l| !l.is_empty());
                Model::Raw(kept)
            }
        }
    }

    fn ddmin_arr(&mut self, case: &ArrayCase, arr: &[i64]) -> Vec<i64> {
        ddmin(arr, |cand| {
            let m = Model::Array(case.with_arr(cand.to_vec()));
            self.accept(&m)
        })
    }

    fn ddmin_in_multi(&mut self, base: &[ArrayCase], idx: usize, arr: &[i64]) -> Vec<i64> {
        ddmin(arr, |cand| {
            let mut tests = base.to_vec();
            tests[idx] = tests[idx].with_arr(cand.to_vec());
            self.accept(&Model::MultiTest(tests))
        })
    }

    fn ddmin_tests(&mut self, tests: &[ArrayCase]) -> Vec<ArrayCase> {
        ddmin(tests, |cand| self.accept(&Model::MultiTest(cand.to_vec())))
    }

    fn ddmin_lines(&mut self, lines: &[Vec<String>]) -> Vec<Vec<String>> {
        ddmin(lines, |cand| self.accept(&Model::Raw(cand.to_vec())))
    }

    fn ddmin_tokens(&mut self, base: &[Vec<String>], idx: usize, tokens: &[String]) -> Vec<String> {
        ddmin(tokens, |cand| {
            let mut lines = base.to_vec();
            lines[idx] = cand.to_vec();
            self.accept(&Model::Raw(lines))
        })
    }

    fn shrink_tree(&mut self, tree: &GraphCase) -> GraphCase {
        let mut current = tree.clone();
        loop {
            let leaves = current.leaves();
            if leaves.len() < 2 {
                break;
            }
            let mut is_leaf = vec![false; current.n + 1];
            for leaf in &leaves {
                is_leaf[*leaf] = true;
            }
            let internal: Vec<usize> = (1..=current.n).filter(|vertex| !is_leaf[*vertex]).collect();
            let base = current.clone();
            let kept_leaves = ddmin(&leaves, |candidate_leaves| {
                let mut kept = internal.clone();
                kept.extend_from_slice(candidate_leaves);
                kept.sort_unstable();
                if kept.is_empty() {
                    return false;
                }
                self.accept(&Model::Tree(base.induced(&kept)))
            });
            if kept_leaves.len() == leaves.len() {
                break;
            }
            let mut kept = internal;
            kept.extend(kept_leaves);
            kept.sort_unstable();
            current = base.induced(&kept);
        }
        current
    }

    fn shrink_graph(&mut self, graph: &GraphCase) -> GraphCase {
        let base = graph.clone();
        let edges = ddmin_allow_empty(&base.edges, |candidate| {
            self.accept(&Model::Graph(base.with_edges(candidate.to_vec())))
        });
        let current = base.with_edges(edges);

        let vertices: Vec<usize> = (1..=current.n).collect();
        let base = current.clone();
        let kept = ddmin(&vertices, |candidate| {
            !candidate.is_empty() && self.accept(&Model::Graph(base.induced(candidate)))
        });
        base.induced(&kept)
    }

    // ---- values ----------------------------------------------------------

    fn values(&mut self, m: &Model) -> Model {
        match m {
            Model::Array(c) => {
                let mut case = c.clone();
                let arr = shrink_ints(&case.arr, |cand| {
                    self.accept(&Model::Array(case.with_arr(cand.to_vec())))
                });
                case = case.with_arr(arr);

                // Extra header scalars (a `K`, a `M`) can shrink too, but never
                // the one bound to the array length.
                let n_idx = case.n_idx;
                for i in 0..case.header.len() {
                    if i == n_idx {
                        continue;
                    }
                    let original = case.header[i];
                    let candidate = shrink_value(original, |cand| {
                        let mut next = case.clone();
                        next.header[i] = cand;
                        self.accept(&Model::Array(next))
                    });
                    if candidate != original {
                        case.header[i] = candidate;
                    }
                }
                Model::Array(case)
            }
            Model::MultiTest(tests) => {
                let mut out = tests.clone();
                for i in 0..out.len() {
                    let base = out.clone();
                    let case = out[i].clone();
                    let arr = shrink_ints(&case.arr, |cand| {
                        let mut t = base.clone();
                        t[i] = t[i].with_arr(cand.to_vec());
                        self.accept(&Model::MultiTest(t))
                    });
                    out[i] = case.with_arr(arr);
                }
                Model::MultiTest(out)
            }
            Model::Tree(_) | Model::Graph(_) => m.clone(),
            // Raw values are not necessarily numeric; leave them alone.
            Model::Raw(_) => m.clone(),
        }
    }
}

/// Classic ddmin: try removing progressively finer chunks, restarting at a
/// coarser granularity whenever a removal sticks.
fn ddmin<T: Clone>(items: &[T], mut accept: impl FnMut(&[T]) -> bool) -> Vec<T> {
    let mut cur = items.to_vec();
    if cur.is_empty() {
        return cur;
    }
    let mut n = 2usize;
    while cur.len() >= 2 {
        let chunk = cur.len().div_ceil(n);
        let mut reduced = false;
        let mut start = 0usize;
        while start < cur.len() {
            let end = (start + chunk).min(cur.len());
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

fn ddmin_allow_empty<T: Clone>(items: &[T], mut accept: impl FnMut(&[T]) -> bool) -> Vec<T> {
    let current = ddmin(items, &mut accept);
    if !current.is_empty() && accept(&[]) {
        Vec::new()
    } else {
        current
    }
}

fn shrink_ints(vals: &[i64], mut accept: impl FnMut(&[i64]) -> bool) -> Vec<i64> {
    let mut cur = vals.to_vec();
    let mut improved = true;
    let mut rounds = 0;
    while improved && rounds < 16 {
        improved = false;
        rounds += 1;
        for i in 0..cur.len() {
            let original = cur[i];
            let candidate = shrink_value(original, |cand| {
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

/// Find the smallest accepted magnitude between zero and `x`. The predicate is
/// expected to have a boundary along that interval, which is the common shape
/// of numeric bugs (`x >= limit`, overflow thresholds, negative bounds). The
/// returned value is always one that was actually accepted.
fn shrink_value(x: i64, mut accept: impl FnMut(i64) -> bool) -> i64 {
    if x == 0 {
        return x;
    }
    if accept(0) {
        return 0;
    }

    let magnitude = x.unsigned_abs();
    if magnitude == 1 {
        return x;
    }
    if accept(x.signum()) {
        return x.signum();
    }

    let negative = x < 0;
    let mut low = 2u64;
    let mut high = magnitude;
    let mut best = x;
    while low < high {
        let mid = low + (high - low) / 2;
        let candidate = signed_magnitude(mid, negative);
        if accept(candidate) {
            best = candidate;
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    if low < magnitude {
        let candidate = signed_magnitude(low, negative);
        if accept(candidate) {
            best = candidate;
        }
    }
    best
}

fn signed_magnitude(magnitude: u64, negative: bool) -> i64 {
    if negative {
        if magnitude == 1u64 << 63 {
            i64::MIN
        } else {
            -(magnitude as i64)
        }
    } else {
        magnitude as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddmin_finds_single_required_element() {
        // Failure condition: the sentinel 42 must be present.
        let items: Vec<i64> = (0..64).collect::<Vec<_>>();
        let mut items = items;
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
    fn shrink_ints_pulls_to_minimum() {
        // Failure condition: some element is negative.
        let out = shrink_ints(&[500, -900_000, 12], |c| c.iter().any(|v| *v < 0));
        assert_eq!(out, vec![0, -1, 0]);
    }

    #[test]
    fn graph_ddmin_can_remove_the_final_edge() {
        let out = ddmin_allow_empty(&[42], |_| true);
        assert!(out.is_empty());
    }

    #[test]
    fn oracle_errors_abort_shrinking() {
        let missing = std::path::PathBuf::from("__ccmin_definitely_missing_executable__");
        let mut oracle = Oracle::new(
            missing.clone(),
            missing,
            std::time::Duration::from_millis(10),
            crate::proc::CompareMode::Exact,
            None,
        );
        let mut on_step = |_: &Model| {};
        let mut shrinker = Shrinker::new(&mut oracle, FailKind::WrongAnswer, &mut on_step);
        let model = Model::Array(ArrayCase {
            header: vec![2],
            n_idx: 0,
            arr: vec![1, 2],
        });
        assert!(shrinker.run(&model).is_err());
    }
}
