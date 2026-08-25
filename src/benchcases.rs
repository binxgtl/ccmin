//! Behavioural snapshots of the v0.4 reducer.
//!
//! These exist to freeze v0.4 before the v0.5 refactor, which collapses seven
//! reduction paths into one. Without a fixed corpus there is no way to tell
//! whether the cleaner abstraction reduces as well as the messy one.
//!
//! **Oracle call counts are asserted exactly, not as an upper bound.** This is
//! not a portable performance benchmark — the judge is a pure in-memory
//! predicate, so the counts are deterministic. The point is to notice
//! immediately when a change alters the search path at all. A `<= 100` style
//! threshold would let a regression from 40 to 70 calls through unnoticed.
//!
//! When a change to the algorithm is deliberate, regenerate and review the
//! diff:
//!
//! ```text
//! UPDATE_BENCH=1 cargo test --release benchcases
//! ```
//!
//! `benchcases/baseline/` holds these exact snapshots. A future
//! `benchcases/capability/` should assert only a quality floor (tokens under
//! some K, failure preserved) so the search algorithm stays free to improve;
//! see `benchcases/README.md`.

use crate::model::{self, Model, ParseOptions, Shape};
use crate::oracle::{FailKind, Judge};
use crate::schema;
use crate::shrink::Shrinker;
use std::path::PathBuf;

