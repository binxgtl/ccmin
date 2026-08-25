# Shared dimensions — design note for v0.5

Status: revision 12. Implementation under way; every reduction path runs
through the cascade engine, the first bidirectional relation is built, and the
first *non*-relational dependency is built beside it.

Revision 2 separated Count from Axis. Revision 3 resolved permutations,
count-shrink propagation and where independence lives. Revision 4 fixed two
places revision 3 contradicted its own model. Revision 5 closed the cascade
fixpoint over cardinality events. Revision 6 separates the static arena from
runtime state, after checkpoint 3 of the implementation ran into it. What each
revision got wrong is kept in §13 to §18 rather than folded in silently.
Revision 7 separates an induced *selection* from a cardinality *requirement*,
after the first real cascade made the difference concrete. Revision 8
records the abstraction that two relation forms actually justified, and the
four larger ones they did not. Revision 9 routes tree pruning through the
same pipeline, which removed the last bespoke reducer and fixed a live bug.

Five sentences the implementation has to keep true throughout:

> **A Count never carries identity.
> An Axis never owns cardinality.
> A Relation never increases a keep-set.
> An arena node never holds instance state.
> Propagation determines what survives; projection only materialises the
> already-decided final state.**

Every architectural correction so far has been a violation of one of them. The
fourth was added by revision 6 and the fifth by revision 9, and both were found
by writing code rather than by reading the document. The fifth is the one the
tree path violated for four revisions without anyone noticing: pruning wrote its
result directly, so for a tree, projection *was* the decision.

### Positions and labels

Two numberings appear throughout and are deliberately never mixed:

```text
position   zero-based, internal   an index into an axis's current domain;
                                  what a keep-mask contains
label      one-based, in the file what a reference value holds, and what a
                                  vertex is called in an edge line
```

A mask of `[0, 2]` keeps the first and third positions, which render as labels
`1` and `2` after compaction. Conversion happens at exactly two places: reading
a reference (`label - 1`) and renumbering one after the fixed point
(`position within the survivors, + 1`). Everywhere else is positions.

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

### Two kinds of induced obligation

These look alike and are not, and collapsing them would be another "two things
treated as one" of the sort §13 to §17 record:

```text
InducedSelection(axis occurrence, mask)   determines identity AND cardinality
RequireCardinality(axis occurrence, k)    determines size only
```

A related distinction, on the value side: **an `index` entry is an identity, not
a magnitude.** Its value names a position; it is not a number with a size that
could usefully be made smaller. The value pass therefore never touches one --
driving a reference toward zero would put it outside `1..=N` and produce an
input that cannot be re-parsed. This falls out of matching on the declaration
rather than the value, and is tested rather than assumed. It is the same
distinction as `int K in 1..N` versus `index I[K] into N`, one level down.

A vertex selection induces a **selection** on the edge axis: not "keep two
edges" but "keep edges 0, 3 and 4". Encoding it as a cardinality requirement
would throw away which edges survived, and every member of that axis would then
be free to keep a different two.

A cardinality requirement is the weaker thing, and is what a count needs when
two of its axes must end the same size without either determining the other's
positions.

It still has no producer. Both cascades that exist are positional: a vertex
selection says *which* edges survive, and an index selection says *which*
references survive. Reassessed after `Index` landed, since that was the obvious
candidate for a size-only rule, and it is not one -- an index knows exactly
which of its entries point at something that vanished. Until a relation turns
up whose information really is "this many" rather than "these ones",
`RequireCardinality` would be a type with no inhabitants.

For a bijection the edge is bidirectional (see the permutations section).

### What two relation forms justified

Earlier revisions sketched a contract of `Relation::induce` and
`Relation::validate`. With two forms built -- graph vertices inducing edges, and
`index` references -- the genuinely common part turned out smaller than that,
and is what got extracted:

```text
induced_selections(schema, data, occurrence, source_mask) -> [Induced { axis, keep }]
```

Both forms have one shape: observe a narrowed occurrence's mask, name a sibling
axis in the same block, return positional survivors. What was duplicated
verbatim was not the rules but the *emit path* after them -- find the target
occurrence, take its domain, `narrow`, enqueue -- so that is what collapsed to
one copy. The rules stayed two plain functions.

