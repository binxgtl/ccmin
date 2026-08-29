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

// ---- schema generation --------------------------------------------------

/// One declaration in a generated block.
///
/// The schema text and a conforming input are produced from the same list, so
/// the input is right by construction rather than by re-implementing the
/// reader inside its own test.
enum Item {
    Count {
        name: String,
        lo: usize,
        hi: usize,
    },
    Array {
        name: String,
        len: String,
        lo: i64,
        hi: i64,
    },
    /// `array A[N] in 1..N` -- magnitudes bounded by a count.
    DynArray {
        name: String,
        len: String,
    },
    Matrix {
        name: String,
        rows: String,
        cols: String,
    },
    Perm {
        name: String,
        len: String,
    },
    Index {
        name: String,
        len: String,
        target: String,
    },
    Graph {
        name: String,
        edges: String,
        verts: String,
    },
    Tree {
        name: String,
        verts: String,
    },
    /// `array W[P.values]` -- follows a permutation's codomain. `len` is the
    /// count behind it, needed only to know how many values to emit.
    PermValues {
        name: String,
        perm: String,
        len: String,
    },
    Repeat {
        count: String,
        body: Vec<Item>,
    },
}

fn decl_text(item: &Item, out: &mut String, indent: &str) {
    match item {
        Item::Count { name, lo, hi } => {
            out.push_str(&format!("{indent}int {name} in {lo}..{hi}\n"));
        }
        Item::Array { name, len, lo, hi } => {
            out.push_str(&format!("{indent}array {name}[{len}] in {lo}..{hi}\n"));
        }
        Item::DynArray { name, len } => {
            out.push_str(&format!("{indent}array {name}[{len}] in 1..{len}\n"));
        }
        Item::Matrix { name, rows, cols } => {
            out.push_str(&format!("{indent}matrix {name}[{rows}][{cols}] in 0..9\n"));
        }
        Item::Perm { name, len } => {
            out.push_str(&format!("{indent}permutation {name}[{len}]\n"));
        }
        Item::Index { name, len, target } => {
            out.push_str(&format!("{indent}index {name}[{len}] into {target}\n"));
        }
        Item::Graph { name, edges, verts } => {
            out.push_str(&format!("{indent}graph {name}[{edges}] vertices {verts}\n"));
        }
        Item::Tree { name, verts } => {
            out.push_str(&format!("{indent}tree {name} vertices {verts}\n"));
        }
        Item::PermValues { name, perm, .. } => {
            out.push_str(&format!("{indent}array {name}[{perm}.values] in 0..99\n"));
        }
        Item::Repeat { count, body } => {
            out.push_str(&format!("{indent}repeat {count} {{\n"));
            for inner in body {
                decl_text(inner, out, "  ");
            }
            out.push_str(&format!("{indent}}}\n"));
        }
    }
}

/// Emit data for one instantiation of a block, recording count values as it
/// goes. Reading is linear, so a count is always known before it is used.
fn emit_data(items: &[Item], rng: &mut Rng, counts: &mut Vec<(String, usize)>, out: &mut String) {
    let get = |counts: &Vec<(String, usize)>, name: &str| -> usize {
        counts
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| *v)
            .unwrap_or(1)
    };
    for item in items {
        match item {
            Item::Count { name, lo, hi } => {
                let v = rng.range(*lo, *hi);
                counts.push((name.clone(), v));
                out.push_str(&format!("{v}\n"));
            }
            Item::Array { len, lo, hi, .. } => {
                let n = get(counts, len);
                let span = (hi - lo + 1).max(1) as usize;
                let vals: Vec<String> = (0..n)
                    .map(|_| (lo + rng.below(span) as i64).to_string())
                    .collect();
                out.push_str(&format!("{}\n", vals.join(" ")));
            }
            Item::DynArray { len, .. } => {
                let n = get(counts, len);
                let vals: Vec<String> = (0..n).map(|_| rng.range(1, n).to_string()).collect();
                out.push_str(&format!("{}\n", vals.join(" ")));
            }
            Item::Matrix { rows, cols, .. } => {
                let (r, c) = (get(counts, rows), get(counts, cols));
                for _ in 0..r {
                    let row: Vec<String> = (0..c).map(|_| rng.below(10).to_string()).collect();
                    out.push_str(&format!("{}\n", row.join(" ")));
                }
            }
            Item::Perm { len, .. } => {
                let n = get(counts, len);
                let mut v: Vec<usize> = (1..=n).collect();
                for i in (1..v.len()).rev() {
                    v.swap(i, rng.below(i + 1));
                }
                let vals: Vec<String> = v.iter().map(|x| x.to_string()).collect();
                out.push_str(&format!("{}\n", vals.join(" ")));
            }
            Item::Index { len, target, .. } => {
                let (k, t) = (get(counts, len), get(counts, target));
                let vals: Vec<String> = (0..k).map(|_| rng.range(1, t).to_string()).collect();
                out.push_str(&format!("{}\n", vals.join(" ")));
            }
            Item::Graph { edges, verts, .. } => {
                let (m, n) = (get(counts, edges), get(counts, verts));
                for _ in 0..m {
                    out.push_str(&format!("{} {}\n", rng.range(1, n), rng.range(1, n)));
                }
            }
            Item::PermValues { len, .. } => {
                let n = get(counts, len);
                let vals: Vec<String> = (0..n).map(|_| rng.below(100).to_string()).collect();
                out.push_str(&format!("{}\n", vals.join(" ")));
            }
            Item::Tree { verts, .. } => {
                let n = get(counts, verts);
                for child in 2..=n {
                    out.push_str(&format!("{} {}\n", rng.range(1, child - 1), child));
                }
            }
            Item::Repeat { count, body } => {
                let t = get(counts, count);
                for _ in 0..t {
                    let depth = counts.len();
                    emit_data(body, rng, counts, out);
                    counts.truncate(depth);
                }
            }
        }
    }
}