// ---- the failure predicates --------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Pred {
    /// Every input fails; the reducer goes as small as the shape allows.
    Always,
    AnyNegative,
    ContainsValue(i64),
    /// Several values at once, so a case can pin two different occurrences.
    ContainsAll(&'static [i64]),
    ContainsToken(&'static str),
    /// A floor, so structural reduction has something to stop against.
    MinTokens(usize),
    /// Drives boundary search rather than deletion.
    AnyAtLeast(i64),
}

impl Pred {
    fn holds(&self, text: &str) -> bool {
        let ints = || {
            text.split_whitespace()
                .filter_map(|t| t.parse::<i64>().ok())
                .collect::<Vec<_>>()
        };
        match self {
            Pred::Always => true,
            Pred::AnyNegative => ints().iter().any(|v| *v < 0),
            Pred::ContainsValue(v) => ints().contains(v),
            Pred::ContainsAll(vs) => {
                let present = ints();
                vs.iter().all(|v| present.contains(v))
            }
            Pred::ContainsToken(t) => text.split_whitespace().any(|x| x == *t),
            Pred::MinTokens(k) => text.split_whitespace().count() >= *k,
            Pred::AnyAtLeast(v) => ints().iter().any(|x| x >= v),
        }
    }
}

struct CountingJudge {
    predicate: Pred,
    calls: usize,
}

impl Judge for CountingJudge {
    fn preserves(&mut self, input: &str, _target: FailKind) -> std::io::Result<bool> {
        self.calls += 1;
        Ok(self.predicate.holds(input))
    }
}

// ---- the corpus ---------------------------------------------------------

struct Case {
    name: &'static str,
    /// A schema, or `None` to use `shape`.
    schema: Option<String>,
    shape: Shape,
    input: String,
    predicate: Pred,
}

fn cases() -> Vec<Case> {
    let mut out = Vec::new();

    let mut add = |name, schema: Option<&str>, shape, input: String, predicate| {
        out.push(Case {
            name,
            schema: schema.map(str::to_string),
            shape,
            input,
            predicate,
        })
    };

    // --- arrays ---------------------------------------------------------
    let mut values: Vec<i64> = (0..100).map(|i| 1000 + i).collect();
    values[37] = 42;
    add(
        "array_delete",
        None,
        Shape::Array,
        format!("100\n{}\n", join(&values)),
        Pred::ContainsValue(42),
    );

    let big: Vec<i64> = (0..40).map(|i| 1_000_000 + i * 13).collect();
    add(
        "array_numeric_boundary",
        None,
        Shape::Array,
        format!("40\n{}\n", join(&big)),
        Pred::AnyAtLeast(500_000),
    );

    // --- matrices -------------------------------------------------------
    // A single column, so only rows can be removed.
    add(
        "matrix_row_delete",
        Some("int R in 1..50\nint C in 1..50\nmatrix G[R][C] in 0..9\n"),
        Shape::Auto,
        "6 1\n0\n0\n7\n0\n0\n0\n".into(),
        Pred::ContainsValue(7),
    );
    // A single row, so only columns can be removed.
    add(
        "matrix_col_delete",
        Some("int R in 1..50\nint C in 1..50\nmatrix G[R][C] in 0..9\n"),
        Shape::Auto,
        "1 6\n0 0 7 0 0 0\n".into(),
        Pred::ContainsValue(7),
    );

    // --- repeat blocks --------------------------------------------------
    add(
        "repeat_iteration_delete",
        Some("int T in 1..10\nrepeat T {\n  int N in 1..10\n  array A[N] in -1000..1000\n}\n"),
        Shape::Auto,
        "3\n2\n5 6\n3\n-7 8 9\n1\n4\n".into(),
        Pred::AnyNegative,
    );

    // --- trees ----------------------------------------------------------
    add(
        "tree_prune_path",
        None,
        Shape::Tree,
        "6\n1 2\n2 3\n3 4\n4 5\n5 6\n".into(),
        Pred::MinTokens(5),
    );
    // Depth 3, so pruning the leaves exposes new leaves and the loop has to
    // run several rounds.
    add(
        "tree_prune_multi_round",
        None,
        Shape::Tree,
        "9\n1 2\n1 3\n1 4\n2 5\n3 6\n4 7\n5 8\n5 9\n".into(),
        Pred::Always,
    );

    // --- graphs ---------------------------------------------------------
    add(
        "graph_edge_delete",
        None,
        Shape::Graph,
        "5 8\n1 2\n2 3\n3 4\n4 5\n5 1\n1 3\n2 4\n3 5\n".into(),
        Pred::MinTokens(4),
    );
    // The case revision 1 of the design note got wrong: vertices 2 and 3 are
    // isolated and that is legal. Compaction maps the retained vertex set,
    // it does not require every label to appear in an edge.
    add(
        "graph_isolated_vertices",
        None,
        Shape::Graph,
        "4 1\n1 4\n".into(),
        Pred::MinTokens(4),
    );
    // Removing the hub kills five edges at once.
    add(
        "graph_vertex_cascade",
        None,
        Shape::Graph,
        "6 5\n1 2\n1 3\n1 4\n1 5\n1 6\n".into(),
        Pred::MinTokens(4),
    );

    // --- constraints ----------------------------------------------------
    add(
        "bounded_numeric",
        Some("int N in 1..50\narray A[N] in 1..1000000\n"),
        Shape::Auto,
        "5\n900000 100 200 300 400\n".into(),
        Pred::AnyAtLeast(500_000),
    );

    // --- more than one reduction path in a single case -------------------
    add(
        "schema_mixed",
        Some(
            "int K in 0..100\nint T in 1..10\nrepeat T {\n  int N in 1..10\n  \
             array A[N] in -1000..1000\n}\n",
        ),
        Shape::Auto,
        "77 3\n2\n5 6\n3\n-7 8 9\n1\n4\n".into(),
        Pred::AnyNegative,
    );

    // --- shared dimensions -----------------------------------------------
    // One count, two arrays, one axis. The predicate needs position 2 of A and
    // position 1 of B, and a single mask projects both, so neither position can
    // be dropped and the survivors are their union.
    add(
        "shared_count_two_arrays",
        Some(
            "int N in 1..10
array A[N] in -100..100
array B[N] in -100..100
",
        ),
        Shape::Auto,
        "4
1 2 3 4
10 20 30 40
"
        .into(),
        Pred::ContainsAll(&[3, 20]),
    );
    // The adversarial one: the same shared axis in two outer instances, each
    // having to keep a different position.
    add(
        "shared_count_nested_instances",
        Some(
            "int T in 1..5
repeat T {
  int N in 1..10
  array A[N] in -100..100
               array B[N] in -100..100
}
",
        ),
        Shape::Auto,
        "2
3
1 7 3
10 11 12
3
4 5 6
20 21 -9
"
        .into(),
        Pred::ContainsAll(&[7, -9]),
    );

    // The section 5 example: a vertex selection induces an edge selection,
    // which must project the weights too.
    add(
        "graph_weighted_edges",
        Some(
            "int N in 1..10
int M in 0..20
graph E[M] vertices N
             array W[M] in 0..99
",
        ),
        Shape::Auto,
        "5 5
1 2
2 3
3 4
4 5
1 5
10 20 30 40 50
"
        .into(),
        Pred::ContainsAll(&[10, 40]),
    );

    // Two graphs sharing both the vertex axis and the edge axis: one vertex
    // selection emits two induced edge masks, and the fixed point intersects
    // them.
    add(
        "graph_two_inducers_one_target",
        Some(
            "int N in 1..10
int M in 0..20
graph E1[M] vertices N
             graph E2[M] vertices N
",
        ),
        Shape::Auto,
        "4 3
1 2
2 3
3 4
1 4
1 3
2 3
"
        .into(),
        Pred::MinTokens(6),
    );

    // --- nested repeats --------------------------------------------------
    // The inner block axis occurs once per outer instance. The predicate needs
    // outer 0's second inner iteration and outer 1's first, so the two
    // occurrences of one AxisId must select different positions.
    add(
        "nested_repeat_occurrences",
        Some(
            "int T in 1..5
repeat T {
  int G in 1..5
  repeat G {
                 int N in 1..5
    array A[N] in -100..100
  }
}
",
        ),
        Shape::Auto,
        "2
2
1
10
3
11 12 13
2
3
20 21 22
1
23
"
        .into(),
        Pred::ContainsAll(&[12, 20]),
    );

    // A graph cascade inside a repeat: the same static edge axis exists once
    // per instance and must induce a different mask in each.
    add(
        "graph_cascade_nested_instances",
        Some(
            "int T in 1..3
repeat T {
  int N in 1..10
  int M in 0..20
               graph E[M] vertices N
  array W[M] in 0..99
}
",
        ),
        Shape::Auto,
        "2
3 3
1 2
2 3
1 3
10 20 30
3 3
1 2
2 3
1 3
60 61 62
"
        .into(),
        Pred::ContainsAll(&[30, 61]),
    );

    // --- index references -------------------------------------------------
    // Selecting the target drops references to what vanished and renumbers
    // the rest.
    add(
        "index_cascade",
        Some(
            "int N in 1..10
array A[N] in 0..999
int K in 0..10
             index I[K] into N
",
        ),
        Shape::Auto,
        "4
10 20 30 40
3
2 4 1
"
        .into(),
        Pred::ContainsAll(&[40]),
    );
    // A graph cascade and an index cascade aiming at one occurrence.
    add(
        "graph_and_index_same_target",
        Some(
            "int N in 1..10
int M in 0..20
graph E[M] vertices N
             index I[M] into N
",
        ),
        Shape::Auto,
        "4 3
1 2
2 3
3 4
4 3 1
"
        .into(),
        Pred::MinTokens(5),
    );
    // The same static index relation once per repeat instance.
    add(
        "index_nested_instances",
        Some(
            "int T in 1..3
repeat T {
  int N in 1..10
  array A[N] in 0..999
               int K in 0..10
  index I[K] into N
}
",
        ),
        Shape::Auto,
        "2
3
10 20 30
2
1 3
3
40 50 60
2
2 3
"
        .into(),
        Pred::ContainsAll(&[30, 40]),
    );

    // A literal edge count cannot absorb a lost edge, so any vertex selection
    // that kills one must be rejected. Without that rule the reducer happily
    // emits a graph with fewer edges than the format declares.
    add(
        "graph_literal_edge_count",
        Some(
            "int N in 2..10
graph E[2] vertices N
",
        ),
        Shape::Auto,
        "4
1 2
3 4
"
        .into(),
        Pred::Always,
    );

    // --- schema trees -----------------------------------------------------
    // The schema tree path had no corpus coverage at all until pruning was
    // routed through the shared pipeline. A vertex-labelled tree: labels are
    // members of the vertex occurrence and follow the pruning.
    add(
        "tree_schema_labels",
        Some(
            "int N in 2..10
tree E vertices N
array Colour[N] in 0..99
",
        ),
        Shape::Auto,
        "4
1 2
1 3
3 4
10 20 30 77
"
        .into(),
        Pred::ContainsAll(&[77]),
    );
    // An index into the vertex axis with a literal length: a prune that would
    // strand a reference has nowhere to put the loss and must be rejected.
    add(
        "tree_schema_index_pins",
        Some(
            "int N in 2..10
tree E vertices N
index I[2] into N
",
        ),
        Shape::Auto,
        "5
1 2
2 3
3 4
4 5
5 1
"
        .into(),
        Pred::Always,
    );

    // --- the unstructured fallback --------------------------------------
    add(
        "raw_tokens",
        None,
        Shape::Raw,
        "a A b b a\nzz b A\n".into(),
        Pred::ContainsToken("A"),
    );

    out
}

fn join(values: &[i64]) -> String {
    values
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

// ---- snapshots ----------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    initial_tokens: usize,
    final_tokens: usize,
    oracle_calls: usize,
    predicate_holds: bool,
    input: String,
}

impl Snapshot {
    fn render(&self, name: &str) -> String {
        format!(
            "# ccmin baseline snapshot for `{name}`\n\
             # Exact counts, not thresholds. Regenerate with:\n\
             #   UPDATE_BENCH=1 cargo test --release benchcases\n\
             initial_tokens = {}\n\
             final_tokens = {}\n\
             oracle_calls = {}\n\
             predicate_holds = {}\n\
             [input]\n{}",
            self.initial_tokens,
            self.final_tokens,
            self.oracle_calls,
            self.predicate_holds,
            self.input,
        )
    }

    fn parse(text: &str) -> Result<Snapshot, String> {
        let mut initial_tokens = None;
        let mut final_tokens = None;
        let mut oracle_calls = None;
        let mut predicate_holds = None;
        let mut input = String::new();
        let mut in_input = false;

        for line in text.lines() {
            if in_input {
                input.push_str(line);
                input.push('\n');
                continue;
            }
            if line.trim() == "[input]" {
                in_input = true;
                continue;
            }
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| format!("bad snapshot line: {line}"))?;
            let value = value.trim();
            match key.trim() {
                "initial_tokens" => initial_tokens = value.parse().ok(),
                "final_tokens" => final_tokens = value.parse().ok(),
                "oracle_calls" => oracle_calls = value.parse().ok(),
                "predicate_holds" => predicate_holds = Some(value == "true"),
                other => return Err(format!("unknown snapshot key: {other}")),
            }
        }

        Ok(Snapshot {
            initial_tokens: initial_tokens.ok_or("missing initial_tokens")?,
            final_tokens: final_tokens.ok_or("missing final_tokens")?,
            oracle_calls: oracle_calls.ok_or("missing oracle_calls")?,
            predicate_holds: predicate_holds.ok_or("missing predicate_holds")?,
            input,
        })
    }
}

