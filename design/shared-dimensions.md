# Shared dimensions — design note for v0.5

Status: proposal, revision 3. No code written.

Revision 2 separated Count from Axis. Revision 3 resolves the three questions
that left open: permutations, count-shrink propagation across several axes, and
where axis independence lives in the model. What each revision got wrong is kept
in §13 and §14 rather than folded in silently.

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

```rust
type CountId = usize;
type AxisId  = usize;

struct Count {
    name: String,        // the declared int; rendered to the file
    bounds: Bounds,      // 1..100
    extent: usize,       // authoritative
}

struct Axis {
    count: CountId,                        // cardinality comes from here
    constraints: Vec<SelectionConstraint>,  // see §6
    keep: Vec<usize>,                      // current positions, into the original
}

struct Schema {
    counts: Vec<Count>,
    axes: Vec<Axis>,
    items: Vec<Decl>,
}

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

## 4. The dependency graph has three edge kinds

Revision 1 had two, having merged the first and third.

| edge | fires when | effect |
| --- | --- | --- |
| **count → collection** | extent changes | every collection on an axis of that count is resized |
| **axis → reference** | positions removed | `Index` values remapped; records holding dead references dropped |
| **count → bounds** | extent changes | `Int(DynamicBounds)` values mentioning that count are **clamped** |

The third is what `int K in 1..N` needs and what revision 1 had no mechanism
for. `N: 10 -> 5` with `K = 8` must clamp `K` to at most 5. That is neither a
resize nor a remap.

---

## 5. Cascade as monotone dataflow

`Select` on an axis can invalidate references, which drops records, which
changes another count, which resizes its subscribers. Revision 1 described this
as a worklist and asserted a round bound that does not hold — duplicate
enqueues can exceed it.

Model it as a classic monotone dataflow analysis instead:

```
state: keep[axis] -> bitmask over original positions

apply(axis, mask):
    worklist = { axis }
    keep[axis] &= mask
    while worklist not empty:
        a = pop(worklist)
        for each subscriber of a:
            induced = positions of other axes invalidated by a's current keep
            for (a2, m2) in induced:
                before = keep[a2]
                keep[a2] &= m2
                if keep[a2] != before:      # enqueue only on an actual shrink
                    push(a2)
    for each count c: extent(c) = min |keep[a]| over axes a of c
    trim any axis of c that still retains more than extent(c)
    relabel every Index reference through the retained-position mapping
```

Termination: the state is a finite product lattice of bitmasks, every update is
`&=` so it descends monotonically, and an axis is enqueued only when its mask
strictly shrinks. Bounded by total positions, and now actually provable rather
than asserted.

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

The rule:

```text
extent(c) = min over axes a of count c of |keep[a]|
every axis of c with |keep[a]| > extent(c) is trimmed by the canonical rule
```

**Canonical rule: mirror the mask of the axis that drove the shrink.** Every
axis of a count starts at the same extent — the count was a single number at
parse time — so mirroring is always well defined. It is deterministic, and it is
the least damaging choice if the axes turn out to be correlated after all.
Phase B may explore alternatives later.

Taking the *minimum* rather than any particular axis keeps the update monotone,
so the termination argument above survives unchanged: the extra trimming only
removes positions.

### Cardinality coupling

Axes sharing a count must end with **equal-size** keep sets. This is the
constraint revision 1 got for free by conflating the concepts, and it is the
main new cost of separating them.

Two-phase reduction:

- **Phase A — cardinality.** ddmin on the count. All its axes drop the same
  positions. This is the cheap common case and is exactly the v0.4 behaviour.
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

1. **Cardinality agreement** — for every collection on axis `a`,
   `len(c) == extent(count(a))`. Per-axis for grids. Should be structurally
   unrepresentable, not merely checked.
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
12. **Count minimum** — `extent(c) == min |keep[a]|` over the axes of `c`, and
    no axis of `c` retains more positions than that.

1, 5 and 7 are what the existing harness already covers for the built-in shapes.
2, 6, 9 and 10 are new and are where I expect the first bugs.

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

A permutation introduces *two* axes on one count, which makes a bare
`array color[N]` ambiguous: position axis, value axis, or a third independent
axis sharing the count? The model can express all three; v0.4 syntax cannot say
which. **Axis naming is therefore a syntax requirement, not a nicety** — it
falls out of the litmus test rather than from wanting a richer language. Some
form of `array color[P.positions]` / `array weight[P.values]` is needed before
permutations are usable.

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
- **Axis naming syntax.** Forced by §9. `array color[N]` is ambiguous once a
  count carries more than one axis. Needs designing before permutations ship,
  and it interacts with how independent axes are declared (§2).
- **Are there relations beyond bijection?** Injection would cover "each query
  names a distinct element"; a general many-to-one would cover most reference
  patterns. `Bijection` may be the wrong first abstraction if a weaker one is
  the common case.
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
