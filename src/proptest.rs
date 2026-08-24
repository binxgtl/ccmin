//! Randomised invariant tests for the parser and the reducers.
//!
//! For a tool whose whole value proposition is "the reduced input is still a
//! legal test case", the invariants matter more than the feature count. These
//! tests assert the properties that must hold for *every* input and *every*
//! failure predicate, rather than for the handful of cases someone thought to
//! write down:
//!
//!   1. `parse(render(m)) == m`             -- the model survives a round trip
//!   2. every candidate the reducer accepts is structurally valid
//!   3. the reduced model still satisfies the predicate it was reduced against
//!   4. the reduced model re-parses to itself
//!
//! Property 2 is the important one. It is checked on every intermediate step,
//! not just the final answer, so a reducer that briefly emits a desynchronised
//! length prefix is caught even if a later pass happens to repair it.
//!
//! The predicates include a deliberately chaotic one. A reducer only has to
//! stay *correct* under a non-monotone oracle, not effective, and adversarial
//! predicates are what expose structural bugs.
//!
//! No dependencies: the generator is a hand-rolled splitmix64 and the graph
//! checks are reimplemented here rather than reusing `model.rs`, so the tests
//! do not validate the code with itself.

use crate::model::{ArrayCase, Edge, GraphCase, Model, ParseOptions, Shape};
use crate::oracle::{FailKind, Judge};
use crate::shrink::Shrinker;
use std::collections::HashSet;

const CASES: u64 = 300;

// ---- deterministic randomness ------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1))
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }

    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }

    /// Values span the whole i64 domain so magnitude shrinking is exercised,
    /// with small values over-represented because that is where bugs cluster.
    fn value(&mut self) -> i64 {
        match self.below(4) {
            0 => self.next_u64() as i64,
            1 => (self.next_u64() % 2_000_000_001) as i64 - 1_000_000_000,
            2 => (self.next_u64() % 21) as i64 - 10,
            _ => [0, 1, -1, i64::MIN, i64::MAX][self.below(5)],
        }
    }
}

// ---- generators ---------------------------------------------------------

fn gen_array(rng: &mut Rng) -> ArrayCase {
    let len = rng.range(1, 12);
    let header_len = rng.range(1, 3);
    let n_idx = rng.below(header_len);
    let mut header: Vec<i64> = (0..header_len).map(|_| rng.value()).collect();
    header[n_idx] = len as i64;
    ArrayCase {
        header,
        n_idx,
        arr: (0..len).map(|_| rng.value()).collect(),
    }
}

fn gen_multitest(rng: &mut Rng) -> Vec<ArrayCase> {
    let count = rng.range(1, 4);
    (0..count)
        .map(|_| {
            let len = rng.below(7);
            ArrayCase {
                header: vec![len as i64],
                n_idx: 0,
                arr: (0..len).map(|_| rng.value()).collect(),
            }
        })
        .collect()
}

/// Built from a random parent array, so it is a tree by construction.
fn gen_tree(rng: &mut Rng) -> GraphCase {
    let n = rng.range(1, 12);
    let edges = (2..=n)
        .map(|v| Edge {
            u: rng.range(1, v - 1),
            v,
        })
        .collect();
    GraphCase { n, edges }
}

fn gen_graph(rng: &mut Rng) -> GraphCase {
    let n = rng.range(1, 10);
    let m = rng.below(14);
    let edges = (0..m)
        .map(|_| Edge {
            u: rng.range(1, n),
            v: rng.range(1, n),
        })
        .collect();
    GraphCase { n, edges }
}

fn gen_raw(rng: &mut Rng) -> Vec<Vec<String>> {
    let lines = rng.range(1, 5);
    (0..lines)
        .map(|_| {
            let tokens = rng.range(1, 6);
            (0..tokens)
                .map(|_| match rng.below(3) {
                    0 => rng.value().to_string(),
                    1 => ["a", "A", "b", "zz"][rng.below(4)].to_string(),
                    _ => format!("x{}", rng.below(100)),
                })
                .collect()
        })
        .collect()
}

fn gen_model(rng: &mut Rng) -> Model {
    match rng.below(5) {
        0 => Model::Array(gen_array(rng)),
        1 => Model::MultiTest(gen_multitest(rng)),
        2 => Model::Tree(gen_tree(rng)),
        3 => Model::Graph(gen_graph(rng)),
        _ => Model::Raw(gen_raw(rng)),
    }
}