fn baseline_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("benchcases")
        .join("baseline")
}

fn tokens(text: &str) -> usize {
    text.split_whitespace().count()
}

fn run(case: &Case) -> Snapshot {
    let schema = case.schema.as_ref().map(|text| {
        schema::parse_schema(text)
            .unwrap_or_else(|e| panic!("{}: schema does not parse: {e}", case.name))
    });
    let model = model::parse_with(
        &case.input,
        ParseOptions {
            shape: case.shape,
            n_index: None,
            schema,
            guess_header: false,
        },
    )
    .unwrap_or_else(|e| panic!("{}: input does not parse: {e}", case.name));

    assert!(
        case.predicate.holds(&model.render()),
        "{}: predicate does not hold on the starting input, so there is nothing to reduce",
        case.name
    );

    let mut judge = CountingJudge {
        predicate: case.predicate,
        calls: 0,
    };
    let reduced = {
        let mut on_step = |_: &Model| {};
        let mut shrinker = Shrinker::new(&mut judge, FailKind::WrongAnswer, &mut on_step);
        shrinker
            .run(&model)
            .unwrap_or_else(|e| panic!("{}: reduction failed: {e}", case.name))
    };

    let text = reduced.render();
    Snapshot {
        initial_tokens: tokens(&model.render()),
        final_tokens: tokens(&text),
        oracle_calls: judge.calls,
        predicate_holds: case.predicate.holds(&text),
        input: text,
    }
}

