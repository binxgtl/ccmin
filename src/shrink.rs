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
        }
    }

    fn accept(&mut self, m: &Model) -> bool {
        let ok = self.oracle.preserves(&m.render(), self.target);
        if ok {
            (self.on_step)(m);
        }
        ok
    }

    pub fn run(&mut self, start: &Model) -> Model {
        let mut best = start.clone();
        // A handful of rounds is plenty; the passes converge fast and this
        // bounds pathological cases.
        for _ in 0..4 {
            let before = best.clone();
            best = self.structural(&best);
            best = self.values(&best);
            if best == before {
                break;
            }
        }
        best
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
                    for cand in toward_zero(case.header[i]) {
                        let mut next = case.clone();
                        next.header[i] = cand;
                        if self.accept(&Model::Array(next.clone())) {
                            case = next;
                            break;
                        }
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
    while improved && rounds < 8 {
        improved = false;
        rounds += 1;
        for i in 0..cur.len() {
            for cand in toward_zero(cur[i]) {
                let mut next = cur.clone();
                next[i] = cand;
                if accept(&next) {
                    cur = next;
                    improved = true;
                    break;
                }
            }
        }
    }
    cur
}

/// Candidate replacements for `x`, strictly smaller in magnitude, simplest
/// first. Sign is preserved by the `±1` step so a negative value that matters
/// stays negative.
fn toward_zero(x: i64) -> Vec<i64> {
    let mut out = Vec::new();
    if x == 0 {
        return out;
    }
    out.push(0);
    if x.abs() > 1 {
        out.push(x.signum());
        out.push(x / 2);
    }
    out.retain(|c| c.abs() < x.abs());
    out.dedup();
    out
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
    fn toward_zero_is_strictly_smaller() {
        for x in [-1_000_000_000i64, -7, -1, 1, 7, 1_000_000_000] {
            for c in toward_zero(x) {
                assert!(c.abs() < x.abs(), "{c} not smaller than {x}");
            }
        }
        assert!(toward_zero(0).is_empty());
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
}
