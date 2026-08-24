# Shared dimensions — design note for v0.5

Status: proposal, revision 6. Implementation under way (v0.5 step 1).

Revision 2 separated Count from Axis. Revision 3 resolved permutations,
count-shrink propagation and where independence lives. Revision 4 fixed two
places revision 3 contradicted its own model. Revision 5 closed the cascade
fixpoint over cardinality events. Revision 6 separates the static arena from
runtime state, after checkpoint 3 of the implementation ran into it. What each
revision got wrong is kept in §13 to §17 rather than folded in silently.

Four sentences the implementation has to keep true throughout:

> **A Count never carries identity.
> An Axis never owns cardinality.
> A Relation never increases a keep-set.
> An arena node never holds instance state.**

Every architectural correction across six revisions has been a violation of one
of them. The fourth was added by revision 6 and is the first one found by
writing code rather than by reading the document.

The model in one line:

```text
Count     how many        cardinality, written to the file
Axis      which           positional identity
Index     that one        a reference naming a position on an axis
Relation  and therefore   legality, and selections induced on other axes
```

Revision 1 collapsed the first two. Revision 2 collapsed the last two, by
reaching for a `permutation` element kind instead of a relation.

---

## 1. What v0.4 does today

For context, since the current shape is what forces the redesign.

`Schema` is a flat list of `Decl` (`Int`, `Array`, `Matrix`, `Tree`, `Graph`,
`Repeat { body }`); `SchemaData` holds a parallel `Vec<Value>`. Reduction walks
both together, addressing values by a `Path` of indices.

**Counts are derived from collections.** `resync()` writes `N = A.len()` after
every structural edit, which is what makes a desynchronised length prefix
unrepresentable.

Three consequences:

1. The collection is primary; the count is its shadow. With two collections on
   one count, `resync()` has two conflicting writes to one slot and the last
   wins silently. That — not conservatism — is why v0.4 rejects sharing.
2. Resync is a repair pass: the model is briefly inconsistent, then fixed.
3. Seven reduction paths (array, matrix rows, matrix cols, repeat iterations,
   graph edges, graph vertices, tree leaves) do one job — pick a subset of
   positions to keep — each with its own bookkeeping.

---

## 2. Four concepts, not one

Revision 1 had a single `Dimension`. That was the root error. There are four
distinct things, and each revision has found one more of them:

**Count** — a cardinality, written to the file as a token. `N` in `array A[N]`.
This is what the grammar constrains: one token, one number, every collection
bound to it has exactly that many elements.

**Axis** — a set of positions *with identity*. Position 3 of an axis is a
particular thing, and if two collections are indexed by the same axis then their
position 3 is the same thing. This is what makes `X[i]`, `Y[i]` the coordinates
of point `i`.

**Reference** (`Index`) — a value that names a position on an axis. A graph
endpoint. When the axis is reduced, references must be remapped, and holders of
dead references must be dropped.

**Relation** — a stated fact tying two axes together, which both restricts legal
selections and *induces* selections on the other side. `Bijection` is the first
one, and it is what a permutation is (§9). Revision 2 missed this layer and
tried to solve permutations with an element kind instead.

An axis has a count. Several axes may share one count. That is the distinction
revision 1 collapsed:

```text
int N
array A[N]
array B[N]
```

The file has one `N`, so `len(A) == len(B)`. It does **not** follow that `A` and
`B` must keep the same positions. With `N: 5 -> 3`, this is a legal candidate:

```text
3
10 30 50        # A kept {0,2,4}
8 9 10          # B kept {1,2,3}
```

Whereas if `A` and `B` are the x and y of the same points, only aligned
selections make sense.

The schema cannot infer which. It has to be declared.

### Both readings are sound

Worth stating plainly, because it sets how much this decision matters: forcing a
shared axis on genuinely independent arrays produces valid inputs (just fewer
reachable ones), and allowing independent selection on genuinely parallel arrays
also produces valid inputs (just semantically scrambled ones — a legal input
describing different points).

So this is a question of **reachability and search cost, not correctness**.
Neither choice can make ccmin confidently wrong. That is a good property: the
default can be revisited later without a compatibility break.

**Proposed default: one axis per count**, with independence opt-in. Aligned
selection is cheaper, is right for the parallel case, and is never unsound. The
opt-in buys reachability where the arrays really are unrelated.