The obligations are unchanged, and are still what makes the lattice argument
hold:

1. `induced_mask` is a subset of `current_mask` -- a relation only removes;
2. induction is monotone -- the same state never yields a larger mask later.

The second common thing is a *validation* rule, not an induction one:

> **A surviving reference must name a surviving position.**

Both forms can violate it and both now reject rather than repair. `index`
already did. A graph endpoint used to be dropped silently, which is wrong for a
reason only visible once counts are shared: dropping an edge leaves the edge
count describing data that is no longer there, and a co-sized member would keep
the longer mask. Making it symmetric also fixed a latent case with no sharing at
all -- `graph E[2] vertices N`, where a literal count cannot absorb a lost edge,
and the reducer previously emitted a graph with fewer edges than the format
declares.

### Rejected, with reasons

- **A `Relation` trait with dynamic dispatch.** The rule set is closed and known
  at a single call site. Dispatch would buy indirection and nothing else, and
  makes two rules harder to read than two named functions do.
- **A `Relation` enum with `induce` and `validate` methods.** The forms are
  *discovered* differently: the graph rule from a member's role on the narrowed
  occurrence, the index rule by scanning the block for declarations pointing at
  it. A uniform relation value would have to be materialised first, which is
  more machinery than the thing it abstracts.
- **Unifying projection.** Graph relabelling and index renumbering are the same
  idea -- rewrite survivors, drop holders of the dead -- but the graph does both
  inside one `Value::Graph`, which bundles the vertex count with the edge list.
  Sharing the mechanism would mean splitting that value: a larger, less local
  change than the duplication costs.
- **A single post-fixpoint `validate_references` pass.** Tempting for symmetry,
  but the graph's check belongs inside the relabelling it already does, and a
  pass existing to hold one extra call site is not an abstraction.

The test of whether this is the right size: a third relation form should need a
rule function and one line in `induced_selections`, and nothing else.

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

**What was actually built (step 9).** Only the third bullet. `permutation P[N]`
declares the relation and `P.values` names its codomain; `array W[P.values]`
follows it. Neither `axis` nor `P.positions` was implemented:

- `P.positions` would be an alias for the count's default axis with identical
  runtime meaning, since a permutation's domain *is* that axis. `array C[N]`
  already follows the domain. A second spelling for one thing is not
  expressivity.
- A general `axis` declaration still has exactly one inhabitant -- the
  permutation codomain -- so it stays unbuilt on the same rule that keeps
  `RequireCardinality` unbuilt. The two-independent-axes-on-one-count case in
  the sketch above remains inexpressible, and that is the honest state.

One gap the litmus surfaced: `array A[N] in 1..N` is still not expressible,
because bounds take integer literals, not counts. Section 9's own example uses
it. The implementation tests the static-bound analogue instead and asserts that
`A.values` is rejected on a non-permutation.

---

## 10. Roadmap

```
0. Freeze v0.4 with benchcases.                              done
1. Split Count / Axis / Reference in the representation.     done
2. Make a selection belong to an axis occurrence.            done
   Shared counts fall out: one mask, every member.
3. Extract vertex -> edge positional induction.              done
   The first real "selection on A induces selection on B".
4. Minimal occurrence-local worklist around induced          done
   selections. One event, intersection-only merge, fixed
   point before projection.
5. Index elements, generalising graph endpoints.             done
   Producer #2 on the existing worklist; no redesign.
6. RequireCardinality, once a relation induces only a size.
   Still no producer: both cascades are positional.
7. Generalise validate / induce once two relation forms      done
   exist to prove the abstraction. One induce seam, one
   emit path, one shared validation rule.
8. Route tree pruning through the same pipeline.             done
   The last bespoke reducer; fixed a live bug and made
   vertex-labelled trees expressible.
9. Permutation as the first bidirectional relation.          done
   Producer #3, and the first that induces *into* an axis
   it is also induced *from*.
10. Count-referenced numeric bounds.                         done
   Not a producer at all: a value dependency that takes no
   part in propagation. See section 21.
11. One scheduler, terminating on convergence.               done
   Fixed a real truncation bug. Feature freeze for v0.5.
```