/// Build a block whose declarations are valid by construction: a count exists
/// before anything is sized by it, an `index` or `permutation` only targets a
/// count that already sizes something, and no count carries two permutations.
fn gen_block(rng: &mut Rng, id: &mut usize, nested: bool) -> Vec<Item> {
    let mut items = Vec::new();
    let mut sized: Vec<String> = Vec::new();
    let mut permuted: HashSet<String> = HashSet::new();

    let base = {
        *id += 1;
        format!("N{id}")
    };
    items.push(Item::Count {
        name: base.clone(),
        lo: 1,
        hi: rng.range(2, 6),
    });
    items.push(Item::Array {
        name: format!("A{id}"),
        len: base.clone(),
        lo: 0,
        hi: 99,
    });
    sized.push(base.clone());

    for _ in 0..rng.range(1, 4) {
        *id += 1;
        let name = format!("X{id}");
        let target = sized[rng.below(sized.len())].clone();
        match rng.below(if nested { 6 } else { 9 }) {
            0 => items.push(Item::Array {
                name,
                len: target,
                lo: -50,
                hi: 50,
            }),
            1 => items.push(Item::DynArray { name, len: target }),
            2 if !permuted.contains(&target) => {
                permuted.insert(target.clone());
                items.push(Item::Perm {
                    name: name.clone(),
                    len: target.clone(),
                });
                // Half the time, hang an array off the codomain as well.
                if rng.below(2) == 0 {
                    *id += 1;
                    items.push(Item::PermValues {
                        name: format!("W{id}"),
                        perm: name,
                        len: target,
                    });
                }
            }
            3 => {
                *id += 1;
                let k = format!("K{id}");
                items.push(Item::Count {
                    name: k.clone(),
                    lo: 1,
                    hi: rng.range(1, 4),
                });
                items.push(Item::Index {
                    name,
                    len: k.clone(),
                    target,
                });
                sized.push(k);
            }
            4 => {
                *id += 1;
                let m = format!("M{id}");
                items.push(Item::Count {
                    name: m.clone(),
                    lo: 1,
                    hi: rng.range(1, 4),
                });
                items.push(Item::Graph {
                    name,
                    edges: m.clone(),
                    verts: target,
                });
                sized.push(m);
            }
            5 => {
                *id += 1;
                let c = format!("C{id}");
                items.push(Item::Count {
                    name: c.clone(),
                    lo: 1,
                    hi: rng.range(1, 3),
                });
                items.push(Item::Matrix {
                    name,
                    rows: target,
                    cols: c.clone(),
                });
                sized.push(c);
            }
            6 => {
                *id += 1;
                let t = format!("T{id}");
                items.push(Item::Count {
                    name: t.clone(),
                    lo: 1,
                    hi: rng.range(1, 3),
                });
                items.push(Item::Repeat {
                    count: t,
                    body: gen_block(rng, id, true),
                });
            }
            7 => {
                // A tree needs at least two vertices, and its own count so the
                // rest of the block is unaffected by the connectivity rule.
                *id += 1;
                let v = format!("V{id}");
                items.push(Item::Count {
                    name: v.clone(),
                    lo: 2,
                    hi: rng.range(2, 6),
                });
                items.push(Item::Tree {
                    name,
                    verts: v.clone(),
                });
                sized.push(v);
            }
            _ => items.push(Item::Array {
                name,
                len: target,
                lo: 1,
                hi: 1000,
            }),
        }
    }
    items
}