---

## 3. Representation

### The arena is static; extents and masks are not

A schema is a set of *declarations*. An input is a set of *instantiations* of
them. Those are different populations, and the difference is not cosmetic:

```text
int T in 1..10
repeat T {
  int N in 1..10
  array A[N] in -1000..1000
}
```

`N` is one declaration. With `T = 3` it has three live instances, and they hold
different values — `2`, `3`, `1` in the corpus case. A `CountId` names the
declaration. It cannot name the value, because there are three of them.

Earlier revisions put `extent` on `Count` and `keep` on `Axis`. Both are
runtime state living on an arena node, which silently collapses
`(declaration, instance)` down to `declaration`. Everything at the top level of
a schema works, because there each declaration has exactly one instance; the
moment a declaration sits inside a `repeat` body it is wrong.

So:

- **The arena holds only what a schema knows before any input exists**: names,
  bounds, constraints, and the axis topology.
- **Cardinality lives in the instantiated data**, in the `Value::Int` slot the
  count renders to. There is no parallel extent table; one source of truth.
- **A keep-mask lives with the instantiated data whose dimension it describes**,
  for the same reason.

The invariant is therefore not "a Count owns cardinality" but the sharper:

> **Each instantiated count slot owns its cardinality. A `CountId` identifies
> the declaration, and thereby the topological role of that slot.**

```rust
type CountId = usize;
type AxisId  = usize;

// ---- static: the arena, fixed once the schema parses --------------------

struct Count {
    name: String,        // the declared int; rendered to the file
    bounds: Bounds,      // 1..100
    axis: AxisId,        // its default axis
}

struct Axis {
    count: CountId,                         // where cardinality comes from
    constraints: Vec<SelectionConstraint>,  // see §6
}

struct Schema {
    counts: Vec<Count>,
    axes: Vec<Axis>,
    items: Vec<Decl>,
}

// ---- dynamic: per instantiation, alongside the data ---------------------
//
//   cardinality  the Value::Int slot a CountId locates in this instantiation
//   keep-mask    held by the data whose dimension the axis occurrence is
//
// Deliberately not given a representation here. `keep[(AxisId, instance)]` is
// the shape of the state, but committing to a tuple key would be inventing a
// runtime instance-addressing abstraction before anything has shown one is
// needed.

enum Decl {
    Scalar { name: String, ty: Scalar },
    Vector { name: String, axis: AxisId, elem: Elem },
    Grid   { name: String, rows: AxisId, cols: AxisId, elem: Elem },
    Block  { axis: AxisId, body: Vec<Decl> },   // today's `repeat`
}

enum Elem {
    /// A magnitude. Its bounds may mention counts (`in 1..N`), in which case a
    /// shrinking count CLAMPS it.
    Int(DynamicBounds),
    /// A name for a position on an axis. A shrinking axis REMAPS it, and a
    /// holder of a dead reference is dropped.
    Index(AxisId),
    Record(Vec<Field>),
}
```

`Int(DynamicBounds)` and `Index(AxisId)` are deliberately separate and never
inferred from syntax. `int K in 1..N` (choose K of them) and
`int v in 1..N` (vertex v) are written identically and behave completely
differently. `graph` lowers its endpoints to `Index` because the `graph` keyword
carries that meaning; a future `record` syntax will need its own way to say it.

An `int` becomes a `Count` only if some axis is bound to it. Otherwise it is a
`Scalar`, shrunk by value — as v0.4 already treats non-derived ints.

---

## 4. The dependency graph has four edge kinds

Revision 1 had two, having merged the first and third. Revision 3 added
`Relation` as a concept (§2, §9) but left this table at three, which was simply
an inconsistency.

| edge | fires when | effect |
| --- | --- | --- |
| **count → collection** | extent changes | every collection on an axis of that count is resized |
| **axis → reference** | positions removed | `Index` values remapped; records holding dead references dropped |
| **count → bounds** | extent changes | `Int(DynamicBounds)` values mentioning that count are **clamped** |
| **axis ↔ relation ↔ axis** | positions removed | a selection on one axis **induces** a selection on another |