#[test]
fn baseline_snapshots_match() {
    let update = std::env::var_os("UPDATE_BENCH").is_some();
    let dir = baseline_dir();
    if update {
        std::fs::create_dir_all(&dir).expect("cannot create benchcases/baseline");
    }

    let mut failures = Vec::new();
    for case in cases() {
        let actual = run(&case);

        // Whatever else changes, the reduced case must still fail.
        assert!(
            actual.predicate_holds,
            "{}: reduced input no longer satisfies the predicate",
            case.name
        );
        assert!(
            actual.final_tokens <= actual.initial_tokens,
            "{}: reduction grew the input",
            case.name
        );

        let path = dir.join(format!("{}.snap", case.name));
        if update {
            std::fs::write(&path, actual.render(case.name))
                .unwrap_or_else(|e| panic!("cannot write {}: {e}", path.display()));
            continue;
        }

        let text = std::fs::read_to_string(&path).unwrap_or_else(|_| {
            panic!(
                "missing snapshot {}\nrun: UPDATE_BENCH=1 cargo test --release benchcases",
                path.display()
            )
        });
        let expected = Snapshot::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));

        if expected != actual {
            failures.push(format!(
                "{}\n  initial_tokens  {} -> {}\n  final_tokens    {} -> {}\n  \
                 oracle_calls    {} -> {}\n  input expected:\n{}\n  input actual:\n{}",
                case.name,
                expected.initial_tokens,
                actual.initial_tokens,
                expected.final_tokens,
                actual.final_tokens,
                expected.oracle_calls,
                actual.oracle_calls,
                indent(&expected.input),
                indent(&actual.input),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} benchcase(s) changed:\n\n{}\n\nIf the change is intended, regenerate and \
         review the diff:\n  UPDATE_BENCH=1 cargo test --release benchcases",
        failures.len(),
        failures.join("\n\n")
    );
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The corpus is only useful if it actually covers the reduction paths the
/// refactor will replace.
#[test]
fn corpus_covers_every_reduction_path() {
    let names: Vec<&str> = cases().iter().map(|c| c.name).collect();
    for required in [
        "array_delete",
        "array_numeric_boundary",
        "matrix_row_delete",
        "matrix_col_delete",
        "repeat_iteration_delete",
        "tree_prune_path",
        "tree_prune_multi_round",
        "graph_edge_delete",
        "graph_isolated_vertices",
        "graph_vertex_cascade",
        "bounded_numeric",
        "schema_mixed",
        "nested_repeat_occurrences",
        "shared_count_two_arrays",
        "shared_count_nested_instances",
        "graph_weighted_edges",
        "graph_two_inducers_one_target",
        "graph_cascade_nested_instances",
        "index_cascade",
        "graph_and_index_same_target",
        "index_nested_instances",
        "graph_literal_edge_count",
        "tree_schema_labels",
        "tree_schema_index_pins",
        "raw_tokens",
    ] {
        assert!(names.contains(&required), "corpus lost `{required}`");
    }
}