Step 8 was not on the numbered list. Step 6 is the only item that was, and it is
blocked, so the choice came from asking which architectural boundary was least
proven. That was section 6's claim that a specialised *generator* can coexist
with shared *validity*, which nothing tested because the only specialised
generator never reached the shared path.

The order changed at step 3. Earlier revisions put `Index` before the cascade;
building it first would have meant writing `induce` with no caller that
enqueues anything. Extracting the one cascade that already existed gives the
worklist something real to carry before it is written.

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

---

## 18. What implementing §5 exposed

Not a modelling error this time -- a shipped bug and a naming risk, both found
by writing the code.

**The sharing restriction was aimed at the wrong role.** Step 2 allowed several
declarations to share a count, and refused only *vertex* counts, on the grounds
that a vertex selection also changes the edge count. The reasoning was right and
the target was wrong: nothing stopped an *edge* count being shared, and that is
the broken configuration. This schema passed validation:

```text
int N in 1..10
int M in 0..20
graph E[M] vertices N
array W[M] in 0..99
```

and selecting vertices produced output that cannot re-parse -- `M` claiming four
edges, one edge line, four weights -- because `resync` saw the graph say one and
`W` say four, and the last write won. Exactly the failure §1 attributes to v0.4,
reintroduced by allowing sharing before the cascade existed. Caught by probing
the combination rather than by any test, which is the argument for probing a new
capability's *interactions* and not only its intended use.

Fixed by this checkpoint: the vertex selection now induces a positional
selection on the edge axis, and that induced selection projects every member of
it. Sharing a graph vertex count is legal as a result; a tree's is still not,
because pruning is a sequence of selections against a changing leaf set.

**Induced selections and cardinality requirements are different.** Recorded in
§4. The temptation is to run everything through one event type, and a vertex
cascade encoded as `RequireCardinality(k)` would silently lose which edges
survived -- the members of that axis would each be free to keep a different `k`.

One process note. A test written *for* a failure mode is not automatically a
test *of* it. The nested-instance test asserted that a replayed mask keeps `E`
and `W` consistent, which a replayed mask does: it projects both wrongly, but
equally. Only pinning which edge survived caught it, and until then the
benchcases were the sole detector. Assert identity, not just consistency.

---

## 19. What routing the tree exposed

Tree pruning was the last reduction path that did not go through the cascade
engine: it wrote the pruned tree and called `resync`. That was a live bug, not
an untidiness. This schema is legal --

```text
int N in 2..10
tree E vertices N
index I[2] into N
```

-- and pruning it produced references to vertices that no longer existed:

```text
2
1 2
5 1        <- vertex 5 is gone
```

which does not re-parse. Nothing caught it because the index rule was never
consulted, `renumber_indices` never ran, and the validate rule never fired. All
three live in `project_fixpoint`, which the tree path skipped.

Two things the fix settled:

**Section 6's claim holds.** A specialised generator can coexist with shared
validity. Leaf pruning stays exactly as it was -- only leaf subsets, iterated as
pruning exposes new leaves -- and everything after it is the same path every
other selection takes. The generator produces masks; it no longer produces
results.

**A tree's edge count is derived, not declared, and projection has to know
that.** The shared validate rule rejects a kept edge whose endpoint is gone,
which is right for a graph whose edge count is a real axis: the loss has nowhere
to go. A tree's edge count is implied by its vertex count, so the loss is
absorbed by definition, and the surviving edges are simply whatever the vertex
selection leaves. Getting this wrong makes trees stop pruning entirely rather
than prune wrongly -- a benchcase catches it.

Sharing a vertex count is legal as a result, so the last special case in
validation is gone and `Uses` collapsed back to a plain set of derived names.

### Rejected

- **A `SelectionConstraint` enum.** Section 6 describes constraints composing as
  an intersection, and `Connected` would be its first variant. It has exactly
  one inhabitant, so it would be an enum with one arm justified by nothing.
  Connectivity is instead enforced where the tree is materialised, next to the
  reference check, which is where the other validity rule already lives.