The third is what `int K in 1..N` needs and what revision 1 had no mechanism
for. `N: 10 -> 5` with `K = 8` must clamp `K` to at most 5. That is neither a
resize nor a remap.

The fourth is distinct from `axis → reference` and must not be folded into it.
They do different things:

```text
axis → reference          a target position disappears
                          -> the HOLDER dies (record dropped)

axis → relation → axis    a selection is made here
                          -> a selection is INDUCED there
```

For a bijection the edge is bidirectional (§9). The engine contract:

```text
Relation::induce(changed_axis, state) -> [(AxisId, KeepMask)]
Relation::validate(state)             -> bool
```

with two obligations on every relation:

1. `induced_mask ⊆ current_mask` — a relation may only remove positions;
2. induction is monotone — the same state never yields a larger mask later.

Given those, §5's finite-descending-lattice argument covers relations with no
change. Writing the contract this way is also what lets a second relation be
added without touching the cascade.

---

## 5. Cascade as monotone dataflow

`Select` on an axis can invalidate references, which drops records, which
changes another count, which resizes its subscribers. Revision 1 described this
as a worklist and asserted a round bound that does not hold — duplicate
enqueues can exceed it.

Model it as a classic monotone dataflow analysis instead:

Cardinality requirements are **events in the same queue**, not a pass that runs
after it drains. Satisfying one shrinks an axis, and that shrink can induce
relations, kill reference holders and lower a further count — so a post-pass
would leave the fixpoint open:

```text
axis B shrinks to meet a cardinality
  -> relation induces on axis C
  -> a reference holder dies
  -> another count drops
  -> more axes must meet a new cardinality
  -> ...
```

Two event kinds, both monotone.

The state is per **axis occurrence**, not per `AxisId`: a declaration inside a
`repeat` body has one occurrence per iteration, each with its own mask, and
they shrink independently (§3). Written below as `keep[axis]` for readability,
where `axis` means the occurrence. The lattice argument is unaffected — the
product is over occurrences rather than over static identifiers, and it is
still finite.

```text
state:
    keep[axis]    bitmask over original positions   -- only ever &=
                  one per axis OCCURRENCE, not per AxisId
    target[count] upper bound on cardinality        -- only ever decreases
                  likewise per count occurrence

apply(axis, mask):
    push RestrictAxis(axis, mask)

    while queue not empty:
        RestrictAxis(a, m):
            before = keep[a]
            keep[a] &= m
            if keep[a] == before: continue          # no change, no work
            for (a2, m2) in induced_by(a):          # references and relations
                push RestrictAxis(a2, m2)
            c = count(a)
            if |keep[a]| < target[c]:
                push RequireCardinality(c, |keep[a]|)

        RequireCardinality(c, k):
            if k >= target[c]: continue             # already at least this tight
            target[c] = k
            for each axis a of c where |keep[a]| > k:
                push RestrictAxis(a, a.select_to(k))
                                                    # a picks its OWN positions

    # only once the queue is empty
    for each count occurrence c: write target[c] into c's own Value::Int slot
    relabel every Index reference through its own axis's retained mapping
    validate every relation and constraint
```

`a.select_to(k)` is the axis's own canonical selection. No mask ever crosses an
axis boundary, which is invariant 14.

Termination: the state is the product of a finite lattice of bitmasks and a
vector of integer upper bounds. `RestrictAxis` only intersects;
`RequireCardinality` only lowers a target; both skip immediately when nothing
changes. Every effective event strictly descends in that product, so the queue
drains in at most `sum(original extents) + sum(initial targets)` effective
steps.

### Count shrinking across several axes

Revision 2 wrote `extent(c) = |keep[any axis of c]|`. That assumes every axis of
a count carries the same mask, which holds only when they are the same axis.
Consider:

```text
int N
int M
graph E[M] vertices N
array Foo[M]              # same count, independent axis
```

Shrinking `N` kills edges, so the edge axis goes `10 -> 7` and count `M` becomes
7. `Foo` must also reach 7 — but *which* three positions does it drop? Nothing
in the cascade determines that, because nothing constrained `Foo`.

**A count propagates a number, never a mask.**

```text
extent(c) = min over axes a of count c of |keep[a]|
then, for every axis a of c:  RequireCardinality(a, extent(c))
```

Each axis satisfies that request with **its own** canonical selection. It is
never handed another axis's mask.

