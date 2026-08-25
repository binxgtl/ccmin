# Changelog

Notable changes per release. Earlier releases (0.1.0 – 0.4.0) predate this file;
see the git tags and the GitHub releases page for those.

## 0.5.0

The schema language learns to describe how the parts of an input depend on one
another, so reduction can delete a vertex, a position or a value and keep every
other part of the file consistent with it.

### Features

- **Shared counts.** Several declarations may size themselves by one `int`.
  `array A[N]` and `array B[N]` are two views of the same `N` positions, so
  deleting position 3 deletes it from both. This previously errored.
- **Parallel data on a shared count.** Weighted edges (`graph E[M] vertices N`
  beside `array W[M]`) and per-vertex labels (`array Colour[N]`) follow from
  shared counts and need no dedicated syntax.
- **`index I[K] into N`.** `K` one-based references to positions counted by `N`.
  Surviving references are renumbered when positions are deleted; a candidate
  that would dangle is discarded.
- **`permutation P[N]`.** The values `1..=N`, each exactly once, validated on
  read. Reduction preserves permutation-ness in both directions.
- **`P.values`.** The codomain of a permutation, so `array W[P.values]` follows
  `P`'s values while `array Colour[N]` follows its positions. Narrowing either
  end pulls the other with it.
- **Count-referenced numeric bounds.** A range side may name an `int` declared
  earlier in the same block: `array A[N] in 1..N`, `int X in 1..N`. These are
  numeric constraints on magnitudes, not references.
- **Reduction reports when it stops early.** If the reducer hits its internal
  round budget instead of converging, it now says so instead of presenting a
  partly reduced result as final.

### Correctness fixes

- **Reduction now terminates on convergence, not on a fixed round cap.** The
  structural and value passes unlock one another, and the alternation was capped
  at sixteen rounds. Inputs needing more — reachable with `array A[N] in 1..N` —
  were silently truncated and the partial result reported as final.
- **A graph sharing an edge count with another declaration could produce
  unparseable output.** Deleting edges left the parallel array at its old length.
- **Tree pruning bypassed the dependency engine**, so an `index` beside a `tree`
  could survive pointing at a deleted vertex. Tree reduction now runs through
  the same path as everything else, which also made vertex-labelled trees work.

### Architecture

Internal, but it is what the above rests on. Counts, axes, positions and
relations are now separate concepts; every reduction path — arrays, matrices,
repeats, graphs, trees, indexes, permutations — runs through one dependency
engine that reaches a fixed point before writing anything out. Structural and
value scheduling is one shared primitive rather than two copies. The reasoning,
including five design revisions that were wrong and why, is in
`design/shared-dimensions.md` in the repository.

The behavioural corpus under `benchcases/` grew from 25 to 32 cases, each
pinning exact output *and* exact oracle-call counts.

### Known limitations

Deliberate, not oversights:

- **Ranges take a literal or one name, never an expression.** No `N-1`, no
  `min(N, 100)`, no arithmetic.
- **A count has exactly one set of positions.** There is no way to declare two
  same-sized but independently deletable axes; that needs named axes, which are
  left out because nothing has yet required them.
- **No relation constrains only a size.** Every dependency either decides which
  elements survive or bounds a value.
- **Reference behaviour is never inferred from a range.** `array A[N] in 1..N`
  is an ordinary integer array; duplicates are legal and nothing is renumbered.
  Only `index` and `permutation` carry identity.
- **Auto-detected `tree`/`graph` shapes still cover unweighted one-based edge
  lists only.** The richer forms need `--schema`; shape detection is unchanged.