- **Making the tree generator general.** Producing arbitrary connected subsets
  rather than leaf prunings would widen what reduction can reach, but it is a
  search-quality change, not a correctness one, and this step was neither.
- **Leaving the bypass and special-casing `index` beside a tree.** That would
  have fixed the symptom and kept the last reducer off the shared path, which is
  the arrangement that hid the bug in the first place.

One honest note on coverage. The connectivity check has a single detector, a
test that hands it a disconnected mask directly. Leaf pruning cannot generate
one, so nothing else reaches it -- the same status as the dangling-reference
guard, and worth stating rather than implying the corpus covers it.


---

## 20. What the first bidirectional relation exposed

Step 7 predicted that a third producer would cost "one rule function and one
line in `induced_selections`". The question this step was built to answer is
whether bidirectionality breaks that.

**It held, and for a reason worth stating precisely.** A permutation is
`Vector<domain, Index(codomain)>` plus one thing. Everything `Index` already
does applies unchanged: the preimage direction (the codomain narrows, so
holders of dead references drop) and the label renumbering. Generalising them
took one helper,

```rust
fn reference_parts(decl: &Decl) -> Option<(&Ref, &Ref)>
```

which answers "how many entries, and which axis do the values name" for both
`Index` and `Permutation`. The genuinely new half is the image direction --
narrowing the domain narrows the codomain -- and that is exactly one rule
function plus one line at the seam.

**But the abstraction is narrower than it looked.** The shared seam covers the
half a bijection has in common with a one-way reference. It did not make the
image direction cheaper; it made it *separable*. Calling the step-7 extraction
"the induce seam" oversells it — it is the *reference* seam, and a relation
that is not reference-shaped will not get the same discount. That is a real
limit, and the next producer should be chosen to test it rather than to confirm
this one.

### A guard that could not be made to fire

`renumber_indices` briefly re-checked that a renumbered permutation was still a
bijection. It was removed. Two separate things justify that, and they are worth
keeping separate, because one of them is evidence and the other is a reason.

**The empirical part.** The guard was deliberately broken under five faults --
image rule disabled, preimage rule disabled, renumbering skipped, propagation
halted before convergence, and the guard alone -- and in every case the set of
failing tests was byte-identical with and without it. That is a measurement. On
its own it says only "no test I wrote distinguishes these", which is exactly
the claim an absent test also supports. It is not proof of anything.

**The argument.** Write the stored mapping as a bijection `s` on `{0..n-1}`;
read-time validation guarantees it is one. Let `mD` and `mC` be the masks on the
domain and codomain at projection time.

1. The image rule, run on `mD`, emits `s(mD)` as an induced selection on `C`.
2. The preimage rule, run on `mC`, emits `s^-1(mC)` as an induced selection on
   `D`.
3. Narrowing is intersection-only and the worklist enqueues on any strict
   shrink, so at a fixed point no rule can shrink anything further. From (1)
   that gives `mC` subset of `s(mD)`; from (2), `mD` subset of `s^-1(mC)`.
4. Applying `s` to the second containment gives `s(mD)` subset of `mC`. With the
   first, `mC = s(mD)` exactly.
5. `s` is injective, so `|mD| = |mC|`. Renumbering maps `mD` and `mC`
   order-isomorphically onto `1..=k` for the same `k`, and the composite is a
   bijection on `1..=k`.

So at a fixed point the guard is *provably* redundant, and the proof rests on
convergence -- which is not free, and is what the "worklist halts before
convergence" injection now tests.

**What the argument does not cover.** Step 3 assumes a fixed point. Off it, the
proof does not apply, yet the guard still never fired. The reason is narrower
and worth stating as the weaker claim it is: whichever of `D` or `C` narrows
first, the corresponding rule is emitted while processing that same occurrence,
so either both masks are present and matched, or the partner mask is absent
entirely and `masks.get` skips renumbering. That is an argument about the
current two rules, not a theorem about the engine, and a third rule touching
either axis would invalidate it. If one arrives, this is the paragraph to
re-read.

An assertion that no fault can trip is not a safety net; it is a claim about the
code that the code cannot check. The claim now lives in a comment at the site,
next to a pointer here.