Revision 3 said the surplus axes should "mirror the mask of the axis that drove
the shrink". That was wrong, and wrong in the specific way this document exists
to avoid: distinct `AxisId`s mean *different identity*, so a mask of `{0, 2, 4}`
on axis A has no meaning applied to axis B. Mirroring quietly restores the
"position i here corresponds to position i there" assumption that revision 1 was
corrected for.

It also created a problem it could not answer. If two axes shrink in one cascade
and both reach the minimum with different masks, which one "drove" it? Answering
that needs provenance tracking to decide something with no semantic content.
Propagating only a cardinality makes the question disappear.

If an axis cannot produce a legal keep-set of the requested size — its
constraints forbid every subset of that cardinality — the candidate is rejected,
and the cascade may continue to a smaller cardinality. That is a normal
rejection, not an error.

**The backward-compatible case falls out.** `array A[N]` and `array B[N]` both
lower to the same default axis of `N` (§9), so they drop the same positions
because they *are* the same axis — not because a count forced them to. Alignment
is topology, not propagation.

Taking the *minimum* keeps the update monotone, so the termination argument
above survives unchanged: the extra trimming only removes positions.

### Cardinality coupling

Axes sharing a count must end with **equal-size** keep sets. This is the
constraint revision 1 got for free by conflating the concepts, and it is the
main new cost of separating them.

Two-phase reduction:

- **Phase A — cardinality.** ddmin on the count. Every axis of the count
  reduces to the chosen size `K` using its own canonical selection. When the
  declarations share one axis — the default, and every v0.4 schema — they drop
  the same positions because they are the same axis. This is the cheap common
  case and reproduces v0.4 exactly.
- **Phase B — realignment.** At fixed cardinality, let an individually-declared
  independent axis swap *which* positions it keeps. A permutation search, more
  expensive, and only worth running for axes declared independent.

Phase A alone reproduces everything v0.4 can do. Phase B is where independent
axes earn their keep, and it can be deferred past v0.5 without blocking
anything.

---

## 6. Selection constraints compose

Revision 1 tagged each dimension with a kind (plain / tree / graph) and picked a
policy from it. That assumes one subscriber decides. It does not:

```text
tree T vertices N
array color[N]
graph G[M] vertices N
```

`N` is subscribed to by a tree and a graph. Legal selections must satisfy
*every* subscriber, so legality is an intersection:

```rust
enum SelectionConstraint {
    MinCardinality(usize),   // from the count's `in lo..`
    Connected,               // a tree subscriber
    // ... future: non-empty, permutation-closed
}
```

`Connected` from the tree wins over the graph's "anything goes". Two trees on
one vertex set need a selection connected in both.

**Independence is not a constraint.** Two declarations on distinct `AxisId`s
sharing a `CountId` already say "same size, different identity"; two on the same
`AxisId` say "same identity". The topology carries it. `SelectionConstraint` is
for legality predicates — `Connected`, `NonEmpty` — and an `Independent` variant
would be a category error.

The candidate *generator* may stay specialised (leaf pruning for trees), but
**validity is checked compositionally**. That split matters: a specialised
generator that happens to violate another subscriber's constraint is caught
rather than silently accepted.

---

## 7. Invariants

Revision 1's compaction invariant was wrong and would have failed correct
inputs. Corrected set:

0. **The arena holds no instance state** — no `Count` or `Axis` node carries
   an extent or a mask. Structural, and the reason the rest of this list is
   phrased per occurrence.
1. **Cardinality agreement** — for every collection on axis occurrence `a`,
   `len(c)` equals the cardinality in the count slot `a` resolves to in *that*
   instantiation. Per-axis for grids.
2. **Cardinality coupling** — all axes sharing a count have equal-size keep sets.
3. **Count bounds** — `bounds(c).lo <= extent(c) <= bounds(c).hi`.
4. **Reference range** — every `Index(a)` value lies in `1..=extent(count(a))`.
5. **Relabelling is a bijection** — the map from retained positions to new
   labels is an order-preserving bijection onto `1..=extent`. *Not* "every label
   appears in some reference": `N = 4` with the single edge `1 4` leaves
   vertices 2 and 3 isolated, references are `{1, 4}`, and that is a perfectly
   legal graph. Revision 1's invariant would have rejected it — and contradicted
   `induced()`, which already does the right thing.
