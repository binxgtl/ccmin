# benchcases

Behavioural snapshots of the reducer, frozen before the v0.5 refactor.

The refactor collapses seven reduction paths — array elements, matrix rows,
matrix columns, repeat iterations, graph edges, graph vertices, tree leaves —
into a single `Select` on an axis. Without a fixed corpus there is no way to
tell whether the cleaner abstraction reduces as well as the messy one it
replaces.

The cases live in [`src/benchcases.rs`](../src/benchcases.rs); this directory
holds only the recorded results.

## Two tiers

**`baseline/` — exact v0.4 behaviour.** Every field is asserted exactly,
including the oracle call count. The judge is a pure in-memory predicate, so
these numbers are deterministic; they are not a portable performance benchmark
and are not meant to be one. The purpose is to notice *immediately* when a
change alters the search path at all.

Exactness is the whole point. Changing ddmin's restart granularity from `n - 1`
to `2` was tried as a check on this suite: it left every reduced input
byte-identical and changed only the call counts, 14→16, 12→14 and 18→20 on the
three graph cases. A `<= 100` style threshold would have reported success.

**`capability/` — quality floor only.** Does not exist yet, deliberately.
After the refactor it should assert the properties that must hold regardless of
how the search works — reduced size under some K, failure still reproduced —
so the optimiser stays free to improve without a snapshot churn on every
tweak. Adding it before the refactor would just be two copies of the same
assertions.

## Running

```bash
cargo test --release benchcases
```

When a change to the algorithm is deliberate, regenerate and **read the diff**:

```bash
UPDATE_BENCH=1 cargo test --release benchcases
```

A snapshot diff is a review artifact, not a chore. If a change moves a call
count and you cannot say why, that is the suite doing its job.

## What each field means

| field | meaning |
| --- | --- |
| `initial_tokens` | whitespace tokens in the starting input |
| `final_tokens` | whitespace tokens after reduction |
| `oracle_calls` | times the reducer asked the judge — the search path |
| `predicate_holds` | the reduced input still fails; asserted separately too |
| `[input]` | the reduced input verbatim |

## Coverage

One case per reduction path, plus a few that combine them. `corpus_covers_every_reduction_path`
fails if one is dropped.

| case | what it pins down |
| --- | --- |
| `array_delete` | element deletion |
| `array_numeric_boundary` | magnitude boundary search |
| `matrix_row_delete` | row deletion, single column so columns cannot move |
| `matrix_col_delete` | column deletion, single row so rows cannot move |
| `repeat_iteration_delete` | whole-iteration deletion |
| `tree_prune_path` | leaf pruning against a floor |
| `tree_prune_multi_round` | pruning that exposes new leaves, several rounds |
| `graph_edge_delete` | edge deletion |
| `graph_isolated_vertices` | vertex deletion and label compaction |
| `graph_vertex_cascade` | one vertex removal killing five edges |
| `bounded_numeric` | declared bounds flooring both length and value |
| `schema_mixed` | scalar, repeat, array and numeric in one case |
| `nested_repeat_occurrences` | one axis occurring once per outer instance |
| `shared_count_two_arrays` | one count, two collections, one mask |
| `shared_count_nested_instances` | a shared axis selecting differently per instance |
| `graph_weighted_edges` | a vertex selection inducing an edge selection |
| `graph_two_inducers_one_target` | two induced masks intersecting |
| `graph_cascade_nested_instances` | one cascade per repeat instance |
| `raw_tokens` | the unstructured fallback |

`graph_isolated_vertices` is the one to keep if you keep only one. `N = 4` with
the single edge `1 4` leaves vertices 2 and 3 isolated, which is perfectly
legal. Revision 1 of the design note wrote an invariant requiring every label to
appear in some reference, which would have rejected it — and would have
contradicted `induced()`, which was already correct. Compaction maps the
*retained vertex set*; it does not require every label to be used.