### The gap this step actually found

Halting the worklist after a single round broke *nothing*. Every cascade in the
corpus -- graph, index, tree, and both permutation directions -- reaches its
fixed point in one round, because each rule maps a set straight to its exact
image or preimage. The iteration that §5 is named for was never exercised.

Reaching a second round needs producers in series, which took a two-hop chain:

```text
int N in 1..10
array A[N] in 0..999
int K in 0..10
index I[K] into N        # round 1: N narrows, so I's own axis narrows
int L in 0..10
index J[L] into K        # round 2: only now can J's references dangle
```

`a_two_hop_index_chain_needs_a_second_round` pins it, and halting after one
round now fails exactly that test. This is the strongest argument so far for
the worklist over a fixed two-pass scheme, and it is embarrassing that it
arrived at step 9 rather than step 4 — step 4's lattice test checked the merge
rule directly and never drove `propagate` far enough to iterate.

### Rejected

- **`P.positions`.** An alias for the default axis; see §9 above.
- **A `Bijection` trait or a `direction` flag on `Index`.** Two forms share the
  reference half and differ by one rule. A flag would put a branch in shared
  code to save one function; a trait would abstract over two implementations
  where one is a strict superset of the other.
- **Making the codomain a filtering mask.** The codomain mask renumbers rather
  than filters, because the mapping is stored once, on the domain. Treating it
  as a second filterable member would delete values from a list that does not
  exist.
- **Keeping the bijection guard as documentation.** A comment says the same
  thing without implying it is checked.

### Coverage, stated honestly

Detectors per injected fault, on the shipped code:

| fault | detectors | benchcases move |
| --- | --- | --- |
| image induction disabled | 5 | yes |
| preimage induction skipped for permutations | 2 | yes |
| final renumbering skipped for permutations | 5 | yes |
| image reads the projected, not original, mapping | 3 | yes |
| image rule drops the occurrence prefix | 1 | no |
| worklist halts before convergence | 1 | no |
| bijection re-check removed | 0 | no |

The last three are the informative rows. Occurrence-prefix handling and
convergence each have a single detector because each needs a schema shape no
other test has -- a permutation inside a `repeat`, and two producers in series.
The zero is why that guard is gone.


---

## 21. Numeric bounds are not identity

`array A[N] in 1..N` was section 9's own example and was not expressible: bounds
took integer literals. Building it is the first dependency in this design that
is **not** a relation, and its value is mostly in what it declines to reuse.

### What the dependency is

`in 1..N` constrains a *magnitude* by the current value of `N`. It is not a
reference into `N`. The distinction is not stylistic:

- A reference names an element. When `N` narrows, the element it names either
  survives -- and the label is rewritten to the survivor's new position -- or it
  does not, and the candidate is rejected. Identity is preserved; the number
  changes.
- A magnitude names a quantity. When `N` narrows, nothing about the quantity
  changes. The number stays; what changes is whether it is still admissible.

`index I[K] into N` and `int X in 1..N` both mention `N` and share nothing else.
The acceptance suite pins that directly: the same projection renumbers `I` and
leaves `X` alone.

### It does not participate in propagation

A numeric bound induces no mask, and it is not routed through
`induced_selections()`. The reason is that it carries no positional information
whatsoever: `in 1..N` says nothing about *which* positions of anything survive,
only whether an already-chosen candidate is legal. There is nothing for a
producer to emit.

The mechanism is one predicate, `dynamic_bounds_hold`, applied where candidates
are accepted in both passes. That placement -- rather than inside
`project_fixpoint` -- is deliberate: it covers structural reduction, value
reduction, tree pruning and any future pass with one rule, and it is the whole
of the "separate dependency layer" this feature needed. A schema with no dynamic
bound skips the check entirely, so every previously written schema follows the
path it always did, which is why all 28 earlier snapshots are byte-identical.

### What happens when `N` shrinks below an existing value

**The candidate is rejected.** Deleting elements can pull `N` under a magnitude
that `in 1..N` still has to admit; that candidate is not reachable *yet*.