6. **Clamped bounds hold** — every `Int(DynamicBounds)` satisfies its bounds
   under the *current* extents.
7. **Round trip** — `parse(render(x)) == x`.
8. **Monotonicity** — no step increases any extent or the total size.
9. **Cascade termination** — `apply` reaches a fixpoint; masks only descend.
10. **Constraint satisfaction** — every accepted candidate satisfies every
    subscriber's constraints, checked as an intersection, not by kind.
11. **Bijection preservation** — for a `Bijection(a, b)`, the mapping restricted
    to `keep[a]` is a bijection onto `keep[b]`. Follows by construction when the
    cascade uses image and preimage (§9), so this is a check that the cascade
    was implemented as designed.
12. **Count minimum** — a count occurrence's cardinality is
    `min |keep[a]|` over its own axis occurrences, and none of them retains
    more positions than that.
13. **Relations only remove** — for every `induce` result,
    `induced_mask ⊆ current_mask` of the target axis. This is the testable form
    of *a Relation never increases a keep-set*, and it is what keeps §5's
    termination argument valid once relations exist.
14. **No mask crosses an axis boundary** — a keep-mask is only ever applied to
    the axis it was computed for. The testable form of *a Count never carries
    identity*: count propagation passes a cardinality, so any code path handing
    one axis's mask to another is a bug by construction.
15. **Instances are independent** — one occurrence of a count declared inside a
    `repeat` body may differ from another, and editing one iteration must
    neither overwrite nor reinterpret another iteration's slot. The testable
    form of *an arena node never holds instance state*, and the exact case that
    caught revision 5.

1, 5 and 7 are what the existing harness already covers for the built-in shapes.
2, 6, 9 and 10 are new and are where I expect the first bugs. 0 and 15 are the
pair that revision 6 added; 15 lands as a regression test in checkpoint 3.

---

## 8. Records

```text
repeat M {
  int u in 1..N     # must be declared a reference, not inferred
  int v in 1..N
  int w in -1000000000..1000000000
}
```

`Vector { axis: M_axis, elem: Record([Index(N_axis), Index(N_axis), Int(..)]) }`.

"Drop records holding a dead reference" is the general form of "drop edges with
a removed endpoint", so `graph` becomes sugar over `record` — but only once the
syntax can *state* that `u` is a reference. Revision 1 claimed records and
shared dimensions were the same feature; they are not. They share the cascade
engine and differ in semantics. That distinction is the whole content of §3's
`Int` / `Index` split.

---

## 9. Permutations are a relation, not an element kind

Revision 2 leaned toward a `permutation` element kind. That is wrong for the
same reason inferring `Index` from `1..N` is wrong: permutation is not a
property any element has. The element `3` does not know whether it sits in a
permutation. The property belongs to the whole collection:

```text
len(P) == N,   1 <= P[i] <= N,   all distinct,   set(P) == 1..N
```

That is exactly a **bijection between two axes**:

```text
P: Vector { axis: PositionAxis, elem: Index(ValueAxis) }
   Bijection(PositionAxis, ValueAxis)
   count(PositionAxis) == count(ValueAxis) == N
```

Two axes, one count — the structure §2 already introduced. No new element kind
is needed, only a relation:

```rust
enum Relation {
    Bijection { domain: AxisId, codomain: AxisId, mapping: DeclId },
}
```

### It preserves permutation-ness for free

The cascade does the work. A selection on positions induces one on values
through the **image** of the mapping; a selection on values induces one on
positions through the **preimage**. Because the mapping is bijective the two
sides always have equal cardinality, which is exactly what the shared count
requires.

`N = 5`, `P = [3, 5, 1, 4, 2]`, with `color` on the position axis and `weight`
on the value axis. Simulated against the model:

| operation | N | P | color | weight | still a permutation |
| --- | --- | --- | --- | --- | --- |
| keep positions {1,3,5} | 3 | `3 1 2` | c1 c3 c5 | w1 w2 w3 | yes |
| keep positions {2,4} | 2 | `2 1` | c2 c4 | w4 w5 | yes |
| drop value 4 | 4 | `3 4 1 2` | c1 c2 c3 c5 | w1 w2 w3 w5 | yes |
| keep values {1,2} | 2 | `1 2` | c3 c5 | w1 w2 | yes |