fn options_for(m: &Model) -> ParseOptions {
    match m {
        Model::Array(c) => ParseOptions {
            shape: Shape::Array,
            n_index: Some(c.n_idx),
            schema: None,
            guess_header: false,
        },
        Model::MultiTest(_) => ParseOptions {
            shape: Shape::MultiTest,
            ..ParseOptions::default()
        },
        Model::Tree(_) => ParseOptions {
            shape: Shape::Tree,
            ..ParseOptions::default()
        },
        Model::Graph(_) => ParseOptions {
            shape: Shape::Graph,
            ..ParseOptions::default()
        },
        Model::Raw(_) => ParseOptions {
            shape: Shape::Raw,
            ..ParseOptions::default()
        },
        // Not produced by these generators; schema inputs have their own tests
        // in schema.rs, where the declared grammar is available.
        Model::Schema(_) => ParseOptions::default(),
    }
}

// ---- independent validity checks ---------------------------------------

/// Deliberately does not call into `model.rs`: a validity check that shares an
/// implementation with the code under test cannot catch a shared mistake.
fn assert_structurally_valid(m: &Model, context: &str) {
    match m {
        Model::Array(c) => {
            assert!(
                c.n_idx < c.header.len(),
                "{context}: n_idx {} outside header of {}",
                c.n_idx,
                c.header.len()
            );
            assert_eq!(
                c.header[c.n_idx],
                c.arr.len() as i64,
                "{context}: declared length does not match the data"
            );
        }
        Model::MultiTest(tests) => {
            for (i, t) in tests.iter().enumerate() {
                assert_eq!(
                    t.header[t.n_idx],
                    t.arr.len() as i64,
                    "{context}: case {i} declared length does not match the data"
                );
            }
        }
        Model::Tree(tree) => {
            assert_endpoints_in_range(tree, context);
            assert!(tree.n >= 1, "{context}: tree with no vertices");
            assert_eq!(
                tree.edges.len(),
                tree.n - 1,
                "{context}: tree must have exactly n-1 edges"
            );
            assert!(
                is_connected(tree),
                "{context}: tree reduction produced a disconnected graph"
            );
        }
        Model::Graph(graph) => assert_endpoints_in_range(graph, context),
        // Schema inputs are generated and checked in schema.rs, where the
        // declared grammar is available to check against.
        Model::Schema(_) => {}
        Model::Raw(_) => {}
    }
}

fn assert_endpoints_in_range(g: &GraphCase, context: &str) {
    for e in &g.edges {
        assert!(
            e.u >= 1 && e.u <= g.n && e.v >= 1 && e.v <= g.n,
            "{context}: dangling endpoint ({}, {}) with n = {}",
            e.u,
            e.v,
            g.n
        );
    }
}

fn is_connected(g: &GraphCase) -> bool {
    if g.n == 0 {
        return false;
    }
    let mut adjacency = vec![Vec::new(); g.n + 1];
    for e in &g.edges {
        adjacency[e.u].push(e.v);
        adjacency[e.v].push(e.u);
    }
    let mut seen = vec![false; g.n + 1];
    let mut stack = vec![1usize];
    seen[1] = true;
    let mut count = 1usize;
    while let Some(v) = stack.pop() {
        for &next in &adjacency[v] {
            if !seen[next] {
                seen[next] = true;
                count += 1;
                stack.push(next);
            }
        }
    }
    count == g.n
}

// ---- predicates ---------------------------------------------------------

fn fnv1a(s: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in s.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

fn ints_of(text: &str) -> Vec<i64> {
    text.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect()
}

/// A family of failure predicates. `Chaotic` is not something a real bug looks
/// like; it is here because a reducer must remain *correct* under an oracle
/// that gives no useful gradient, and that is where structural bugs surface.
fn predicate(kind: usize, k: usize) -> Box<dyn Fn(&str) -> bool> {
    match kind {
        0 => Box::new(|_: &str| true),
        1 => Box::new(move |t: &str| t.split_whitespace().count() >= k),
        2 => Box::new(|t: &str| ints_of(t).iter().any(|v| *v < 0)),
        3 => Box::new(move |t: &str| {
            ints_of(t).iter().map(|v| v.unsigned_abs()).sum::<u64>() >= k as u64
        }),
        _ => Box::new(|t: &str| fnv1a(t) % 4 != 0),
    }
}

struct PredicateJudge<'a> {
    predicate: &'a dyn Fn(&str) -> bool,
}

impl Judge for PredicateJudge<'_> {
    fn preserves(&mut self, input: &str, _target: FailKind) -> std::io::Result<bool> {
        Ok((self.predicate)(input))
    }
}

// ---- properties ---------------------------------------------------------

#[test]
fn parse_round_trips_every_generated_model() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed);
        let model = gen_model(&mut rng);
        let text = model.render();
        let reparsed = crate::model::parse_with(&text, options_for(&model))
            .unwrap_or_else(|e| panic!("seed {seed}: {text:?} failed to re-parse: {e}"));
        assert_eq!(reparsed, model, "seed {seed}: round trip changed the model");
    }
}