Clamping during projection was the tempting alternative and is wrong. Projection
already rewrites numbers -- it renumbers references -- so rewriting a magnitude
looks like more of the same. It is not. Renumbering preserves identity: the same
element, a new label, and the rendered input still denotes what it denoted.
Clamping 7 to 3 preserves nothing. It is a value edit performed by a structural
pass, on a value the oracle never approved changing, and it would silently make
structural reduction lossy in a way no test of structure would catch.

The third option -- some new dependency mechanism that propagates a value floor
-- had no producer demanding it and, as below, no problem left to solve.

### The pass-order consequence

Rejection is only correct if the deletion is offered again later. It is, and
this was checked before the feature was built rather than assumed:

```rust
for _ in 0..16 {
    let before = best.clone();
    best = self.structural(&best);
    best = self.values(&best);
    if best == before { break; }
}
```

The schedule already alternates and already retries to a fixed point, so a
structurally infeasible deletion becomes feasible once the value pass has pulled
the offending magnitudes down, and the next round takes it.
`structural_shrinking_is_retried_after_the_value_pass` pins this with *static*
bounds only -- it was written before dynamic bounds existed, precisely so that
the schedule and the feature relying on it are not tested by the same code.

**No pass-order change was needed.** That is the honest answer, and it is the
first time in this design that an existing mechanism turned out to be enough.

One wart the injections exposed: `Shrinker::run` and the `reduce` test helper
contain the same sixteen-round loop, written twice. Breaking the real one is
caught by benchcases and no unit test; breaking the helper is caught by three
unit tests and no benchcase. They agree today. Nothing makes them agree
tomorrow.

### Rejected

- **A general expression language.** `N-1`, `min(N, 100)`, `2*N`. Every
  acceptance case for this step is satisfied by a bare name, and an expression
  grammar would bring parsing, precedence, an AST, and a resolver with no
  concrete case demanding any of it. A bare name is also the only form for which
  "the bound is the current value of that count" needs no further explanation.
- **Named axes, to give `A.values` meaning here.** A count-bounded array has no
  second axis, because its values are not positions in anything. Reaching for
  one would be the exact confusion this section exists to prevent.
- **Bounds naming a count from an enclosing block.** Rejected for symmetry with
  the existing rule that a count must be declared in the block it sizes. Same
  scope rule, same error, one thing to learn.
- **Resolving names at check time rather than parse time.** Storing the slot
  makes resolution occurrence-local by construction -- the value is read out of
  the same block instantiation -- so one `repeat` iteration cannot see another's
  count. Storing the name would have made that a property of the lookup code
  instead, which is a thing that can be got wrong; the injection that breaks
  block resolution is caught by two tests, and it would have needed more.
- **Clamping, again.** Worth listing twice. It is the design that makes every
  test pass and quietly destroys the input.

### Coverage

| fault | detectors | benchcases move |
| --- | --- | --- |
| dynamic bound never resolves | 8 | yes |
| block resolution ignores the repeat instance | 2 | yes |
| candidate validation disabled | 5 | yes |
| magnitudes renumbered as if references | 4 | yes |
| retry loop cut to one round (real reducer) | 1 | yes |
| retry loop cut to one round (test helper) | 3 | no |

The two single-detector rows are the informative ones, and they are informative
in opposite directions: the real reducer's schedule is covered *only* by
benchcases, and the helper's *only* by unit tests. Neither reaches the other.


---

## 22. The round cap was a semantic bound

Section 21 established that retrying structural reduction after value reduction
is correctness-relevant. That makes the shape of the retry loop part of the
reducer's semantics, and it was worth two separate questions.

### Why the loops were duplicated

`Shrinker::run` scheduled `Model`s; the `reduce` helper in `schema.rs` scheduled
`SchemaData` so that schema tests could avoid building a `Judge` and running a
program. The bodies were identical -- same cap, same order, same `==` check --
and the production one additionally propagated oracle errors and dispatched over
all six `Model` variants.

The duplication bought nothing. A copy is not an independent implementation, so
it never cross-checked anything; it only meant a fault in either copy was
invisible to the other's tests. Breaking the production loop was caught by
benchcases and no unit test. Breaking the helper was caught by unit tests and no
benchcase.

### Why sixteen, and why that was wrong