No clamping, no dropping of arbitrary elements, no per-integer special case —
and `color` follows positions while `weight` follows values, each through its
own axis. This is the litmus test for the whole Count/Axis/Reference split, and
the model passes it in both directions.

### Do not infer it from `in 1..N`

The same discipline as `Int` versus `Index`. These are different types:

```text
array A[N] in 1..N      Vector<N_axis, Int(bounds = 1..N)>
                        N shrinks  ->  values are CLAMPED
                        [1, 1, 5, 2, 5] is perfectly legal

permutation P[N]        Vector<PositionAxis, Index(ValueAxis)>
                        + Bijection(PositionAxis, ValueAxis)
                        N shrinks  ->  positions selected, values REMAPPED
```

`array A[N] in 1..N` must not become a permutation because a bound looks like
one.

### What it forces on the syntax

A permutation introduces *two* axes on one count, so a bare `array color[N]`
becomes ambiguous: position axis, value axis, or a third independent axis
sharing the count? The model can express all three; v0.4 syntax cannot say
which. **Axis naming is a requirement for expressivity**, and it falls out of
the litmus test rather than from wanting a richer language.

The semantics are settled even though the spelling is not:

- **Every count has a default axis.** A bare `array A[N]` names it. Two bare
  declarations on the same count therefore land on the *same* axis and stay
  aligned — not because anything forces them to, but because they are one axis.
  Every v0.4 schema keeps its current behaviour by construction.
- **Axes can be declared and named**, and a declaration may index one directly.
  Two declarations naming different axes of one count are independent; they
  share cardinality and nothing else.
- **A permutation exposes two projections**, its domain and its codomain, and a
  declaration may index either.

Illustrative spelling only — not a syntax proposal:

```text
int N
axis points[N]              # a named axis of N
array X[points]             # parallel: same axis
array Y[points]

axis aidx[N]                # two axes, one count
axis bidx[N]
array A[aidx]               # independent: same size, different identity
array B[bidx]

permutation P[N]
array color[P.positions]    # follows the domain
array weight[P.values]      # follows the codomain

array Legacy[N]             # desugars to N's default axis
```

What matters is that **independence is expressed by naming a different axis**,
never by a modifier such as `array A[N] independent`. Independence is not a
property of an array; it is which axis the array points at (§6).

---

## 10. Roadmap

```
0. Freeze v0.4 with benchcases.
1. Split Count / Axis / Reference in the representation. No syntax.
2. Fix dependency-graph + fixpoint semantics (§4, §5).
3. Port array / matrix / repeat.
4. Port graph / tree onto Index + SelectionConstraint.
5. Only then: decide whether shared N means same count or same axis,
   and add the syntax to say which.
```

Step 0 first is not optional. The refactor collapses seven reduction paths into
one; without a fixed corpus asserting tokens-out and oracle-calls-out per case,
there is no way to tell whether the cleaner abstraction reduces as well as the
messy one. `proptest.rs` already drives the reducer through a pure in-memory
`Judge`, so oracle counts are deterministic and need no processes.

---

## 11. Falsifiable tests

Revision 1 proposed: *"allowing sharing should require deleting a validation
check; if it needs new code the model is wrong."*

That test is void. Once same-count and same-axis are distinct, allowing sharing
needs new **information** in the syntax — which is not evidence of a bad
abstraction, only that v0.4's syntax cannot express the distinction.

Revision 2's replacement was also wrong, in a smaller way: it proposed that
independent axes need "one new `SelectionConstraint` variant". Independence is
not a constraint — it is which `AxisId` a declaration points at (§6). Sharper:

> Adding syntax for independent axes should only change **which `AxisId` a
> declaration lowers to**. It must not introduce a new semantic operation.

The parser decides `shared identity -> same AxisId` versus `same count only ->
distinct AxisIds sharing a CountId`. The cascade already knows the Count/Axis
relation and never needs to know what the user typed.

A second, cheaper one:

> Step 4 should **delete** `GraphCase::induced` and the bespoke tree pruning,
> replacing them with an `Index` cascade plus a `Connected` constraint. If both
> survive alongside the general mechanism, the generalisation did not happen.