/// A schema and an input that satisfies it.
fn gen_schema_case(rng: &mut Rng) -> (String, String) {
    let mut id = 0usize;
    let items = gen_block(rng, &mut id, false);
    let mut text = String::new();
    for item in &items {
        decl_text(item, &mut text, "");
    }
    let mut input = String::new();
    emit_data(&items, rng, &mut Vec::new(), &mut input);
    (text, input)
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
            shrinker.run(&model).expect("pure predicate cannot error").0
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
            shrinker.run(&model).unwrap().0
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
            shrinker.run(&tree).unwrap().0
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

// ---- schema properties --------------------------------------------------

/// Asserts the reducer's central promise on *every* candidate it constructs,
/// not merely on the one it returns: a rendered candidate always re-parses
/// under the same schema. Dangling references, a broken permutation, a count
/// out of step with its data, or a value outside a count-referenced range
/// would all surface here.
struct SchemaJudge<'a> {
    schema: &'a std::rc::Rc<crate::schema::Schema>,
    predicate: &'a dyn Fn(&str) -> bool,
    seed: u64,
    seen: usize,
}

impl Judge for SchemaJudge<'_> {
    fn preserves(&mut self, input: &str, _target: FailKind) -> std::io::Result<bool> {
        self.seen += 1;
        if let Err(e) = crate::schema::parse_input(self.schema, input) {
            panic!(
                "seed {}: the reducer offered an input that does not parse: {e}\n\
                 --- schema ---\n{}\n--- candidate ---\n{input}",
                self.seed,
                self.schema_text()
            );
        }
        Ok((self.predicate)(input))
    }
}

impl SchemaJudge<'_> {
    fn schema_text(&self) -> String {
        "(see the failing seed)".into()
    }
}

fn schema_model(text: &str, input: &str) -> Option<(Model, std::rc::Rc<crate::schema::Schema>)> {
    let schema = crate::schema::parse_schema(text).ok()?;
    let options = crate::model::ParseOptions {
        shape: crate::model::Shape::Auto,
        n_index: None,
        schema: Some(std::rc::Rc::clone(&schema)),
        guess_header: false,
    };
    let model = crate::model::parse_with(input, options).ok()?;
    Some((model, schema))
}

#[test]
fn generated_schemas_and_inputs_agree() {
    let mut built = 0;
    for seed in 0..CASES {
        let mut rng = Rng::new(seed ^ 0x5C4E);
        let (text, input) = gen_schema_case(&mut rng);
        let schema = crate::schema::parse_schema(&text).unwrap_or_else(|e| {
            panic!("seed {seed}: generated schema does not parse: {e}\n{text}")
        });
        let data = crate::schema::parse_input(&schema, &input).unwrap_or_else(|e| {
            panic!("seed {seed}: generated input does not parse: {e}\n{text}\n---\n{input}")
        });
        assert_eq!(
            data.render(),
            input,
            "seed {seed}: render is not the identity on a freshly read input\n{text}"
        );
        built += 1;
    }
    assert!(built > 0, "the generator produced nothing");
}

#[test]
fn every_schema_candidate_the_reducer_offers_re_parses() {
    let mut exercised = 0;
    for seed in 0..CASES {
        let mut rng = Rng::new(seed ^ 0xA17E);
        let (text, input) = gen_schema_case(&mut rng);
        let Some((model, schema)) = schema_model(&text, &input) else {
            continue;
        };
        let k = rng.range(1, 6);
        let predicate = predicate(rng.below(5), k);
        if !predicate(&model.render()) {
            continue;
        }
        let mut judge = SchemaJudge {
            schema: &schema,
            predicate: predicate.as_ref(),
            seed,
            seen: 0,
        };
        let mut on_step = |_: &Model| {};
        let mut shrinker = Shrinker::new(&mut judge, FailKind::WrongAnswer, &mut on_step);
        let (reduced, _) = shrinker
            .run(&model)
            .unwrap_or_else(|e| panic!("seed {seed}: reduction failed: {e}\n{text}"));

        let out = reduced.render();
        crate::schema::parse_input(&schema, &out).unwrap_or_else(|e| {
            panic!("seed {seed}: the reduced input does not parse: {e}\n{text}\n---\n{out}")
        });
        assert!(
            predicate(&out),
            "seed {seed}: the reduced input no longer satisfies the predicate\n{text}\n---\n{out}"
        );
        exercised += 1;
    }
    assert!(
        exercised > CASES as usize / 4,
        "only {exercised} schema reductions ran; the generator or the filter is too strict"
    );
}

