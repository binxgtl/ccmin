//! Delta debugging over the parsed model.
//!
//! Two alternating passes, run to a fixpoint:
//!   1. structural — delete elements (and whole test cases) via ddmin
//!   2. value      — pull each remaining integer toward zero
//!
//! Every candidate is rendered from the model, so the declared length prefix
//! always matches the data. See `model.rs` for why that matters.

use crate::model::{ArrayCase, GraphCase, Model};
use crate::oracle::{FailKind, Judge};
use crate::reduce::{ddmin, ddmin_allow_empty, shrink_ints, shrink_value};

pub struct Shrinker<'a> {
    judge: &'a mut dyn Judge,
    target: FailKind,
    on_step: &'a mut dyn FnMut(&Model),
    error: Option<String>,
}

impl<'a> Shrinker<'a> {
    pub fn new(
        judge: &'a mut dyn Judge,
        target: FailKind,
        on_step: &'a mut dyn FnMut(&Model),
    ) -> Self {
        Shrinker {
            judge,
            target,
            on_step,
            error: None,
        }
    }

    fn accept(&mut self, m: &Model) -> bool {
        if self.error.is_some() {
            return false;
        }
        let ok = match self.judge.preserves(&m.render(), self.target) {
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
            Model::Schema(data) => {
                Model::Schema(data.structural_pass(&mut |candidate| {
                    self.accept(&Model::Schema(candidate.clone()))
                }))
            }
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
            Model::Schema(data) => Model::Schema(
                data.value_pass(&mut |candidate| self.accept(&Model::Schema(candidate.clone()))),
            ),
            Model::Tree(_) | Model::Graph(_) => m.clone(),
            // Raw values are not necessarily numeric; leave them alone.
            Model::Raw(_) => m.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle::Oracle;

    /// Records every candidate the reducer asks about. Failure condition is
    /// "some value is negative", which is the shape of the demo's bug.
    struct Recorder {
        seen: Vec<String>,
    }

    impl crate::oracle::Judge for Recorder {
        fn preserves(&mut self, input: &str, _target: FailKind) -> std::io::Result<bool> {
            self.seen.push(input.to_string());
            Ok(input
                .split_whitespace()
                .filter_map(|t| t.parse::<i64>().ok())
                .any(|v| v < 0))
        }
    }

    /// The fixpoint loop re-runs both passes until nothing changes, so the last
    /// round necessarily re-asks questions the previous round already answered.
    /// That is what `Oracle`'s memo cache exists to absorb; if this ever stops
    /// holding, the cache is dead weight and should go.
    #[test]
    fn the_reducer_re_asks_candidates_so_memoisation_pays() {
        let mut arr: Vec<i64> = (1..=100).collect();
        arr[37] = -999_999_999;
        let model = Model::Array(ArrayCase {
            header: vec![100],
            n_idx: 0,
            arr,
        });

        let mut recorder = Recorder { seen: Vec::new() };
        let mut on_step = |_: &Model| {};
        let mut shrinker = Shrinker::new(&mut recorder, FailKind::WrongAnswer, &mut on_step);
        let reduced = shrinker.run(&model).unwrap();

        // The reducer still does its job.
        assert_eq!(
            reduced,
            Model::Array(ArrayCase {
                header: vec![1],
                n_idx: 0,
                arr: vec![-1],
            })
        );

        let total = recorder.seen.len();
        let unique: std::collections::HashSet<&String> = recorder.seen.iter().collect();
        assert!(
            unique.len() < total,
            "expected repeated candidates, saw {total} queries all distinct"
        );
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