---

## 12. Open questions

- ~~**Permutations.**~~ Resolved in §9: a bijection between two axes, not an
  element kind. `array A[N] in 1..N` stays a clamped `Int`; `permutation P[N]`
  becomes two axes plus a relation.
- ~~**Axis naming syntax.**~~ Semantics settled in §9: default axis per count,
  named axes for independence, projections for permutations. The *spelling* is
  still open, but nothing downstream depends on it.
- ~~**Are there relations beyond bijection?**~~ Resolved: `Bijection` is the
  right first relation, and the two candidates are not siblings.
  - *Many-to-one already exists.* `Vector<M_axis, Index(N_axis)>` lets any
    number of records reference vertex 3; when vertex 3 goes, its holders are
    dropped. `Index` is many-to-one reference semantics, so a relation for it
    would be redundant.
  - *Injection is a collection constraint, not a relation.* "These M queries
    name distinct vertices" is `AllDistinct(references)`. Shrinking the domain
    does **not** force the codomain down to the image, because the codomain may
    legitimately contain unused elements.
  - *Bijection is genuinely different*: total, injective, surjective and equal
    in cardinality, which is exactly why selection must propagate **both** ways.

    Keep the engine API abstract (`induce` / `validate`, §4) so a second
    relation costs no cascade changes, but do not pre-generalise into a
    `Mapping { injective, surjective, total, .. }` lattice before a real second
    use case exists. That direction ends in a relation-algebra side project
    rather than a test-case minimiser.
- **Can an axis exist with no collection?** `graph ... vertices N` has a vertex
  set with no stored data; its extent is the count itself. Tree and graph
  already behave this way, so probably yes, but it means an axis may have no
  subscriber holding data while still carrying constraints.
- **Is phase B worth building?** Independent axes only pay off when a bug needs
  misaligned selections. I cannot yet name a real CP bug that does. It may be
  right to ship the Count/Axis split for *correctness of the model* and never
  implement realignment.
- **Should mismatched sharing be rejected?** A schema author could write
  `array A[N]` and `array B[N]` for a format that actually writes the length
  twice. Detecting that at validation time may be worth the error message.
- **Grid axes.** `matrix G[R][C]` gives rows and columns separate axes. Should a
  `array rowLabel[R]` share the *row axis* of `G` or merely its count? Same
  question as §2, one level down, and probably the same answer.

---

## 13. What revision 1 got wrong

Kept deliberately: the errors were in the reasoning, not the details.

1. **Conflated cardinality with identity.** Proved that one count token forces
   `len(A) == len(B)`, then used that to claim `A` and `B` must keep the same
   positions. The second does not follow. Revision 1's own open question #4
   asked exactly this and answered "I could not construct one" — the
   counterexample is any two independent sequences of the same length.
2. **Claimed `int u in 1..N` *is* a reference.** It is a bound. `int K in 1..N`
   is a magnitude that must be clamped; `int v in 1..N` is a reference that must
   be remapped. Not inferable from syntax, so they must be separate element
   kinds. This also revealed a missing third edge kind in the dependency graph.
3. **Wrote a compaction invariant that rejects legal inputs.** Required every
   label in `1..=extent` to appear in some reference; isolated vertices are
   legal and common. It also contradicted `induced()`, which was already
   correct. Had it been implemented as written, the property harness would have
   produced false failures.
4. **Asserted a termination bound instead of proving one.** `sum(extent)` rounds
   holds only if every pop strictly decreases; duplicate enqueues break it.
   Monotone dataflow with enqueue-on-shrink gives a real proof.
5. **Attached the selection policy to the dimension.** Assumed a single
   subscriber determines legality. With several subscribers on one axis,
   legality is the intersection of their constraints.

Corrections 1, 2 and 5 came from external review; 3 and 4 were confirmed against
the v0.4 source while checking them.

---

## 14. What revision 2 got wrong

1. **Reached for a `permutation` element kind.** Permutation is a collection
   property, not an element property — the value `3` cannot know whether it sits
   in one. It is a bijection between two axes, which the Count/Axis split of
   revision 2 had already made expressible. Revision 2 introduced the machinery
   and then failed to use it.