#[test]
fn generated_models_are_valid_to_begin_with() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed);
        let model = gen_model(&mut rng);
        assert_structurally_valid(&model, &format!("seed {seed}: generator"));
    }
}

#[test]
fn every_candidate_the_reducer_accepts_is_structurally_valid() {
    let mut checked = 0usize;
    for seed in 0..CASES {
        let mut rng = Rng::new(seed ^ 0xA5A5);
        let model = gen_model(&mut rng);
        let kind = rng.below(5);
        let k = rng.range(1, 8);
        let predicate = predicate(kind, k);

        // The reducer is only meaningful when the starting point fails.
        if !predicate(&model.render()) {
            continue;
        }
        checked += 1;

        let mut accepted: Vec<Model> = Vec::new();
        let reduced = {
            let mut judge = PredicateJudge {
                predicate: predicate.as_ref(),
            };
            let mut on_step = |m: &Model| accepted.push(m.clone());
            let mut shrinker = Shrinker::new(&mut judge, FailKind::WrongAnswer, &mut on_step);
            shrinker.run(&model).expect("pure predicate cannot error")
        };

        // Property 2, checked on every intermediate step.
        for (i, candidate) in accepted.iter().enumerate() {
            assert_structurally_valid(candidate, &format!("seed {seed}: accepted step {i}"));
        }

        // Property 3: the answer still reproduces.
        assert_structurally_valid(&reduced, &format!("seed {seed}: final"));
        assert!(
            predicate(&reduced.render()),
            "seed {seed}: reduced model no longer satisfies the predicate"
        );

        // Property 4: and it is still a legal input of the same shape.
        let text = reduced.render();
        let reparsed = crate::model::parse_with(&text, options_for(&reduced))
            .unwrap_or_else(|e| panic!("seed {seed}: reduced {text:?} does not re-parse: {e}"));
        assert_eq!(
            reparsed, reduced,
            "seed {seed}: reduced model does not round trip"
        );
    }
    assert!(
        checked > CASES as usize / 4,
        "only {checked} cases had a failing start; the generators or predicates are too weak"
    );
}

#[test]
fn reduction_never_grows_the_input() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed ^ 0x5A5A);
        let model = gen_model(&mut rng);
        let predicate = predicate(rng.below(5), rng.range(1, 8));
        if !predicate(&model.render()) {
            continue;
        }

        let before = model.size();
        let reduced = {
            let mut judge = PredicateJudge {
                predicate: predicate.as_ref(),
            };
            let mut on_step = |_: &Model| {};
            let mut shrinker = Shrinker::new(&mut judge, FailKind::WrongAnswer, &mut on_step);
            shrinker.run(&model).unwrap()
        };
        assert!(
            reduced.size() <= before,
            "seed {seed}: reduction grew {before} -> {}",
            reduced.size()
        );
    }
}

#[test]
fn tree_reduction_preserves_treeness_under_a_chaotic_oracle() {
    // Trees are the shape with the most invariants to break, and the chaotic
    // predicate gives the reducer no gradient to follow.
    for seed in 0..CASES {
        let mut rng = Rng::new(seed ^ 0x7EE7);
        let tree = Model::Tree(gen_tree(&mut rng));
        let predicate = predicate(4, 0);
        if !predicate(&tree.render()) {
            continue;
        }

        let mut accepted: Vec<Model> = Vec::new();
        let reduced = {
            let mut judge = PredicateJudge {
                predicate: predicate.as_ref(),
            };
            let mut on_step = |m: &Model| accepted.push(m.clone());
            let mut shrinker = Shrinker::new(&mut judge, FailKind::WrongAnswer, &mut on_step);
            shrinker.run(&tree).unwrap()
        };

        for (i, candidate) in accepted.iter().enumerate() {
            assert_structurally_valid(candidate, &format!("seed {seed}: tree step {i}"));
        }
        assert_structurally_valid(&reduced, &format!("seed {seed}: tree final"));
    }
}

#[test]
fn graph_vertex_compaction_leaves_no_gaps() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed ^ 0x9999);
        let graph = gen_graph(&mut rng);
        let all: Vec<usize> = (1..=graph.n).collect();

        // Every induced subset must renumber to a contiguous 1..=k.
        let keep: Vec<usize> = all.iter().copied().filter(|_| rng.below(2) == 0).collect();
        if keep.is_empty() {
            continue;
        }
        let induced = graph.induced(&keep);
        assert_eq!(induced.n, keep.len(), "seed {seed}: vertex count wrong");
        assert_endpoints_in_range(&induced, &format!("seed {seed}: induced"));

        let labels: HashSet<usize> = induced.edges.iter().flat_map(|e| [e.u, e.v]).collect();
        for label in labels {
            assert!(
                (1..=induced.n).contains(&label),
                "seed {seed}: label {label} outside 1..={}",
                induced.n
            );
        }
    }
}