Sixteen was a safety bound written when structural and value passes were assumed
to unlock one another once or twice. It silently became a semantic bound:
reduction stopped there and reported the result as final.

Termination never depended on it. Every accepted edit strictly decreases the
pair

```text
(number of data elements, total distance from each value to its target)
```

under lexicographic order: deletion lowers the first and cannot raise the second
-- it drops non-negative terms from a sum -- and a value step lowers the second
while leaving the first alone. Both components are non-negative integers, so
"a round changed nothing" is always reached. The cap was never the terminator; it
was a backstop that happened to fire first on long inputs.

### A legal case that needs more than sixteen

Expressible with v0.5 features as they already stand, no new feature invented
for it:

```text
int N in 1..40
array A[N] in 1..N
```

with `N = 20`, every value `20`, and an oracle that accepts while the largest
value is within one of `N`. Deleting an element lowers `N`, which the dynamic
bound refuses while a larger value survives; and the largest value cannot fall
more than one step ahead of `N` without losing the failure. So each outer round
makes exactly one unit of progress, and reaching the fixed point takes about
twenty.

Under the old cap this returned `N = 5` with four values still present, as a
finished answer. It is now `long_alternating_chain` in the corpus, driven
through the production reducer rather than a helper.

**The cap was an accidental semantic bound and the truncation was a correctness
bug.** Not a performance policy: nothing about it was tuned, and no benchcase
oracle count changed when it was lifted, because no existing case reached it.

### What replaced it

One primitive, `reduce::to_fixed_point`, used by both production and the test
helper. It terminates when a whole round changes nothing. It still takes a
budget, because a pass that violates the decreasing-measure contract would
otherwise hang a CLI, but the budget is now

- scaled by input size, since that is what bounds productive rounds, and
- *reported*: exhaustion returns `Fixpoint::Exhausted`, the CLI prints a warning
  saying the result may not be fully reduced and that this is a ccmin bug, and
  the type makes callers acknowledge which case they got.

Sharing does not weaken testing here, which was the thing to check before
deduplicating. The scheduler is tested directly, against synthetic passes, with
no schema or model involved -- convergence, long chains, exhaustion, a zero
budget, error propagation. Those tests would be impossible to write against
either old copy without dragging in a whole model. What is lost is nothing,
because a copy was never an independent check.

### The other sixteen

`shrink_ints_toward` has its own `rounds < 16` sweep cap. It is the same magic
number and, in isolation, the same defect. It is now benign, and for a reason
rather than by luck: the outer loop only stops when a whole round changes
nothing, so a value pass that gives up early leaves work that the next round
picks up. The inner cap can only change how work is distributed across rounds,
not the final result. The twenty-round case above passes with that cap still in
place, which is the evidence. It is left alone under the freeze, and noted here
so it is not rediscovered as a mystery.

### Rejected

- **No budget at all.** The measure argument says the loop terminates, but it
  assumes both passes only ever shrink. That is a property of today's passes,
  not of the signature, and a hung CLI is a bad way to learn otherwise.
- **A flat larger cap.** It moves the cliff instead of removing it, and leaves
  the same silent truncation for whoever eventually walks off the new edge.
- **Returning an error on exhaustion.** A partly reduced input is still useful.
  Refusing to emit it would trade a quiet wrong answer for a loud lost one.
- **Keeping the helper as an independent schedule.** Attractive in principle --
  two implementations that must agree. In practice it was a copy, and copies
  drift silently rather than disagreeing loudly.

### Coverage

| fault | detectors | benchcases move |
| --- | --- | --- |
| shared scheduler runs one round | 11 | yes |
| convergence detection disabled | 6 | yes |
| test call site given a budget of one | 4 | no |
| exhaustion reported as success | 2 | no |
| production call site given a budget of one | 1 | yes |
| structural pass not retried after values | 1 | yes |

The two single-detector rows are both production *call site* faults, caught by
the benchcase corpus and by no unit test. That asymmetry is deliberate and is
what benchcases are for; it is not evidence that a unit test is missing. The
important change is the top row: a fault in the scheduler itself is now caught
from both directions at once, which is exactly what the duplicated loops could
not do.