2. **Left count-shrink propagation undefined for several axes.**
   `extent(c) = |keep[any axis of c]|` is only correct when every axis of the
   count carries the same mask. With independent axes it says nothing about
   which positions the unconstrained ones drop. Fixed by the minimum rule and a
   canonical mirror in §5.
3. **Treated axis independence as a `SelectionConstraint`.** A category error:
   independence is the topology of the model — which `AxisId` a declaration
   points at — not a predicate on legal selections. `SelectionConstraint` is for
   `Connected` and its kin.

All three came from external review. The permutation litmus test in §9 was then
simulated against the model before being written down, in both the position-to-
value and value-to-position directions.

---

## 15. What revision 3 got wrong

Both found by external review, and both are the same failure: adding a concept
and then not propagating it through the rest of the document.

1. **Added `Relation` as a concept but left the dependency graph at three edge
   kinds.** §9 defines a bijection as inducing selections in both directions,
   which is plainly a fourth edge. Folding it into `axis → reference` would have
   been wrong too: a dead reference kills its *holder*, whereas a relation
   induces a *selection on another axis*. Fixed in §4, with an `induce` /
   `validate` contract so a second relation does not touch the cascade.

2. **Had a count propagate a mask.** Revision 3 said surplus axes should "mirror
   the mask of the axis that drove the shrink" — which reinstates the exact
   assumption revision 1 was corrected for, since distinct axes mean distinct
   identity and a mask from one has no meaning on another. It also needed
   provenance tracking to break ties between two axes shrinking at once, in
   order to decide something with no semantic content. Fixed in §5: a count
   propagates a cardinality and each axis picks its own positions.

Both were violations of the three sentences at the top of this document, which
is why they are now stated there. The second in particular is why the summary
leads with *a Count never carries identity*.

---

## 16. What revision 4 got wrong

One thing, and unlike the earlier revisions it was an algorithm bug rather than
a modelling one — the concepts were right, the scheduling was not.

**`RequireCardinality` ran after the worklist drained.** Satisfying a
cardinality shrinks an axis, and that shrink can induce relations, kill
reference holders and lower a further count. All of that needed to be back on
the queue, so the fixpoint was not closed and the pass could terminate in a
state that still had pending work.

Fixed in §5 by making cardinality requirements events in the same queue as axis
restrictions. The termination argument came out stronger: the state is now
explicitly the product of a bitmask lattice and a vector of integer upper
bounds, with every effective event strictly descending in it.

Worth noting for the implementation: this class of bug does not show up in a
model review, only in reading the pseudocode as an operational semantics. The
same is likely true of whatever is still wrong here, which is the argument for
stopping the design and writing benchcases.

---

## 17. What revision 5 got wrong

One thing, and it is the first error in this document found by writing code
rather than by reading it.

**`extent` was a field on `Count`, and `keep` a field on `Axis`.** Both are
runtime state on an arena node. A `CountId` names a *declaration*; a
declaration inside a `repeat` body has one instance per iteration, each with its
own value. Putting the value on the node collapses `(declaration, instance)` to
`declaration`, and the collapse is invisible for any schema whose declarations
are all top level — which is every example in revisions 1 through 5.

Caught by checkpoint 3 of the implementation, immediately, on the corpus case
`repeat_iteration_delete`:

```text
int T in 1..10
repeat T {
  int N in 1..10
  array A[N] in -1000..1000
}
```

with `T = 3` and `N` taking `2`, `3`, `1`. A flat `extents[CountId]` table held
only the last write, and a consistency assertion comparing the table against the
declared slots failed on the first candidate:

```text
initial: count N is 1 in the table but 2 in the slot
```

Fixed in §3 by separating the static arena from instantiated state, and in §5 by
making the cascade state per axis occurrence rather than per `AxisId`. The
sharper invariant that replaces "a Count owns cardinality" is in §3.

Two notes on process, since this is the fifth architectural correction:

The failure was not reachable by review. Every earlier correction came from
reading the model; this one required an implementation and a corpus case with a
nested declaration. §16 predicted exactly that, and the prediction cost one
reverted checkpoint to confirm.

The reduced checkpoint 3 that follows does *not* introduce instance frames.
Nothing in this failure shows that a runtime instance-addressing abstraction is
required — only that cardinality must not be duplicated. Building frames now
would fold two independently verifiable decisions into one change.