#[test]
fn schema_reduction_never_grows_the_input() {
    for seed in 0..CASES {
        let mut rng = Rng::new(seed ^ 0x9B21);
        let (text, input) = gen_schema_case(&mut rng);
        let Some((model, _)) = schema_model(&text, &input) else {
            continue;
        };
        let before = model.render().split_whitespace().count();
        let k = rng.range(1, 6);
        let predicate = predicate(rng.below(5), k);
        if !predicate(&model.render()) {
            continue;
        }
        let mut judge = PredicateJudge {
            predicate: predicate.as_ref(),
        };
        let mut on_step = |_: &Model| {};
        let mut shrinker = Shrinker::new(&mut judge, FailKind::WrongAnswer, &mut on_step);
        let (reduced, _) = shrinker.run(&model).expect("pure predicate cannot error");
        let after = reduced.render().split_whitespace().count();
        assert!(
            after <= before,
            "seed {seed}: reduction grew the input from {before} to {after}\n{text}"
        );
    }
}

/// A schema file is user input and may be anything at all. Every outcome must
/// be an error, never a panic and never a hang.
#[test]
fn malformed_schema_text_is_rejected_not_fatal() {
    const WORDS: &[&str] = &[
        "int",
        "array",
        "matrix",
        "tree",
        "graph",
        "index",
        "permutation",
        "repeat",
        "in",
        "into",
        "vertices",
        "{",
        "}",
        "[",
        "]",
        "..",
        "N",
        "A",
        "0",
        "1",
        "-1",
        "999999999999999999999",
        "#",
        ".values",
        "[]",
        "[N]",
        "[N][M]",
        "1..N",
        "..",
        "N..1",
        "",
        "\t",
    ];
    for seed in 0..CASES * 4 {
        let mut rng = Rng::new(seed ^ 0xDEAD);
        let lines = rng.range(1, 6);
        let mut text = String::new();
        for _ in 0..lines {
            let n = rng.range(1, 6);
            let toks: Vec<&str> = (0..n).map(|_| WORDS[rng.below(WORDS.len())]).collect();
            text.push_str(&toks.join(" "));
            text.push('\n');
        }
        // Whatever comes back, it must be a value and not a crash.
        if let Ok(schema) = crate::schema::parse_schema(&text) {
            // A schema that parsed must also survive arbitrary input safely.
            let _ = crate::schema::parse_input(&schema, "1 2 3\n4 5 6\n");
            let _ = crate::schema::parse_input(&schema, "");
        }
    }
}

/// The same for the data file, against a schema that is known good.
#[test]
fn malformed_input_is_rejected_not_fatal() {
    let schemas = [
        "int N in 1..10\narray A[N] in 1..N\n",
        "int N in 1..10\nint M in 0..20\ngraph E[M] vertices N\nindex I[M] into N\n",
        "int N in 2..10\ntree E vertices N\narray C[N] in 0..9\n",
        "int N in 1..10\npermutation P[N]\narray W[P.values] in 0..99\n",
        "int T in 1..3\nrepeat T {\n  int N in 1..5\n  array A[N] in 1..N\n}\n",
    ];
    for seed in 0..CASES * 4 {
        let mut rng = Rng::new(seed ^ 0xBEEF);
        let schema = crate::schema::parse_schema(schemas[rng.below(schemas.len())])
            .expect("fixture schemas parse");
        let n = rng.below(14);
        let toks: Vec<String> = (0..n)
            .map(|_| match rng.below(6) {
                0 => rng.value().to_string(),
                1 => rng.range(0, 6).to_string(),
                2 => "0".into(),
                3 => "-1".into(),
                4 => i64::MIN.to_string(),
                _ => "x".into(),
            })
            .collect();
        let _ = crate::schema::parse_input(&schema, &toks.join(" "));
    }
}
