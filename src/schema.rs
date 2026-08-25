//! A user-declared input grammar, with constraints.
//!
//! Auto-detection has to *guess* what an input means, which is why v0.3 made it
//! deliberately conservative: a wrong guess corrupts the format and produces a
//! counterexample that never existed. A schema does not guess. The user states
//! the grammar, so widening what ccmin can reduce does not widen what it can
//! get wrong -- the two concerns that look opposed for auto-detection are
//! independent here.
//!
//! The schema is also the only sensible place to put constraints, which is why
//! they are part of it from the start rather than bolted on later:
//!
//! ```text
//! int T in 1..100
//! repeat T {
//!   int N in 1..1000
//!   array A[N] in 1..1000000000
//! }
//! ```
//!
//! Without `in 1..1000000000`, a reducer will happily drive a value to `0` and
//! hand back a "counterexample" the judge could never produce. That is the last
//! remaining way ccmin can be confidently wrong, and a declared bound closes it:
//! values shrink toward the legal value nearest zero, never past it, and an
//! array whose length field cannot be zero is never emptied.
//!
//! Length fields are *derived*, never shrunk directly. After any structural
//! edit the declared counts are recomputed from the data, so a desynchronised
//! `N` is unrepresentable for schema inputs exactly as it is for the built-in
//! shapes.

use crate::model::{Edge, GraphCase};
use crate::reduce::{ddmin_floor, ddmin_min_len, shrink_ints_toward, shrink_value_toward};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::rc::Rc;

// ---- grammar ------------------------------------------------------------

/// One side of a range.
///
/// A named side is a *numeric* dependency: `in 1..N` constrains a magnitude by
/// the current value of `N`. It is emphatically not a reference into `N`, and
/// nothing here induces a positional mask -- see `Limits` and section 21 of the
/// design note.
///
/// The name is stored as the slot of its `int` within the same block, resolved
/// once at parse time. That keeps `Bounds` `Copy`, and it makes resolution
/// occurrence-local for free: the value is read out of the same block
/// instantiation, so one `repeat` iteration cannot see another's `N`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bound {
    Lit(i64),
    Slot(usize),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bounds {
    pub lo: Option<Bound>,
    pub hi: Option<Bound>,
}

/// A `Bounds` with both sides reduced to numbers, against one block's values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    pub lo: Option<i64>,
    pub hi: Option<i64>,
}

impl Bounds {
    /// Does either side name a count? Only these need re-checking when
    /// structure shrinks; a literal range cannot be invalidated by deletion.
    fn is_dynamic(&self) -> bool {
        matches!(self.lo, Some(Bound::Slot(_))) || matches!(self.hi, Some(Bound::Slot(_)))
    }

    /// Reduce both sides against the block this declaration lives in. A named
    /// side that does not resolve to an `int` leaves that side unbounded, which
    /// is the permissive answer; validation has already rejected the cases
    /// where that could happen.
    fn resolve(&self, block: &[Value]) -> Limits {
        let side = |b: Option<Bound>| match b {
            None => None,
            Some(Bound::Lit(v)) => Some(v),
            Some(Bound::Slot(slot)) => match block.get(slot) {
                Some(Value::Int(v)) => Some(*v),
                _ => None,
            },
        };
        Limits {
            lo: side(self.lo),
            hi: side(self.hi),
        }
    }
}

impl Limits {
    fn contains(&self, v: i64) -> bool {
        !self.lo.is_some_and(|lo| v < lo) && !self.hi.is_some_and(|hi| v > hi)
    }

    /// The legal value closest to zero: where value shrinking aims.
    fn target(&self) -> i64 {
        let mut t = 0i64;
        if let Some(lo) = self.lo {
            if t < lo {
                t = lo;
            }
        }
        if let Some(hi) = self.hi {
            if t > hi {
                t = hi;
            }
        }
        t
    }

    /// A lower bound on a count field is a floor on how far structure shrinks.
    fn min_count(&self) -> usize {
        match self.lo {
            Some(lo) if lo > 0 => lo as usize,
            _ => 0,
        }
    }

    fn describe(&self) -> String {
        match (self.lo, self.hi) {
            (Some(lo), Some(hi)) => format!("{lo}..{hi}"),
            (Some(lo), None) => format!("{lo}.."),
            (None, Some(hi)) => format!("..{hi}"),
            (None, None) => "unbounded".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ref {
    Lit(i64),
    Name(String),
    /// `P.values`: the codomain axis of the permutation named `P`.
    ///
    /// The domain needs no spelling of its own -- it *is* the count's default
    /// axis, so a bare `[N]` already names it, and `P.positions` would be an
    /// alias with identical meaning.
    Values(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decl {
    Int {
        name: String,
        bounds: Bounds,
    },
    Array {
        name: String,
        len: Ref,
        bounds: Bounds,
    },
    Matrix {
        name: String,
        rows: Ref,
        cols: Ref,
        bounds: Bounds,
    },
    /// `verts` vertices, `verts - 1` edges, connected and acyclic.
    Tree {
        name: String,
        verts: Ref,
    },
    Graph {
        name: String,
        edges: Ref,
        verts: Ref,
    },
    /// `len` values forming a bijection: each names a one-based position on
    /// this permutation's own codomain axis, and together they cover it
    /// exactly once.
    ///
    /// Structurally this is an `Index` into a second axis of the same count,
    /// which is where the preimage direction and the renumbering come from for
    /// free. What a permutation adds is the image direction and the bijection
    /// invariant.
    Permutation {
        name: String,
        len: Ref,
        /// Always `Ref::Values(name)`; stored so `sizing_roles` can hand out a
        /// reference to it.
        values: Ref,
    },
    /// `len` values, each naming a one-based position on the axis that sizes
    /// `target`. A reference, not a magnitude: `index` says so explicitly
    /// rather than being inferred from an `in 1..N` bound, because
    /// `int K in 1..N` ("choose K of them") means something else entirely.
    Index {
        name: String,
        len: Ref,
        target: Ref,
    },
    Repeat {
        count: Ref,
        body: Vec<Decl>,
    },
}

/// The numeric range a declaration constrains its values by. Structural
/// declarations carry none, which reads as unbounded.
fn decl_bounds(decl: &Decl) -> Bounds {
    match decl {
        Decl::Int { bounds, .. } | Decl::Array { bounds, .. } | Decl::Matrix { bounds, .. } => {
            *bounds
        }
        _ => Bounds::default(),
    }
}

impl Decl {
    fn name(&self) -> Option<&str> {
        match self {
            Decl::Int { name, .. }
            | Decl::Array { name, .. }
            | Decl::Matrix { name, .. }
            | Decl::Tree { name, .. }
            | Decl::Graph { name, .. }
            | Decl::Index { name, .. }
            | Decl::Permutation { name, .. } => Some(name),
            Decl::Repeat { .. } => None,
        }
    }
}

// ---- counts and axes -----------------------------------------------------
//
// Migration scaffolding for v0.5 (design/shared-dimensions.md). The eventual
// model separates three things v0.4 keeps as one: a Count owns cardinality, an
// Axis owns positional identity, and a Reference names a position on an axis.
//
// The arena is **static**: names, bounds, axis topology, and where each count's
// value sits inside an instantiation of its block. It holds no cardinality and
// no keep-mask, because a declaration inside a `repeat` body has one instance
// per iteration and each has its own. Putting either on an arena node collapses
// (declaration, instance) into (declaration) -- see revision 6 of the design
// note, and `a_count_inside_a_repeat_differs_per_iteration`.
//
// Sizing lookups now resolve through the arena. The `derived` set survives only
// so `value_pass` can ask whether a name is a count, and a test asserts the two
// descriptions still agree.

pub type CountId = usize;
pub type AxisId = usize;

/// An `int` that some declaration is sized by.
///
/// Static. A count does **not** hold a cardinality: a declaration inside a
/// `repeat` body is instantiated once per iteration and each instance has its
/// own value. What the arena can hold is where to *find* that value, which is
/// fixed by the schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Count {
    pub name: String,
    pub bounds: Bounds,
    /// Every count has exactly one axis for now. Several axes per count is
    /// what makes shared dimensions possible, and is not this step.
    pub axis: AxisId,
    /// Index of this count's `int` declaration within its own block.
    ///
    /// Values mirror declarations positionally, so in any instantiation of
    /// that block `values[slot]` is this count's authoritative cardinality.
    /// Validation requires a count to be declared in the block it sizes, so
    /// the index is always meaningful where it is used.
    pub slot: usize,
}

/// A set of positions with identity. Cardinality is *not* stored here; it
/// comes from the count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Axis {
    pub count: CountId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schema {
    pub items: Vec<Decl>,
    /// Names bound to a count or vertex total, so `value_pass` knows not to
    /// shrink them directly. Sizing no longer goes through this set --
    /// `counts` does that -- and a test asserts the two still describe the
    /// same names.
    derived: HashSet<String>,
    counts: Vec<Count>,
    axes: Vec<Axis>,
    count_by_name: HashMap<String, CountId>,
    /// The codomain axis of each permutation. Its count is the same as the
    /// domain's; only the identity differs.
    codomain_by_name: HashMap<String, AxisId>,
}

impl Schema {
    pub fn is_derived(&self, name: &str) -> bool {
        self.derived.contains(name)
    }

    pub fn count_id(&self, name: &str) -> Option<CountId> {
        self.count_by_name.get(name).copied()
    }

    pub fn count_ids(&self) -> std::ops::Range<CountId> {
        0..self.counts.len()
    }

    /// The single axis of a count. Plural axes per count arrive with shared
    /// dimensions; until then this is total.
    pub fn default_axis(&self, id: CountId) -> AxisId {
        self.counts[id].axis
    }

    /// Where this count's value sits within an instantiation of its block.
    pub fn count_slot(&self, id: CountId) -> usize {
        self.counts[id].slot
    }

    /// The axis that sizes this reference, or `None` when the count is a
    /// literal and the size is therefore fixed by the schema.
    pub fn sizing_axis(&self, r: &Ref) -> Option<AxisId> {
        match r {
            Ref::Lit(_) => None,
            // Names are unique across the schema and validation already
            // required a count to be declared in the block it sizes, so a
            // global lookup here cannot resolve to the wrong one.
            Ref::Name(n) => self.count_id(n).map(|id| self.default_axis(id)),
            Ref::Values(perm) => self.codomain_by_name.get(perm).copied(),
        }
    }

    /// The count a reference draws its cardinality from. A permutation's two
    /// axes share one count, so both projections answer the same.
    pub fn count_of(&self, r: &Ref) -> Option<CountId> {
        match r {
            Ref::Lit(_) => None,
            Ref::Name(n) => self.count_id(n),
            Ref::Values(perm) => self
                .codomain_by_name
                .get(perm)
                .map(|axis| self.axes[*axis].count),
        }
    }

    /// Cardinality bounds reach an axis through its count. An axis never owns
    /// them.
    pub fn axis_bounds(&self, axis: AxisId) -> Bounds {
        self.counts[self.axes[axis].count].bounds
    }
}

/// Read only by the migration tests, which check the arena's shape directly.
#[cfg(test)]
impl Schema {
    /// Bounds of whatever sizes this reference. The reducer resolves the axis
    /// once per occurrence instead, so this survives only for the equivalence
    /// test against the pre-arena block scan.
    pub fn sizing_bounds(&self, r: &Ref) -> Option<Bounds> {
        self.sizing_axis(r).map(|axis| self.axis_bounds(axis))
    }

    pub fn count(&self, id: CountId) -> &Count {
        &self.counts[id]
    }

    pub fn axis(&self, id: AxisId) -> &Axis {
        &self.axes[id]
    }
}

/// A permutation needs a second axis on its count: same cardinality, its own
/// identity. Allocated after the counts exist, in declaration order.
fn build_codomain_axes(
    items: &[Decl],
    count_by_name: &HashMap<String, CountId>,
    axes: &mut Vec<Axis>,
    codomain_by_name: &mut HashMap<String, AxisId>,
) -> Result<(), String> {
    for decl in items {
        match decl {
            Decl::Permutation { name, len, .. } => {
                let Ref::Name(n) = len else {
                    return Err(format!(
                        "`{name}` needs a named count, not a literal length: its codomain has \
                         to be an axis"
                    ));
                };
                let Some(count) = count_by_name.get(n).copied() else {
                    return Err(format!(
                        "`{name}` is a permutation of `{n}`, but nothing is sized by `{n}`"
                    ));
                };
                if codomain_by_name.values().any(|a| axes[*a].count == count) {
                    return Err(format!(
                        "`{n}` already carries a permutation; a count may carry at most one, \
                         because its default axis is that permutation's domain"
                    ));
                }
                codomain_by_name.insert(name.clone(), axes.len());
                axes.push(Axis { count });
            }
            Decl::Repeat { body, .. } => {
                build_codomain_axes(body, count_by_name, axes, codomain_by_name)?
            }
            _ => {}
        }
    }
    Ok(())
}

/// `X.values` must name a declared permutation.
fn check_projections(
    items: &[Decl],
    codomain_by_name: &HashMap<String, AxisId>,
) -> Result<(), String> {
    for decl in items {
        for (r, _) in sizing_roles(decl) {
            if let Ref::Values(owner) = r {
                if !codomain_by_name.contains_key(owner) {
                    return Err(format!(
                        "`{owner}.values` is used, but `{owner}` is not a permutation"
                    ));
                }
            }
        }
        if let Decl::Repeat { body, .. } = decl {
            check_projections(body, codomain_by_name)?;
        }
    }
    Ok(())
}

fn check_index_targets(
    items: &[Decl],
    count_by_name: &HashMap<String, CountId>,
) -> Result<(), String> {
    for decl in items {
        match decl {
            Decl::Index {
                name,
                target: Ref::Name(n),
                ..
            } if !count_by_name.contains_key(n) => {
                return Err(format!(
                    "`{name}` indexes into `{n}`, but nothing is sized by `{n}`, so there is \
                     no axis to reference"
                ));
            }
            Decl::Repeat { body, .. } => check_index_targets(body, count_by_name)?,
            _ => {}
        }
    }
    Ok(())
}

/// Allocate one count per derived `int`, and one axis per count, in
/// declaration order so the identifiers are stable.
fn build_arenas(
    items: &[Decl],
    derived: &HashSet<String>,
    counts: &mut Vec<Count>,
    axes: &mut Vec<Axis>,
    by_name: &mut HashMap<String, CountId>,
) {
    for (slot, decl) in items.iter().enumerate() {
        match decl {
            Decl::Int { name, bounds } if derived.contains(name) => {
                let count = counts.len();
                let axis = axes.len();
                counts.push(Count {
                    name: name.clone(),
                    bounds: *bounds,
                    axis,
                    slot,
                });
                axes.push(Axis { count });
                by_name.insert(name.clone(), count);
            }
            Decl::Repeat { body, .. } => build_arenas(body, derived, counts, axes, by_name),
            _ => {}
        }
    }
}

// ---- schema text parsing -------------------------------------------------

pub fn parse_schema(text: &str) -> Result<Rc<Schema>, String> {
    let mut lines: Vec<(usize, Vec<String>)> = Vec::new();
    for (no, raw) in text.lines().enumerate() {
        let stripped = raw.split('#').next().unwrap_or("");
        let tokens: Vec<String> = tokenise(stripped);
        if !tokens.is_empty() {
            lines.push((no + 1, tokens));
        }
    }

    let mut cursor = 0usize;
    let items = parse_block(&lines, &mut cursor, false)?;
    if cursor != lines.len() {
        return Err(format!("line {}: unexpected `}}`", lines[cursor].0));
    }
    if items.is_empty() {
        return Err("schema is empty".into());
    }

    let mut derived = HashSet::new();
    let mut all_names = HashSet::new();
    validate(&items, &mut derived, &mut all_names)?;

    let mut counts = Vec::new();
    let mut axes = Vec::new();
    let mut count_by_name = HashMap::new();
    build_arenas(&items, &derived, &mut counts, &mut axes, &mut count_by_name);
    let mut codomain_by_name = HashMap::new();
    build_codomain_axes(&items, &count_by_name, &mut axes, &mut codomain_by_name)?;
    check_projections(&items, &codomain_by_name)?;
    // An index can only reference an axis that exists, and an axis exists only
    // where something is sized by that count.
    check_index_targets(&items, &count_by_name)?;

    Ok(Rc::new(Schema {
        items,
        derived,
        counts,
        axes,
        count_by_name,
        codomain_by_name,
    }))
}

/// Splits on whitespace but also makes brackets and braces their own tokens, so
/// `A[N]` and `A [ N ]` are both accepted.
fn tokenise(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in line.chars() {
        match ch {
            '[' | ']' | '{' | '}' => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                out.push(ch.to_string());
            }
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn parse_block(
    lines: &[(usize, Vec<String>)],
    cursor: &mut usize,
    nested: bool,
) -> Result<Vec<Decl>, String> {
    let mut items = Vec::new();
    while *cursor < lines.len() {
        let (no, tokens) = &lines[*cursor];
        if tokens[0] == "}" {
            if !nested {
                return Ok(items);
            }
            if tokens.len() != 1 {
                return Err(format!("line {no}: `}}` must be alone on its line"));
            }
            *cursor += 1;
            return Ok(items);
        }
        *cursor += 1;
        let decl = parse_decl(*no, tokens, lines, cursor, &items)?;
        items.push(decl);
    }
    if nested {
        return Err("unterminated `repeat` block: missing `}`".into());
    }
    Ok(items)
}

fn parse_decl(
    no: usize,
    tokens: &[String],
    lines: &[(usize, Vec<String>)],
    cursor: &mut usize,
    // Everything already declared in this block, so a range may name an
    // earlier `int`. Sequential parsing is what rules out forward references.
    items: &[Decl],
) -> Result<Decl, String> {
    let at = |msg: String| format!("line {no}: {msg}");
    match tokens[0].as_str() {
        "int" => {
            if tokens.len() < 2 {
                return Err(at("`int` needs a name".into()));
            }
            let name = ident(no, &tokens[1])?;
            let bounds = parse_bounds(no, &tokens[2..], items)?;
            Ok(Decl::Int { name, bounds })
        }
        "array" => {
            let (name, dims, rest) = parse_name_dims(no, &tokens[1..])?;
            if dims.len() != 1 {
                return Err(at(format!(
                    "`array` takes exactly one length, got {}; use `matrix` for two",
                    dims.len()
                )));
            }
            let bounds = parse_bounds(no, rest, items)?;
            Ok(Decl::Array {
                name,
                len: dims[0].clone(),
                bounds,
            })
        }
        "matrix" => {
            let (name, dims, rest) = parse_name_dims(no, &tokens[1..])?;
            if dims.len() != 2 {
                return Err(at(format!(
                    "`matrix` takes two dimensions, got {}",
                    dims.len()
                )));
            }
            let bounds = parse_bounds(no, rest, items)?;
            Ok(Decl::Matrix {
                name,
                rows: dims[0].clone(),
                cols: dims[1].clone(),
                bounds,
            })
        }
        "tree" => {
            let (name, dims, rest) = parse_name_dims(no, &tokens[1..])?;
            if !dims.is_empty() {
                return Err(at(
                    "`tree` has no edge count of its own; it is always `vertices - 1`".into(),
                ));
            }
            let verts = parse_vertices(no, rest)?;
            Ok(Decl::Tree { name, verts })
        }
        "graph" => {
            let (name, dims, rest) = parse_name_dims(no, &tokens[1..])?;
            if dims.len() != 1 {
                return Err(at("`graph` needs an edge count, as in `graph E[M]`".into()));
            }
            let verts = parse_vertices(no, rest)?;
            Ok(Decl::Graph {
                name,
                edges: dims[0].clone(),
                verts,
            })
        }
        "permutation" => {
            let (name, dims, rest) = parse_name_dims(no, &tokens[1..])?;
            if dims.len() != 1 {
                return Err(at(
                    "`permutation` needs a length, as in `permutation P[N]`".into()
                ));
            }
            if !rest.is_empty() {
                return Err(at(format!("unexpected `{}`", rest.join(" "))));
            }
            let values = Ref::Values(name.clone());
            Ok(Decl::Permutation {
                name,
                len: dims[0].clone(),
                values,
            })
        }
        "index" => {
            let (name, dims, rest) = parse_name_dims(no, &tokens[1..])?;
            if dims.len() != 1 {
                return Err(at(
                    "`index` needs a length, as in `index I[K] into N`".into()
                ));
            }
            if rest.len() != 2 || rest[0] != "into" {
                return Err(at("expected `into <count>` (for example `into N`)".into()));
            }
            let target = value_ref(no, &rest[1])?;
            if matches!(target, Ref::Lit(_)) {
                return Err(at("`into` needs the name of a count".into()));
            }
            Ok(Decl::Index {
                name,
                len: dims[0].clone(),
                target,
            })
        }
        "repeat" => {
            if tokens.len() != 3 || tokens[2] != "{" {
                return Err(at("expected `repeat <count> {`".into()));
            }
            let count = value_ref(no, &tokens[1])?;
            let body = parse_block(lines, cursor, true)?;
            if body.is_empty() {
                return Err(at("`repeat` block is empty".into()));
            }
            Ok(Decl::Repeat { count, body })
        }
        other => Err(at(format!(
            "unknown declaration `{other}` (expected int, array, matrix, tree, graph, index, \
             permutation or repeat)"
        ))),
    }
}

/// Parses `NAME`, `NAME[A]` or `NAME[A][B]`, returning the remaining tokens.
fn parse_name_dims(no: usize, tokens: &[String]) -> Result<(String, Vec<Ref>, &[String]), String> {
    if tokens.is_empty() {
        return Err(format!("line {no}: expected a name"));
    }
    let name = ident(no, &tokens[0])?;
    let mut dims = Vec::new();
    let mut i = 1usize;
    while i < tokens.len() && tokens[i] == "[" {
        if i + 2 >= tokens.len() || tokens[i + 2] != "]" {
            return Err(format!("line {no}: unclosed `[` after `{name}`"));
        }
        dims.push(value_ref(no, &tokens[i + 1])?);
        i += 3;
    }
    Ok((name, dims, &tokens[i..]))
}

fn parse_vertices(no: usize, tokens: &[String]) -> Result<Ref, String> {
    if tokens.len() != 2 || tokens[0] != "vertices" {
        return Err(format!(
            "line {no}: expected `vertices <count>` (for example `vertices N`)"
        ));
    }
    value_ref(no, &tokens[1])
}

fn parse_bounds(no: usize, tokens: &[String], items: &[Decl]) -> Result<Bounds, String> {
    if tokens.is_empty() {
        return Ok(Bounds::default());
    }
    if tokens[0] != "in" || tokens.len() < 2 {
        return Err(format!(
            "line {no}: expected `in <lo>..<hi>` (either side may be omitted), got `{}`",
            tokens.join(" ")
        ));
    }
    // Whitespace inside the range is insignificant, so `1..5` and `1 .. 5`
    // both work.
    let spec = &tokens[1..].concat();
    let Some(dot) = spec.find("..") else {
        return Err(format!(
            "line {no}: range `{spec}` is missing `..`, as in `1..1000000000`"
        ));
    };
    let (lo_text, hi_text) = (&spec[..dot], &spec[dot + 2..]);
    let lo = parse_bound_side(no, lo_text, items)?;
    let hi = parse_bound_side(no, hi_text, items)?;
    // Only a literal range can be judged empty here. A named side is whatever
    // the data says at the time, so emptiness is a runtime condition.
    if let (Some(Bound::Lit(lo)), Some(Bound::Lit(hi))) = (lo, hi) {
        if lo > hi {
            return Err(format!("line {no}: range `{spec}` is empty ({lo} > {hi})"));
        }
    }
    Ok(Bounds { lo, hi })
}

/// A range side is a literal or the name of an `int` declared *earlier in the
/// same block*. The "earlier" and "same block" parts are not restrictions
/// invented here: they are what makes the value readable when the bound is
/// checked, both while reading input and while reducing.
fn parse_bound_side(no: usize, text: &str, items: &[Decl]) -> Result<Option<Bound>, String> {
    if text.is_empty() {
        return Ok(None);
    }
    if let Ok(v) = text.parse::<i64>() {
        return Ok(Some(Bound::Lit(v)));
    }
    if ident(no, text).is_err() {
        return Err(format!(
            "line {no}: `{text}` is neither an integer nor a name"
        ));
    }
    match items
        .iter()
        .position(|d| matches!(d, Decl::Int { name, .. } if name == text))
    {
        Some(slot) => Ok(Some(Bound::Slot(slot))),
        None if items.iter().any(|d| d.name() == Some(text)) => Err(format!(
            "line {no}: `{text}` is not an `int`, so it cannot bound a range"
        )),
        None => Err(format!(
            "line {no}: no `int` named `{text}` is declared before this line in \
             the same block; a range may only name an earlier `int`"
        )),
    }
}

fn ident(no: usize, token: &str) -> Result<String, String> {
    let ok = !token.is_empty()
        && token
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if ok {
        Ok(token.to_string())
    } else {
        Err(format!("line {no}: `{token}` is not a valid name"))
    }
}

fn value_ref(no: usize, token: &str) -> Result<Ref, String> {
    if let Some((owner, field)) = token.split_once('.') {
        let owner = ident(no, owner)?;
        return match field {
            "values" => Ok(Ref::Values(owner)),
            other => Err(format!(
                "line {no}: unknown projection `.{other}`; a permutation exposes `.values`, \
                 and its domain is the count itself"
            )),
        };
    }
    if let Ok(v) = token.parse::<i64>() {
        if v < 0 {
            return Err(format!("line {no}: count `{v}` cannot be negative"));
        }
        return Ok(Ref::Lit(v));
    }
    ident(no, token).map(Ref::Name)
}

// ---- validation ----------------------------------------------------------

fn validate(
    items: &[Decl],
    derived: &mut HashSet<String>,
    all_names: &mut HashSet<String>,
) -> Result<(), String> {
    // Ints usable as a count in this block. Restricting count references to the
    // enclosing block keeps every resync local, which is what makes "the
    // declared length always matches the data" checkable.
    let mut ints_here: HashMap<String, Bounds> = HashMap::new();

    for decl in items {
        if let Some(name) = decl.name() {
            if !all_names.insert(name.to_string()) {
                return Err(format!("`{name}` is declared more than once"));
            }
        }

        // Sharing a count is simply legal: one axis, one selection, every
        // member projected together. Every form that induces on another axis
        // now does so through the shared pipeline, so nothing is left to
        // special-case here.
        let mut use_count = |r: &Ref, role: &str, owner: &str| -> Result<(), String> {
            let Ref::Name(n) = r else { return Ok(()) };
            if !ints_here.contains_key(n) {
                return Err(format!(
                    "`{owner}` uses `{n}` as its {role}, but `{n}` is not an `int` declared \
                     earlier in the same block"
                ));
            }
            derived.insert(n.clone());
            Ok(())
        };

        match decl {
            Decl::Int { name, bounds } => {
                ints_here.insert(name.clone(), *bounds);
            }
            Decl::Array { name, len, .. } => use_count(len, "length", name)?,
            Decl::Matrix {
                name, rows, cols, ..
            } => {
                use_count(rows, "row count", name)?;
                use_count(cols, "column count", name)?;
            }
            Decl::Tree { name, verts } => use_count(verts, "vertex count", name)?,
            Decl::Graph { name, edges, verts } => {
                use_count(edges, "edge count", name)?;
                use_count(verts, "vertex count", name)?;
            }
            Decl::Index { name, len, .. } => use_count(len, "length", name)?,
            Decl::Permutation { name, len, .. } => use_count(len, "length", name)?,
            Decl::Repeat { count, body } => {
                use_count(count, "repeat count", "repeat")?;
                validate(body, derived, all_names)?;
            }
        }
    }
    Ok(())
}

/// The pre-arena lookup: scan the enclosing block for an `int` of that name.
///
/// Superseded by `Schema::sizing_bounds`, and kept only so a test can assert
/// the two agree. Deleted once the arena is the sole path.
#[cfg(test)]
fn count_bounds_by_scan(items: &[Decl], r: &Ref) -> Option<Bounds> {
    match r {
        Ref::Lit(_) => None,
        Ref::Name(n) => items.iter().find_map(|d| match d {
            Decl::Int { name, bounds } if name == n => Some(*bounds),
            _ => None,
        }),
        // The pre-arena scan predates projections and has no answer for one.
        // The equivalence test skips them for that reason.
        Ref::Values(_) => None,
    }
}

// ---- data ----------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Int(i64),
    Array(Vec<i64>),
    Matrix(Vec<Vec<i64>>),
    /// Used for both `tree` and `graph`.
    Graph(GraphCase),
    /// One entry per iteration.
    Repeat(Vec<Vec<Value>>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaData {
    pub schema: Rc<Schema>,
    pub values: Vec<Value>,
}

// ---- reading an input against the schema --------------------------------

struct Cursor<'a> {
    tokens: &'a [i64],
    at: usize,
}

impl Cursor<'_> {
    fn take(&mut self, what: &str) -> Result<i64, String> {
        let v = self.tokens.get(self.at).copied().ok_or_else(|| {
            format!(
                "input ended while reading {what}: expected more values after {} tokens",
                self.at
            )
        })?;
        self.at += 1;
        Ok(v)
    }
}

pub fn parse_input(schema: &Rc<Schema>, text: &str) -> Result<SchemaData, String> {
    let mut tokens = Vec::new();
    for (i, tok) in text.split_whitespace().enumerate() {
        tokens.push(tok.parse::<i64>().map_err(|_| {
            format!(
                "token {} (`{tok}`) is not an integer; schema inputs must be all integers",
                i + 1
            )
        })?);
    }

    let mut cursor = Cursor {
        tokens: &tokens,
        at: 0,
    };
    // A register, not storage. Reading is linear, so by the time a length is
    // needed the count for the current iteration has just been read into it.
    // The authoritative value is the `Value::Int` slot; this only carries it
    // forward a few tokens.
    let mut current = vec![0i64; schema.count_ids().len()];
    let values = read_block(schema, &schema.items, &mut cursor, &mut current)?;

    if cursor.at != tokens.len() {
        return Err(format!(
            "schema consumed {} of {} values; {} left over",
            cursor.at,
            tokens.len(),
            tokens.len() - cursor.at
        ));
    }
    Ok(SchemaData {
        schema: Rc::clone(schema),
        values,
    })
}

fn read_block(
    schema: &Schema,
    items: &[Decl],
    cursor: &mut Cursor,
    current: &mut [i64],
) -> Result<Vec<Value>, String> {
    let mut out = Vec::with_capacity(items.len());
    for decl in items {
        // `out` is this block's values so far. A range may only name an
        // earlier `int`, so everything a bound can refer to is already in it.
        let value = read_decl(schema, decl, cursor, current, &out)?;
        out.push(value);
    }
    Ok(out)
}

/// How long is the thing this reference sizes? A literal answers for itself; a
/// name answers through its count, located by identifier rather than by string.
fn resolve(schema: &Schema, r: &Ref, current: &[i64], owner: &str) -> Result<usize, String> {
    let v = match r {
        Ref::Lit(v) => *v,
        Ref::Name(n) => {
            let id = schema
                .count_id(n)
                .ok_or_else(|| format!("`{owner}`: `{n}` has no value yet"))?;
            current[id]
        }
        // A permutation's two axes share one count, so the codomain is as long
        // as the domain.
        Ref::Values(perm) => {
            let id = schema
                .count_of(r)
                .ok_or_else(|| format!("`{owner}`: `{perm}` is not a permutation"))?;
            current[id]
        }
    };
    usize::try_from(v).map_err(|_| format!("`{owner}`: count {v} is negative"))
}

fn read_decl(
    schema: &Schema,
    decl: &Decl,
    cursor: &mut Cursor,
    current: &mut [i64],
    block: &[Value],
) -> Result<Value, String> {
    match decl {
        Decl::Int { name, bounds } => {
            let v = cursor.take(name)?;
            check_bound(v, &bounds.resolve(block), name)?;
            if let Some(id) = schema.count_id(name) {
                current[id] = v;
            }
            Ok(Value::Int(v))
        }
        Decl::Array { name, len, bounds } => {
            let n = resolve(schema, len, current, name)?;
            let mut arr = Vec::with_capacity(n);
            for _ in 0..n {
                let v = cursor.take(name)?;
                check_bound(v, &bounds.resolve(block), name)?;
                arr.push(v);
            }
            Ok(Value::Array(arr))
        }
        Decl::Matrix {
            name,
            rows,
            cols,
            bounds,
        } => {
            let r = resolve(schema, rows, current, name)?;
            let c = resolve(schema, cols, current, name)?;
            let mut grid = Vec::with_capacity(r);
            for _ in 0..r {
                let mut row = Vec::with_capacity(c);
                for _ in 0..c {
                    let v = cursor.take(name)?;
                    check_bound(v, &bounds.resolve(block), name)?;
                    row.push(v);
                }
                grid.push(row);
            }
            Ok(Value::Matrix(grid))
        }
        Decl::Tree { name, verts } => {
            let n = resolve(schema, verts, current, name)?;
            if n == 0 {
                return Err(format!("`{name}`: a tree needs at least one vertex"));
            }
            let edges = read_edges(cursor, n - 1, n, name)?;
            let graph = GraphCase { n, edges };
            if !is_tree(&graph) {
                return Err(format!(
                    "`{name}`: the {} edges do not form a tree (must be connected and acyclic)",
                    graph.edges.len()
                ));
            }
            Ok(Value::Graph(graph))
        }
        Decl::Graph { name, edges, verts } => {
            let n = resolve(schema, verts, current, name)?;
            let m = resolve(schema, edges, current, name)?;
            let list = read_edges(cursor, m, n, name)?;
            Ok(Value::Graph(GraphCase { n, edges: list }))
        }
        Decl::Permutation { name, len, .. } => {
            let n = resolve(schema, len, current, name)?;
            let mut mapping = Vec::with_capacity(n);
            for _ in 0..n {
                mapping.push(cursor.take(name)?);
            }
            if !is_permutation(&mapping) {
                return Err(format!(
                    "`{name}`: the {n} values are not a permutation of 1..={n}"
                ));
            }
            Ok(Value::Array(mapping))
        }
        Decl::Index { name, len, target } => {
            let n = resolve(schema, len, current, name)?;
            let extent = resolve(schema, target, current, name)?;
            let mut refs = Vec::with_capacity(n);
            for _ in 0..n {
                let v = cursor.take(name)?;
                let ok = v >= 1 && usize::try_from(v).map(|v| v <= extent).unwrap_or(false);
                if !ok {
                    return Err(format!("`{name}`: reference {v} is outside 1..={extent}"));
                }
                refs.push(v);
            }
            Ok(Value::Array(refs))
        }
        Decl::Repeat { count, body } => {
            let k = resolve(schema, count, current, "repeat")?;
            let mut iters = Vec::with_capacity(k);
            for _ in 0..k {
                iters.push(read_block(schema, body, cursor, current)?);
            }
            Ok(Value::Repeat(iters))
        }
    }
}

fn read_edges(
    cursor: &mut Cursor,
    count: usize,
    n: usize,
    owner: &str,
) -> Result<Vec<Edge>, String> {
    let mut edges = Vec::with_capacity(count);
    for _ in 0..count {
        let u = cursor.take(owner)?;
        let v = cursor.take(owner)?;
        let (u, v) = (endpoint(u, n, owner)?, endpoint(v, n, owner)?);
        edges.push(Edge { u, v });
    }
    Ok(edges)
}

fn endpoint(value: i64, n: usize, owner: &str) -> Result<usize, String> {
    let v = usize::try_from(value).ok().filter(|v| *v >= 1 && *v <= n);
    v.ok_or_else(|| format!("`{owner}`: endpoint {value} is outside 1..={n}"))
}

fn check_bound(v: i64, bounds: &Limits, name: &str) -> Result<(), String> {
    if bounds.contains(v) {
        Ok(())
    } else {
        Err(format!(
            "`{name}`: value {v} is outside the declared range {}",
            bounds.describe()
        ))
    }
}

/// Exactly the values `1..=len`, each once.
fn is_permutation(values: &[i64]) -> bool {
    let mut seen = vec![false; values.len()];
    for v in values {
        let Ok(index) = usize::try_from(*v - 1) else {
            return false;
        };
        match seen.get_mut(index) {
            Some(slot) if !*slot => *slot = true,
            _ => return false,
        }
    }
    true
}

fn is_tree(graph: &GraphCase) -> bool {
    if graph.n == 0 || graph.edges.len() + 1 != graph.n {
        return false;
    }
    let mut parent: Vec<usize> = (0..=graph.n).collect();
    fn root(parent: &mut [usize], mut v: usize) -> usize {
        while parent[v] != v {
            parent[v] = parent[parent[v]];
            v = parent[v];
        }
        v
    }
    for e in &graph.edges {
        let (a, b) = (root(&mut parent, e.u), root(&mut parent, e.v));
        if a == b {
            return false;
        }
        parent[a] = b;
    }
    true
}

// ---- rendering -----------------------------------------------------------

impl SchemaData {
    pub fn render(&self) -> String {
        let mut out = String::new();
        render_block(&self.schema.items, &self.values, &mut out);
        out
    }

    pub fn size(&self) -> usize {
        count_block(&self.values)
    }

    pub fn avg_magnitude(&self) -> f64 {
        let mut sum = 0f64;
        let mut n = 0usize;
        magnitude_block(&self.values, &mut sum, &mut n);
        if n == 0 {
            0.0
        } else {
            sum / n as f64
        }
    }
}

/// Consecutive scalars share a line, which is how `N M` is written by hand.
/// Everything else gets its own line. C++ token-based reading makes the exact
/// layout irrelevant to the programs; it is chosen for the human reading the
/// reduced case.
fn render_block(items: &[Decl], values: &[Value], out: &mut String) {
    let mut pending: Vec<String> = Vec::new();
    let flush = |pending: &mut Vec<String>, out: &mut String| {
        if !pending.is_empty() {
            out.push_str(&pending.join(" "));
            out.push('\n');
            pending.clear();
        }
    };

    for (decl, value) in items.iter().zip(values) {
        match value {
            Value::Int(v) => pending.push(v.to_string()),
            Value::Array(arr) => {
                flush(&mut pending, out);
                out.push_str(&join(arr));
                out.push('\n');
            }
            Value::Matrix(grid) => {
                flush(&mut pending, out);
                for row in grid {
                    out.push_str(&join(row));
                    out.push('\n');
                }
            }
            Value::Graph(graph) => {
                flush(&mut pending, out);
                for e in &graph.edges {
                    out.push_str(&format!("{} {}\n", e.u, e.v));
                }
            }
            Value::Repeat(iters) => {
                flush(&mut pending, out);
                let Decl::Repeat { body, .. } = decl else {
                    continue;
                };
                for iter in iters {
                    render_block(body, iter, out);
                }
            }
        }
    }
    flush(&mut pending, out);
}

fn join(values: &[i64]) -> String {
    values
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

fn count_block(values: &[Value]) -> usize {
    values
        .iter()
        .map(|v| match v {
            Value::Int(_) => 1,
            Value::Array(a) => a.len(),
            Value::Matrix(g) => g.iter().map(|r| r.len()).sum(),
            Value::Graph(g) => g.edges.len(),
            Value::Repeat(iters) => iters.iter().map(|i| count_block(i)).sum(),
        })
        .sum()
}

fn magnitude_block(values: &[Value], sum: &mut f64, n: &mut usize) {
    let add = |v: i64, sum: &mut f64, n: &mut usize| {
        *sum += (v as f64).abs();
        *n += 1;
    };
    for value in values {
        match value {
            Value::Int(v) => add(*v, sum, n),
            Value::Array(a) => a.iter().for_each(|v| add(*v, sum, n)),
            Value::Matrix(g) => g.iter().flatten().for_each(|v| add(*v, sum, n)),
            Value::Graph(_) => {}
            Value::Repeat(iters) => iters.iter().for_each(|i| magnitude_block(i, sum, n)),
        }
    }
}

// ---- resync --------------------------------------------------------------

impl SchemaData {
    /// Recompute every derived count from the data it describes. Called after
    /// each structural edit, which is what makes a mismatched count field
    /// impossible to emit.
    pub fn resync(&mut self) {
        let schema = Rc::clone(&self.schema);
        resync_block(&schema, &schema.items, &mut self.values);
    }
}

fn resync_block(schema: &Schema, items: &[Decl], values: &mut [Value]) {
    // What each sizing reference is actually sizing, measured from the data.
    let mut sizes: Vec<(&Ref, usize)> = Vec::new();
    for (i, decl) in items.iter().enumerate() {
        match (decl, &values[i]) {
            (Decl::Array { len, .. }, Value::Array(arr)) => sizes.push((len, arr.len())),
            (Decl::Matrix { rows, cols, .. }, Value::Matrix(grid)) => {
                sizes.push((rows, grid.len()));
                sizes.push((cols, grid.first().map_or(0, |r| r.len())));
            }
            (Decl::Tree { verts, .. }, Value::Graph(g)) => sizes.push((verts, g.n)),
            (Decl::Graph { edges, verts, .. }, Value::Graph(g)) => {
                sizes.push((edges, g.edges.len()));
                sizes.push((verts, g.n));
            }
            (Decl::Index { len, .. }, Value::Array(refs)) => sizes.push((len, refs.len())),
            (Decl::Permutation { len, .. }, Value::Array(m)) => sizes.push((len, m.len())),
            (Decl::Repeat { count, .. }, Value::Repeat(iters)) => sizes.push((count, iters.len())),
            _ => {}
        }
    }

    // A shared count is written once per member. They must already agree,
    // because a shared dimension is projected by a single mask; disagreement
    // would be the silent last-write-wins that made v0.4 reject sharing.
    #[cfg(debug_assertions)]
    {
        let mut seen: HashMap<CountId, usize> = HashMap::new();
        for (r, size) in &sizes {
            if let Some(id) = schema.count_of(r) {
                if let Some(previous) = seen.insert(id, *size) {
                    debug_assert_eq!(
                        previous, *size,
                        "members sharing count {id} disagree: {previous} vs {size}"
                    );
                }
            }
        }
    }

    for (r, size) in sizes {
        let Some(id) = schema.count_of(r) else {
            continue;
        };
        // `values` is this block's own instantiation, so the count's static
        // slot index addresses this instance and no other. That is the whole
        // reason a count declared inside a repeat can differ per iteration.
        if let Some(target) = values.get_mut(schema.count_slot(id)) {
            if matches!(target, Value::Int(_)) {
                *target = Value::Int(size as i64);
            }
        }
    }

    // Recurse after the counts in this block are settled.
    for (i, decl) in items.iter().enumerate() {
        if let (Decl::Repeat { body, .. }, Value::Repeat(iters)) = (decl, &mut values[i]) {
            for iter in iters {
                resync_block(schema, body, iter);
            }
        }
    }
}

// ---- reduction -----------------------------------------------------------

/// A path names one value. Reading it: `path[0]` indexes the block's values;
/// if that value is a repeat and the path continues, `path[1]` selects the
/// iteration and the remainder continues inside it. Leaf paths have odd length.
type Path = Vec<usize>;

/// The values of one block instantiation. `prefix` is empty for the top
/// level, or `..., repeat_decl, iteration` for one pass of a `repeat` body --
/// which is exactly why a bound resolved through it cannot see a sibling
/// iteration's count.
fn block_values_at<'a>(values: &'a [Value], prefix: &[usize]) -> Option<&'a [Value]> {
    let Some((&iteration, head)) = prefix.split_last() else {
        return Some(values);
    };
    let Value::Repeat(iters) = value_at(values, head)? else {
        return None;
    };
    iters.get(iteration).map(Vec::as_slice)
}

fn value_at<'a>(values: &'a [Value], path: &[usize]) -> Option<&'a Value> {
    let value = values.get(*path.first()?)?;
    if path.len() == 1 {
        return Some(value);
    }
    let Value::Repeat(iters) = value else {
        return None;
    };
    value_at(iters.get(path[1])?, &path[2..])
}

fn value_at_mut<'a>(values: &'a mut [Value], path: &[usize]) -> Option<&'a mut Value> {
    let head = *path.first()?;
    let value = values.get_mut(head)?;
    if path.len() == 1 {
        return Some(value);
    }
    let Value::Repeat(iters) = value else {
        return None;
    };
    value_at_mut(iters.get_mut(path[1])?, &path[2..])
}

fn decl_at<'a>(items: &'a [Decl], path: &[usize]) -> Option<&'a Decl> {
    let decl = items.get(*path.first()?)?;
    if path.len() == 1 {
        return Some(decl);
    }
    let Decl::Repeat { body, .. } = decl else {
        return None;
    };
    decl_at(body, &path[2..])
}

fn sites(items: &[Decl], values: &[Value], prefix: &mut Path, out: &mut Vec<Path>) {
    for (i, (decl, value)) in items.iter().zip(values).enumerate() {
        prefix.push(i);
        out.push(prefix.clone());
        if let (Decl::Repeat { body, .. }, Value::Repeat(iters)) = (decl, value) {
            for (k, iter) in iters.iter().enumerate() {
                prefix.push(k);
                sites(body, iter, prefix, out);
                prefix.pop();
            }
        }
        prefix.pop();
    }
}

fn put(data: &mut SchemaData, path: &[usize], value: Value) {
    if let Some(slot) = value_at_mut(&mut data.values, path) {
        *slot = value;
    }
}

/// Does any range in this schema name a count?
///
/// Worth asking once per pass: when the answer is no -- every schema written
/// before this feature -- candidate checking is skipped entirely and reduction
/// follows exactly the path it always did.
fn any_dynamic_bounds(items: &[Decl]) -> bool {
    items.iter().any(|d| match d {
        Decl::Repeat { body, .. } => any_dynamic_bounds(body),
        other => decl_bounds(other).is_dynamic(),
    })
}

/// Is every dynamically bounded value still inside its range?
///
/// This is the entire mechanism, and it is deliberately not a cascade. A
/// numeric bound says nothing about *which* positions survive, so it induces no
/// mask and takes no part in propagation; it only decides whether an
/// already-chosen candidate is legal. Deleting array elements can pull `N`
/// below a magnitude that `in 1..N` still has to admit, and the correct answer
/// is that this candidate is not reachable *yet*: the value pass shrinks the
/// offending magnitudes first, and the next structural round -- the schedule
/// already alternates -- offers the same deletion again.
///
/// Clamping instead was rejected. Renumbering a reference during projection
/// preserves identity: the same element, a new label. Clamping a magnitude
/// from 7 to 3 preserves nothing, and would be a value edit smuggled into a
/// structural pass without the oracle ever approving it as one.
fn dynamic_bounds_hold(data: &SchemaData) -> bool {
    for path in data.all_sites() {
        let Some(decl) = decl_at(&data.schema.items, &path) else {
            continue;
        };
        let bounds = decl_bounds(decl);
        if !bounds.is_dynamic() {
            continue;
        }
        let Some(block) = block_values_at(&data.values, &path[..path.len() - 1]) else {
            return false;
        };
        let limits = bounds.resolve(block);
        let ok = match value_at(&data.values, &path) {
            Some(Value::Int(v)) => limits.contains(*v),
            Some(Value::Array(a)) => a.iter().all(|v| limits.contains(*v)),
            Some(Value::Matrix(m)) => m.iter().flatten().all(|v| limits.contains(*v)),
            _ => true,
        };
        if !ok {
            return false;
        }
    }
    true
}

impl SchemaData {
    fn all_sites(&self) -> Vec<Path> {
        let mut out = Vec::new();
        sites(&self.schema.items, &self.values, &mut Vec::new(), &mut out);
        out
    }

    fn all_occurrences(&self) -> Vec<Occurrence> {
        let mut out = Vec::new();
        occurrences(
            &self.schema,
            &self.schema.items,
            &self.values,
            &Vec::new(),
            &mut out,
        );
        out
    }

    /// Delete data: array elements, matrix rows and columns, edges, vertices,
    /// and whole repeat iterations. Declared counts are recomputed after each
    /// accepted edit, and no count is taken below its declared minimum.
    pub fn structural_pass(&self, accept: &mut dyn FnMut(&SchemaData) -> bool) -> SchemaData {
        let dynamic = any_dynamic_bounds(&self.schema.items);
        let accept: &mut dyn FnMut(&SchemaData) -> bool =
            &mut |c: &SchemaData| (!dynamic || dynamic_bounds_hold(c)) && accept(c);
        let mut data = self.clone();
        // Every accepted edit strictly shrinks the input, so this terminates;
        // the counter only bounds pathological schemas.
        for _ in 0..512 {
            let mut changed = false;
            for occ in data.all_occurrences() {
                if shrink_occurrence(&mut data, &occ, accept) {
                    changed = true;
                    // Removing an iteration invalidates deeper occurrences.
                    break;
                }
            }
            if !changed {
                break;
            }
        }
        data
    }

    /// Pull integers toward the legal value nearest zero. Derived counts are
    /// skipped: they follow the data, they do not lead it.
    pub fn value_pass(&self, accept: &mut dyn FnMut(&SchemaData) -> bool) -> SchemaData {
        let dynamic = any_dynamic_bounds(&self.schema.items);
        let accept: &mut dyn FnMut(&SchemaData) -> bool =
            &mut |c: &SchemaData| (!dynamic || dynamic_bounds_hold(c)) && accept(c);
        let mut data = self.clone();
        let schema = Rc::clone(&data.schema);
        for path in data.all_sites() {
            let Some(decl) = decl_at(&schema.items, &path) else {
                continue;
            };
            let Some(value) = value_at(&data.values, &path).cloned() else {
                continue;
            };
            // Where value shrinking aims, with any named side of the range
            // read from this site's own block instantiation.
            let limits = match block_values_at(&data.values, &path[..path.len() - 1]) {
                Some(block) => decl_bounds(decl).resolve(block),
                None => Limits::default(),
            };
            match (decl, value) {
                (Decl::Int { name, .. }, Value::Int(v)) => {
                    if schema.is_derived(name) {
                        continue;
                    }
                    let reduced = shrink_value_toward(v, limits.target(), |cand| {
                        let mut trial = data.clone();
                        put(&mut trial, &path, Value::Int(cand));
                        accept(&trial)
                    });
                    if reduced != v {
                        put(&mut data, &path, Value::Int(reduced));
                    }
                }
                (Decl::Array { .. }, Value::Array(arr)) => {
                    let reduced = shrink_ints_toward(&arr, limits.target(), |cand| {
                        let mut trial = data.clone();
                        put(&mut trial, &path, Value::Array(cand.to_vec()));
                        accept(&trial)
                    });
                    if reduced != arr {
                        put(&mut data, &path, Value::Array(reduced));
                    }
                }
                (Decl::Matrix { .. }, Value::Matrix(grid)) => {
                    let mut next = grid.clone();
                    for r in 0..next.len() {
                        let row = next[r].clone();
                        let reduced = shrink_ints_toward(&row, limits.target(), |cand| {
                            let mut trial_grid = next.clone();
                            trial_grid[r] = cand.to_vec();
                            let mut trial = data.clone();
                            put(&mut trial, &path, Value::Matrix(trial_grid));
                            accept(&trial)
                        });
                        next[r] = reduced;
                    }
                    if next != grid {
                        put(&mut data, &path, Value::Matrix(next));
                    }
                }
                // Endpoints and iteration counts are structure, not values.
                _ => {}
            }
        }
        data
    }
}

/// Which dimension of a declaration an axis indexes.
///
/// A matrix is indexed by two axes, a graph by two (its edge list and its
/// vertex set), everything else by one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Elements,
    Rows,
    Cols,
    Edges,
    GraphVertices,
    /// Distinguished from `GraphVertices` because a tree may only drop leaves.
    TreeVertices,
    Iterations,
    /// A permutation's codomain. It carries no stored data of its own -- the
    /// mapping lives on the domain -- so it contributes no positional edit;
    /// its mask drives renumbering instead.
    PermutationValues,
}

/// One declaration that an axis occurrence indexes, and how.
#[derive(Clone, Debug)]
struct Member {
    decl: usize,
    role: Role,
}

/// An **axis occurrence**: an axis together with the block instantiation it
/// lives in.
///
/// `prefix` names that instantiation and is an ordinary `Path` prefix, so this
/// introduces no new addressing -- a declaration inside a `repeat` body has one
/// occurrence per iteration because it has one prefix per iteration. Two
/// occurrences of the same `AxisId` are unrelated and select independently.
#[derive(Clone, Debug)]
struct Occurrence {
    prefix: Path,
    axis: AxisId,
    /// Every declaration in this instantiation sized by this axis. More than
    /// one means a shared dimension: they select together, by construction.
    members: Vec<Member>,
}

impl Occurrence {
    fn path_of(&self, member: &Member) -> Path {
        let mut path = self.prefix.clone();
        path.push(member.decl);
        path
    }
}

fn sizing_roles(decl: &Decl) -> Vec<(&Ref, Role)> {
    match decl {
        Decl::Int { .. } => vec![],
        Decl::Array { len, .. } => vec![(len, Role::Elements)],
        Decl::Matrix { rows, cols, .. } => vec![(rows, Role::Rows), (cols, Role::Cols)],
        Decl::Tree { verts, .. } => vec![(verts, Role::TreeVertices)],
        Decl::Graph { edges, verts, .. } => {
            vec![(edges, Role::Edges), (verts, Role::GraphVertices)]
        }
        // Only the length. `target` is a reference, not a size.
        Decl::Index { len, .. } => vec![(len, Role::Elements)],
        // The domain holds the mapping; the codomain is a real axis with the
        // same cardinality, and needs an occurrence even when nothing is
        // declared on it, because the bijection narrows it.
        Decl::Permutation { len, values, .. } => {
            vec![(len, Role::Elements), (values, Role::PermutationValues)]
        }
        Decl::Repeat { count, .. } => vec![(count, Role::Iterations)],
    }
}

/// The declarations of the block a `Path` prefix names.
fn block_at<'a>(items: &'a [Decl], prefix: &[usize]) -> Option<&'a [Decl]> {
    if prefix.is_empty() {
        return Some(items);
    }
    let Decl::Repeat { body, .. } = items.get(*prefix.first()?)? else {
        return None;
    };
    block_at(body, &prefix[2..])
}

/// How many positions this member currently has along `role`.
fn extent_of(value: &Value, role: Role) -> Option<usize> {
    Some(match (value, role) {
        (Value::Array(a), Role::Elements) => a.len(),
        (Value::Matrix(g), Role::Rows) => g.len(),
        (Value::Matrix(g), Role::Cols) => g.first().map_or(0, |r| r.len()),
        (Value::Graph(g), Role::Edges) => g.edges.len(),
        (Value::Graph(g), Role::GraphVertices | Role::TreeVertices) => g.n,
        (Value::Repeat(iters), Role::Iterations) => iters.len(),
        // A bijection has as many values as positions, and the mapping is
        // stored on the domain, so the domain's length is the codomain's too.
        (Value::Array(mapping), Role::PermutationValues) => mapping.len(),
        _ => return None,
    })
}

/// Every axis occurrence in this instantiation, in declaration order.
///
/// Order matters: the reducer visits occurrences in this order and restarts on
/// the first accepted change, so it is part of the search path the benchcases
/// pin.
fn occurrences(
    schema: &Schema,
    items: &[Decl],
    values: &[Value],
    prefix: &Path,
    out: &mut Vec<Occurrence>,
) {
    // Axes already seen in *this* block instantiation. Scoped per call, so a
    // nested block never merges with its parent.
    let mut here: Vec<(AxisId, usize)> = Vec::new();

    for (i, decl) in items.iter().enumerate() {
        for (r, role) in sizing_roles(decl) {
            let Some(axis) = schema.sizing_axis(r) else {
                continue;
            };
            let member = Member { decl: i, role };
            match here.iter().find(|(a, _)| *a == axis) {
                Some((_, at)) => out[*at].members.push(member),
                None => {
                    here.push((axis, out.len()));
                    out.push(Occurrence {
                        prefix: prefix.clone(),
                        axis,
                        members: vec![member],
                    });
                }
            }
        }
        if let (Decl::Repeat { body, .. }, Some(Value::Repeat(iters))) = (decl, values.get(i)) {
            for (k, iter) in iters.iter().enumerate() {
                let mut inner = prefix.clone();
                inner.push(i);
                inner.push(k);
                occurrences(schema, body, iter, &inner, out);
            }
        }
    }
}

/// The edge positions that survive a vertex selection: those whose endpoints
/// both remain.
///
/// Derived from the **original** edge list and the selected vertex positions,
/// never from an already-projected graph. That ordering is what makes this an
/// induced selection rather than a recomputation.
fn induced_edge_keep(graph: &GraphCase, vertex_keep: &[usize]) -> Vec<usize> {
    let mut alive = vec![false; graph.n + 1];
    for &position in vertex_keep {
        if position < graph.n {
            alive[position + 1] = true;
        }
    }
    graph
        .edges
        .iter()
        .enumerate()
        .filter(|(_, e)| alive[e.u] && alive[e.v])
        .map(|(i, _)| i)
        .collect()
}

/// Sorted intersection. The only way a mask ever changes.
fn intersect(a: &[usize], b: &[usize]) -> Vec<usize> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

/// An occurrence, keyed the way the solver addresses it: by the concrete block
/// instantiation and axis, never by a static `AxisId` alone.
type OccurrenceKey = (Path, AxisId);

/// The survivor set of every occurrence a candidate touches.
///
/// A `BTreeMap` rather than a `HashMap` so iteration -- and therefore the
/// projection it drives -- is deterministic.
struct Propagation {
    masks: BTreeMap<OccurrenceKey, Vec<usize>>,
    /// Successful narrowings. Every one strictly removes at least one position,
    /// so this is bounded by the total number of positions in play. Asserted in
    /// tests as a guard against a re-enqueue cycle.
    updates: usize,
}

impl Propagation {
    /// The merge rule, and the only one: intersect, and report whether that
    /// actually removed anything. Never widens.
    fn narrow(&mut self, key: OccurrenceKey, incoming: &[usize], domain: &[usize]) -> bool {
        let current = self.masks.entry(key).or_insert_with(|| domain.to_vec());
        let merged = intersect(current, incoming);
        if merged == *current {
            return false;
        }
        *current = merged;
        self.updates += 1;
        true
    }
}

/// Run the selection to a fixed point *before* projecting anything.
///
/// Projecting between events would let a later inducer read data whose
/// positional identity had already been destroyed, so every inducer derives
/// from the original data plus the current source mask.
/// A selection one relation induces on another axis of the same block.
struct Induced {
    axis: AxisId,
    keep: Vec<usize>,
}

/// Everything the relations induce when `occ` narrows to `source`.
///
/// The two rules have the same shape -- observe an occurrence's mask, name a
/// sibling axis, return positional survivors -- and the emit path after them
/// (find the target, take its domain, narrow, enqueue) was duplicated verbatim.
/// That shared shape is the whole abstraction; it is a function returning a
/// list, not a trait, because the rule set is closed and known at the only call
/// site, so dispatch would buy indirection and nothing else.
///
/// Order is graph rules then index rules, matching what the open-coded loops
/// did. The fixed point does not depend on it -- intersection commutes -- but
/// keeping it makes the search path reproducible.
fn induced_selections(
    schema: &Schema,
    data: &SchemaData,
    occ: &Occurrence,
    source: &[usize],
) -> Vec<Induced> {
    let mut out = Vec::new();
    induce_from_graph_vertices(schema, data, occ, source, &mut out);
    induce_from_index_targets(schema, data, &occ.prefix, occ.axis, source, &mut out);
    induce_from_permutation_image(schema, data, occ, source, &mut out);
    out
}

/// Keeping a set of vertices determines which edge positions can survive.
///
/// Derived from the ORIGINAL edge list, never from a projection.
fn induce_from_graph_vertices(
    schema: &Schema,
    data: &SchemaData,
    occ: &Occurrence,
    source: &[usize],
    out: &mut Vec<Induced>,
) {
    for member in &occ.members {
        if member.role != Role::GraphVertices {
            continue;
        }
        let path = occ.path_of(member);
        let (Some(Value::Graph(graph)), Some(Decl::Graph { edges, .. })) =
            (value_at(&data.values, &path), decl_at(&schema.items, &path))
        else {
            continue;
        };
        let Some(axis) = schema.sizing_axis(edges) else {
            continue;
        };
        out.push(Induced {
            axis,
            keep: induced_edge_keep(graph, source),
        });
    }
}

/// The two references a reference-holding declaration carries: how many
/// entries it has, and which axis its values name.
///
/// A permutation is an `Index` into a second axis of its own count, which is
/// why the preimage rule and the renumbering below work on it unchanged. The
/// only thing it adds is the image direction.
fn reference_parts(decl: &Decl) -> Option<(&Ref, &Ref)> {
    match decl {
        Decl::Index { len, target, .. } => Some((len, target)),
        Decl::Permutation { len, values, .. } => Some((len, values)),
        _ => None,
    }
}

/// An `index` into this axis loses every reference to a position that is gone.
///
/// Reads the ORIGINAL references: their values are positions in the target's
/// original domain, which a projection no longer carries.
fn induce_from_index_targets(
    schema: &Schema,
    data: &SchemaData,
    prefix: &Path,
    axis: AxisId,
    source: &[usize],
    out: &mut Vec<Induced>,
) {
    let Some(block) = block_at(&schema.items, prefix) else {
        return;
    };
    for (i, decl) in block.iter().enumerate() {
        let Some((len, target)) = reference_parts(decl) else {
            continue;
        };
        if schema.sizing_axis(target) != Some(axis) {
            continue;
        }
        let Some(index_axis) = schema.sizing_axis(len) else {
            continue;
        };
        let mut path = prefix.clone();
        path.push(i);
        let Some(Value::Array(refs)) = value_at(&data.values, &path) else {
            continue;
        };
        out.push(Induced {
            axis: index_axis,
            keep: refs
                .iter()
                .enumerate()
                .filter(|(_, v)| match usize::try_from(**v - 1) {
                    Ok(position) => source.binary_search(&position).is_ok(),
                    Err(_) => false,
                })
                .map(|(position, _)| position)
                .collect(),
        });
    }
}

/// A permutation's domain determines its image: keeping a set of positions
/// keeps exactly the values they map to.
///
/// This is the half `Index` does not provide. `Index` gives the preimage --
/// when the target narrows, holders of dead references drop -- and a bijection
/// additionally forces the reverse. Running to a fixed point makes the two meet
/// at `codomain == image(domain)` and `domain == preimage(codomain)`, which is
/// exactly the surviving permutation.
fn induce_from_permutation_image(
    schema: &Schema,
    data: &SchemaData,
    occ: &Occurrence,
    source: &[usize],
    out: &mut Vec<Induced>,
) {
    let Some(block) = block_at(&schema.items, &occ.prefix) else {
        return;
    };
    for (i, decl) in block.iter().enumerate() {
        let Decl::Permutation { len, values, .. } = decl else {
            continue;
        };
        // Fires when the *domain* narrows; the preimage rule handles the other
        // direction from the codomain.
        if schema.sizing_axis(len) != Some(occ.axis) {
            continue;
        }
        let Some(codomain) = schema.sizing_axis(values) else {
            continue;
        };
        let mut path = occ.prefix.clone();
        path.push(i);
        let Some(Value::Array(mapping)) = value_at(&data.values, &path) else {
            continue;
        };
        // The image, as zero-based codomain positions, read from the ORIGINAL
        // mapping and the current domain mask.
        let mut image: Vec<usize> = source
            .iter()
            .filter_map(|position| mapping.get(*position))
            .filter_map(|label| usize::try_from(*label - 1).ok())
            .collect();
        image.sort_unstable();
        out.push(Induced {
            axis: codomain,
            keep: image,
        });
    }
}

fn propagate(
    data: &SchemaData,
    seed: &Occurrence,
    seed_keep: &[usize],
    all: &[Occurrence],
) -> Propagation {
    let schema = Rc::clone(&data.schema);
    let mut state = Propagation {
        masks: BTreeMap::new(),
        updates: 0,
    };
    state
        .masks
        .insert((seed.prefix.clone(), seed.axis), seed_keep.to_vec());

    let mut queue: VecDeque<OccurrenceKey> = VecDeque::new();
    queue.push_back((seed.prefix.clone(), seed.axis));

    while let Some(key) = queue.pop_front() {
        let Some(occ) = all.iter().find(|o| o.prefix == key.0 && o.axis == key.1) else {
            continue;
        };
        let Some(source) = state.masks.get(&key).cloned() else {
            continue;
        };

        // One emit path for every relation form.
        for Induced { axis, keep } in induced_selections(&schema, data, occ, &source) {
            let target: OccurrenceKey = (key.0.clone(), axis);
            let Some(target_occ) = all.iter().find(|o| o.prefix == key.0 && o.axis == axis) else {
                continue;
            };
            let Some(extent) = occurrence_extent(data, target_occ) else {
                continue;
            };
            let domain: Vec<usize> = (0..extent).collect();
            if state.narrow(target.clone(), &keep, &domain) {
                queue.push_back(target);
            }
        }
    }

    state
}

/// Rewrite surviving references from original target positions to projected
/// ones.
///
/// Derived from the final keep-mask, never maintained during propagation. A
/// reference whose target did not survive means the induction missed it: the
/// candidate is rejected rather than clipped, renumbered onto a neighbour, or
/// left dangling.
fn renumber_indices(
    schema: &Schema,
    items: &[Decl],
    trial: &mut SchemaData,
    prefix: &Path,
    masks: &BTreeMap<OccurrenceKey, Vec<usize>>,
) -> Option<()> {
    for (i, decl) in items.iter().enumerate() {
        let mut path = prefix.clone();
        path.push(i);

        if let Decl::Repeat { body, .. } = decl {
            let iterations = match value_at(&trial.values, &path) {
                Some(Value::Repeat(iters)) => iters.len(),
                _ => 0,
            };
            for k in 0..iterations {
                let mut inner = path.clone();
                inner.push(k);
                renumber_indices(schema, body, trial, &inner, masks)?;
            }
            continue;
        }

        let Some((_, target)) = reference_parts(decl) else {
            continue;
        };
        let Some(axis) = schema.sizing_axis(target) else {
            continue;
        };
        let Some(mask) = masks.get(&(prefix.clone(), axis)) else {
            continue; // the target was untouched, so the labels still hold
        };
        let Some(Value::Array(refs)) = value_at(&trial.values, &path).cloned() else {
            continue;
        };
        let mut rewritten = Vec::with_capacity(refs.len());
        for reference in refs {
            let old = usize::try_from(reference - 1).ok()?;
            // Position within the survivors, one-based. `None` here is a
            // dangling reference: reject the candidate.
            let new = mask.binary_search(&old).ok()? + 1;
            rewritten.push(new as i64);
        }
        // No bijection re-check here on purpose. Read-time validation rejects
        // a non-permutation input, and for candidates the image and preimage
        // rules are exact inverses of one mapping, so every mask pair the
        // worklist can reach is already matched. A guard here was written,
        // then deliberately broken under five separate faults, and never once
        // changed a test outcome; see design/shared-dimensions.md section 20.
        put(trial, &path, Value::Array(rewritten));
    }
    Some(())
}

/// Keep `vertex_keep` vertices and `edge_keep` edge positions, relabelling the
/// survivors to `1..=k`. Either mask absent means "all of them".
///
/// One `Value::Graph` belongs to two occurrences, so it can receive two masks;
/// applying them together is what lets projection happen once.
fn project_graph(
    g: &GraphCase,
    vertex_keep: Option<&[usize]>,
    edge_keep: Option<&[usize]>,
) -> Option<GraphCase> {
    let kept: Vec<usize> = match vertex_keep {
        Some(v) => v.to_vec(),
        None => (0..g.n).collect(),
    };
    let mut remap = vec![0usize; g.n + 1];
    for (new, &position) in kept.iter().enumerate() {
        if position < g.n {
            remap[position + 1] = new + 1;
        }
    }

    let mut edges = Vec::new();
    for (i, e) in g.edges.iter().enumerate() {
        let selected = match edge_keep {
            Some(keep) => keep.binary_search(&i).is_ok(),
            None => true,
        };
        if !selected {
            continue;
        }
        let (u, v) = (remap[e.u], remap[e.v]);
        // The shared validate rule: a surviving reference must name a
        // surviving position. Reaching here means the induction kept an edge
        // whose endpoint is gone, so reject the candidate rather than drop the
        // edge silently -- dropping it would leave the edge count describing
        // data that is no longer there. Same discipline as `renumber_indices`.
        if u == 0 || v == 0 {
            return None;
        }
        edges.push(Edge { u, v });
    }

    Some(GraphCase {
        n: kept.len(),
        edges,
    })
}

/// Apply every mask that reached one value, in one step.
fn apply_masks(value: &Value, ops: &[(Role, Vec<usize>)]) -> Option<Value> {
    let find = |wanted: Role| {
        ops.iter()
            .find(|(role, _)| *role == wanted)
            .map(|(_, mask)| mask.as_slice())
    };
    Some(match value {
        Value::Graph(g) => {
            let vertices = find(Role::GraphVertices).or_else(|| find(Role::TreeVertices));
            // A tree's edge count is implied by its vertex count rather than
            // declared, so the surviving edges are whatever the vertex
            // selection leaves. A graph's edge count is a real axis, so its
            // mask has to come from the propagation instead.
            let edges: Option<Vec<usize>> = match find(Role::TreeVertices) {
                Some(kept) => Some(induced_edge_keep(g, kept)),
                None => find(Role::Edges).map(<[usize]>::to_vec),
            };
            let projected = project_graph(g, vertices, edges.as_deref())?;
            if find(Role::TreeVertices).is_some() && !is_tree(&projected) {
                // The leaf-pruning generator is supposed to guarantee this.
                // Checking it where the tree is materialised makes the
                // guarantee observable rather than assumed, and keeps validity
                // on the shared path even though generation is specialised.
                return None;
            }
            Value::Graph(projected)
        }
        Value::Matrix(grid) => {
            let mut out = grid.clone();
            if let Some(rows) = find(Role::Rows) {
                out = rows.iter().filter_map(|&i| out.get(i).cloned()).collect();
            }
            if let Some(cols) = find(Role::Cols) {
                out = select_columns(&out, cols);
            }
            Value::Matrix(out)
        }
        Value::Array(a) => match find(Role::Elements) {
            Some(mask) => Value::Array(mask.iter().filter_map(|&i| a.get(i).copied()).collect()),
            // A permutation's codomain mask drives renumbering rather than
            // filtering: the mapping is stored on the domain.
            None if find(Role::PermutationValues).is_some() => Value::Array(a.clone()),
            None => return None,
        },
        Value::Repeat(iters) => {
            let mask = find(Role::Iterations)?;
            Value::Repeat(mask.iter().filter_map(|&i| iters.get(i).cloned()).collect())
        }
        Value::Int(_) => return None,
    })
}

/// Project the whole fixed point onto the data, reading originals throughout.
fn project_fixpoint(
    data: &SchemaData,
    state: &Propagation,
    all: &[Occurrence],
) -> Option<SchemaData> {
    // Deterministic: BTreeMap over paths, fed from a BTreeMap over occurrences.
    let mut edits: BTreeMap<Path, Vec<(Role, Vec<usize>)>> = BTreeMap::new();
    for ((prefix, axis), mask) in &state.masks {
        let Some(occ) = all.iter().find(|o| &o.prefix == prefix && o.axis == *axis) else {
            continue;
        };
        for member in &occ.members {
            edits
                .entry(occ.path_of(member))
                .or_default()
                .push((member.role, mask.clone()));
        }
    }

    let mut trial = data.clone();
    for (path, ops) in edits {
        let original = value_at(&data.values, &path)?;
        let projected = apply_masks(original, &ops)?;
        put(&mut trial, &path, projected);
    }

    // Only now, with the final masks known.
    let schema = Rc::clone(&data.schema);
    renumber_indices(
        &schema,
        &schema.items,
        &mut trial,
        &Vec::new(),
        &state.masks,
    )?;
    // Downstream bookkeeping only: resync observes members that are already
    // consistent, it does not take part in deciding which positions survived.
    trial.resync();
    Some(trial)
}

/// Build the candidate in which this occurrence keeps only `keep`.
fn project_occurrence(
    data: &SchemaData,
    occ: &Occurrence,
    keep: &[usize],
    all: &[Occurrence],
) -> Option<SchemaData> {
    let state = propagate(data, occ, keep, all);
    project_fixpoint(data, &state, all)
}

fn shrink_occurrence(
    data: &mut SchemaData,
    occ: &Occurrence,
    accept: &mut dyn FnMut(&SchemaData) -> bool,
) -> bool {
    let schema = Rc::clone(&data.schema);
    let bounds = match block_values_at(&data.values, &occ.prefix) {
        Some(block) => schema.axis_bounds(occ.axis).resolve(block),
        None => Limits::default(),
    };
    // The solver addresses targets by (prefix, axis), so it needs the whole
    // set; induction still only ever reaches the same block instantiation.
    let all: Vec<Occurrence> = data.all_occurrences();

    // A tree may only shed leaves, so it runs a constrained sequence of
    // selections rather than one. Validation keeps it a sole member.
    if occ.members.iter().any(|m| m.role == Role::TreeVertices) {
        return prune_tree(data, occ, bounds.min_count().max(1), accept);
    }

    let Some(extent) = occurrence_extent(data, occ) else {
        return false;
    };
    let min = match occ.members.iter().any(|m| m.role == Role::GraphVertices) {
        true => bounds.min_count().max(1),
        false => bounds.min_count(),
    };
    let allow_empty = !occ.members.iter().any(|m| m.role == Role::GraphVertices);
    if extent <= min {
        return false;
    }

    let positions: Vec<usize> = (0..extent).collect();
    let mut test = |candidate: &[usize]| match project_occurrence(data, occ, candidate, &all) {
        Some(trial) => accept(&trial),
        None => false,
    };
    let kept = if allow_empty {
        ddmin_floor(&positions, min, &mut test)
    } else {
        ddmin_min_len(&positions, min, &mut test)
    };
    // ddmin only removes, so the survivor set can only shrink relative to the
    // occurrence's domain. That is what keeps the fixpoint argument valid.
    debug_assert!(kept.len() <= extent);
    if kept.len() == extent {
        return false;
    }
    match project_occurrence(data, occ, &kept, &all) {
        Some(next) => {
            *data = next;
            true
        }
        None => false,
    }
}

/// Every member of an occurrence must currently have the same extent -- that is
/// what sharing a count means. Disagreement is a bug, not an input error.
fn occurrence_extent(data: &SchemaData, occ: &Occurrence) -> Option<usize> {
    let mut agreed: Option<usize> = None;
    for member in &occ.members {
        let value = value_at(&data.values, &occ.path_of(member))?;
        let extent = extent_of(value, member.role)?;
        match agreed {
            None => agreed = Some(extent),
            Some(previous) => debug_assert_eq!(
                previous, extent,
                "members of one axis occurrence disagree on extent"
            ),
        }
    }
    agreed
}

/// Reduce a tree by pruning leaves.
///
/// The *generator* is specialised -- only leaf subsets keep a tree connected,
/// and pruning exposes new leaves, so this is a sequence of selections rather
/// than one. Everything after it is the shared path: each candidate goes
/// through `project_occurrence`, so co-sized members are projected, index
/// references into the vertex axis are dropped and renumbered, and the result
/// is validated like any other candidate.
///
/// Pruning used to write the tree directly and call `resync`, bypassing
/// propagation. Beside `index I[2] into N` that was a live bug: references
/// were neither dropped nor renumbered, and the reduced input could not be
/// re-parsed.
fn prune_tree(
    data: &mut SchemaData,
    occ: &Occurrence,
    min: usize,
    accept: &mut dyn FnMut(&SchemaData) -> bool,
) -> bool {
    let key = (occ.prefix.clone(), occ.axis);
    let Some(member) = occ.members.iter().find(|m| m.role == Role::TreeVertices) else {
        return false;
    };
    let path = occ.path_of(member);

    let mut current = data.clone();
    let mut changed = false;
    while let Some(Value::Graph(tree)) = value_at(&current.values, &path).cloned() {
        let leaves = tree.leaves();
        if leaves.len() < 2 || tree.n <= min {
            break;
        }
        let mut is_leaf = vec![false; tree.n + 1];
        for leaf in &leaves {
            is_leaf[*leaf] = true;
        }
        let internal: Vec<usize> = (1..=tree.n).filter(|v| !is_leaf[*v]).collect();
        // Keep enough leaves to satisfy the declared vertex floor.
        let min_leaves = min.saturating_sub(internal.len());

        // Re-derived each round: the previous projection changed the data the
        // occurrences describe.
        let all = current.all_occurrences();
        let Some(round) = all.iter().find(|o| o.prefix == key.0 && o.axis == key.1) else {
            break;
        };

        // Vertex labels are one-based in the file; masks are zero-based
        // positions on the axis.
        let mask_of = |kept_labels: &[usize]| -> Vec<usize> {
            let mut mask: Vec<usize> = kept_labels.iter().map(|label| label - 1).collect();
            mask.sort_unstable();
            mask
        };
        let candidate_labels = |chosen: &[usize]| -> Vec<usize> {
            let mut kept = internal.clone();
            kept.extend(chosen.iter().filter_map(|&i| leaves.get(i).copied()));
            kept.sort_unstable();
            kept
        };

        let positions: Vec<usize> = (0..leaves.len()).collect();
        let kept_leaves = ddmin_min_len(&positions, min_leaves, |chosen| {
            let mask = mask_of(&candidate_labels(chosen));
            match project_occurrence(&current, round, &mask, &all) {
                Some(trial) => accept(&trial),
                None => false,
            }
        });
        if kept_leaves.len() == leaves.len() {
            break;
        }

        let mask = mask_of(&candidate_labels(&kept_leaves));
        let Some(next) = project_occurrence(&current, round, &mask, &all) else {
            break;
        };
        current = next;
        changed = true;
    }

    if changed {
        *data = current;
    }
    changed
}

fn select_columns(grid: &[Vec<i64>], keep: &[usize]) -> Vec<Vec<i64>> {
    grid.iter()
        .map(|row| keep.iter().filter_map(|c| row.get(*c).copied()).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(schema_text: &str, input: &str) -> SchemaData {
        let schema = parse_schema(schema_text).expect("schema should parse");
        parse_input(&schema, input).expect("input should parse")
    }

    /// Drive both passes to a fixpoint, the way `Shrinker` does.
    fn reduce(data: &SchemaData, predicate: impl Fn(&str) -> bool) -> SchemaData {
        let mut accept = |candidate: &SchemaData| predicate(&candidate.render());
        let mut current = data.clone();
        for _ in 0..16 {
            let before = current.clone();
            current = current.structural_pass(&mut accept);
            current = current.value_pass(&mut accept);
            if current == before {
                break;
            }
        }
        current
    }

    /// The schedule alternates structural and value reduction and retries both
    /// until a fixed point, so a structural step that is *currently* infeasible
    /// is not lost -- the next round sees it again once values have shrunk.
    /// Dynamic bounds are about to depend on exactly this, so it is pinned
    /// here, with static bounds, before that feature exists.
    #[test]
    fn structural_shrinking_is_retried_after_the_value_pass() {
        let text = "int N in 1..10
array A[N] in 0..999
";
        let data = build(
            text,
            "5
7 7 7 7 7
",
        );

        // Dropping an element is allowed only once every value is at most 1.
        // Round 1 therefore cannot shrink `N` at all; the value pass then makes
        // it feasible and round 2 takes it down.
        let out = reduce(&data, |rendered| {
            let v = ints(rendered);
            let (n, rest) = v.split_first().expect("a count");
            *n as usize == rest.len() && (rest.iter().all(|x| *x <= 1) || rest.len() >= 5)
        });
        assert_eq!(ints(&out.render()), vec![1, 0]);
    }

    // ---- dynamic numeric bounds -----------------------------------------

    /// 1. `array A[N] in 1..N` parses and round-trips unchanged.
    #[test]
    fn a_count_bounded_array_round_trips() {
        let text = "int N in 1..10\narray A[N] in 1..N\n";
        let input = "3\n1 3 2\n";
        let data = build(text, input);
        assert_eq!(data.render(), input);
        assert_eq!(parse_input(&data.schema, &data.render()).unwrap(), data);

        // Out of range for the *current* N, so reading refuses it.
        let err = parse_input(&data.schema, "3\n1 4 2\n").unwrap_err();
        assert!(err.contains("outside the declared range 1..3"), "{err}");
    }

    /// 2. It is an ordinary integer array. Duplicates are legal, and it gains
    ///    none of the identity machinery: there is no codomain to reference.
    #[test]
    fn a_count_bounded_array_is_not_a_permutation() {
        let text = "int N in 1..10\narray A[N] in 1..N\n";
        let data = build(text, "3\n2 2 2\n");
        assert_eq!(ints(&data.render()), vec![3, 2, 2, 2]);

        // No second axis exists, so nothing can follow A's values.
        let err = parse_schema("int N in 1..10\narray A[N] in 1..N\narray W[A.values]\n")
            .expect_err("A is not a permutation");
        assert!(err.contains('A'), "{err}");
    }

    /// 3. Every candidate the oracle is offered satisfies its own dynamic
    ///    bound. Structural deletion can pull `N` under a surviving magnitude,
    ///    and when it does the candidate is withheld rather than repaired.
    #[test]
    fn structural_candidates_never_break_a_dynamic_bound() {
        let text = "int N in 1..10\narray A[N] in 1..N\n";
        let data = build(text, "5\n1 2 3 4 5\n");

        let mut seen = 0usize;
        let out = data.structural_pass(&mut |candidate| {
            seen += 1;
            let v = ints(&candidate.render());
            let (n, rest) = v.split_first().expect("a count");
            assert!(
                rest.iter().all(|x| *x >= 1 && *x <= *n),
                "candidate {v:?} violates 1..{n}"
            );
            true
        });
        assert!(seen > 0, "the pass must actually offer candidates");
        let v = ints(&out.render());
        let (n, rest) = v.split_first().expect("a count");
        assert!(rest.iter().all(|x| *x >= 1 && *x <= *n));
    }

    /// 4. The point of choosing rejection over clamping: a deletion that is
    ///    infeasible now becomes feasible after the value pass, and the
    ///    schedule retries it. Round 1 cannot shrink `N` at all here.
    #[test]
    fn n_shrinks_only_after_values_do() {
        let text = "int N in 1..10\narray A[N] in 1..N\n";
        let data = build(text, "5\n5 5 5 5 5\n");

        // One round on its own gets stuck: every deletion leaves a 5 behind.
        let mut accept = |_: &SchemaData| true;
        let one_round = data.structural_pass(&mut accept);
        assert_eq!(
            ints(&one_round.render()),
            vec![5, 5, 5, 5, 5, 5],
            "structure alone cannot move"
        );

        // The full schedule alternates, so values drop to 1 and the next
        // structural round takes N all the way down.
        let out = reduce(&data, |_| true);
        assert_eq!(ints(&out.render()), vec![1, 1]);
    }

    /// 5. The scalar case falls out of the same resolution.
    #[test]
    fn a_scalar_may_be_bounded_by_a_count() {
        let text = "int N in 1..10\narray A[N] in 0..999\nint X in 1..N\n";
        let data = build(text, "3\n7 8 9\n3\n");
        assert_eq!(parse_input(&data.schema, &data.render()).unwrap(), data);

        let err = parse_input(&data.schema, "3\n7 8 9\n4\n").unwrap_err();
        assert!(err.contains("outside the declared range 1..3"), "{err}");

        // X is an ordinary magnitude, so it shrinks toward its resolved floor.
        let out = reduce(&data, |_| true);
        assert_eq!(ints(&out.render()), vec![1, 0, 1]);
    }

    /// 6. Inside a `repeat`, a bound resolves against its own instance. The
    ///    input below is legal for instance 0 and illegal for instance 1, so a
    ///    resolver that leaked the first `N` would wrongly accept it.
    #[test]
    fn a_dynamic_bound_resolves_within_its_own_instance() {
        let text = "int T in 1..3\nrepeat T {\n  int N in 1..10\n  array A[N] in 1..N\n}\n";
        let data = build(text, "2\n3\n3 1 1\n2\n1 2\n");
        assert_eq!(parse_input(&data.schema, &data.render()).unwrap(), data);

        // 3 is fine under instance 0's N=3 and impossible under instance 1's
        // N=2. Reading must judge each iteration on its own count.
        let err = parse_input(&data.schema, "2\n3\n3 1 1\n2\n3 1\n").unwrap_err();
        assert!(err.contains("outside the declared range 1..2"), "{err}");
    }

    /// 6b. The reduce-side twin of the test above. Reading resolves against
    ///     the block it is building; projection resolves through
    ///     `block_values_at`, which is a separate path -- and at first only the
    ///     benchcase caught an injected fault in it.
    #[test]
    fn a_projected_candidate_resolves_its_own_instance() {
        let text = "int T in 1..3
repeat T {
  int N in 1..10
  array A[N] in 1..N
}
";
        let data = build(
            text,
            "2
5
1 1 1 1 1
2
2 2
",
        );

        let axis = data
            .schema
            .sizing_axis(&Ref::Name("N".into()))
            .expect("N has an axis");
        let all = data.all_occurrences();
        let instances: Vec<Occurrence> = all.iter().filter(|o| o.axis == axis).cloned().collect();
        assert_eq!(instances.len(), 2, "one occurrence per iteration");

        // Instance 1 keeps one of its two positions, leaving the value 2 under
        // a new N of 1. Instance 0's N is still 5, so a resolver reading the
        // wrong block would wave this candidate through.
        let kept = project_occurrence(&data, &instances[1], &[0], &all).expect("projects");
        assert_eq!(ints(&kept.render()), vec![2, 5, 1, 1, 1, 1, 1, 1, 2]);
        assert!(!dynamic_bounds_hold(&kept), "2 is out of range for N = 1");
    }

    /// 7. A range may only name an `int` declared earlier in the same block.
    #[test]
    fn an_unknown_or_forward_bound_name_is_rejected() {
        let unknown =
            parse_schema("int N in 1..10\narray A[N] in 1..M\n").expect_err("M does not exist");
        assert!(unknown.contains("no `int` named `M`"), "{unknown}");

        let forward =
            parse_schema("array A[3] in 1..M\nint M in 1..10\n").expect_err("M is declared later");
        assert!(forward.contains("no `int` named `M`"), "{forward}");

        let other_block =
            parse_schema("int T in 1..3\nrepeat T {\n  int N in 1..10\n}\narray A[3] in 1..N\n")
                .expect_err("N lives in the repeat body");
        assert!(other_block.contains("no `int` named `N`"), "{other_block}");
    }

    /// 8. Only an `int` can bound a range, and the error says so directly
    ///    rather than reporting the name as missing.
    #[test]
    fn a_non_integer_declaration_cannot_bound_a_range() {
        let err = parse_schema("int N in 1..10\narray B[N] in 0..9\narray A[3] in 1..B\n")
            .expect_err("B is an array");
        assert!(err.contains("`B` is not an `int`"), "{err}");
    }

    /// 9. A literal range resolves to itself, whatever the data says, so every
    ///    schema written before this feature behaves exactly as it did.
    #[test]
    fn literal_bounds_are_unaffected_by_resolution() {
        let literal = Bounds {
            lo: Some(Bound::Lit(2)),
            hi: Some(Bound::Lit(7)),
        };
        assert!(!literal.is_dynamic());
        assert_eq!(
            literal.resolve(&[Value::Int(99)]),
            Limits {
                lo: Some(2),
                hi: Some(7)
            }
        );
        assert_eq!(literal.resolve(&[]), literal.resolve(&[Value::Int(1)]));

        let text = "int N in 1..10\narray A[N] in 1..5\n";
        assert!(!any_dynamic_bounds(&parse_schema(text).unwrap().items));
    }

    /// 10. `index I[K] into N` and `int X in 1..N` both mention `N` and mean
    ///     entirely different things. Narrowing `N` renumbers the reference,
    ///     because it names an element; the magnitude is left alone.
    #[test]
    fn a_reference_renumbers_where_a_magnitude_does_not() {
        let text = "int N in 1..10\narray A[N] in 0..999\nint K in 0..10\n\
                    index I[K] into N\nint X in 1..N\n";
        let data = build(text, "4\n10 20 30 40\n2\n3 4\n2\n");

        let (occ, all) = occurrence_for(&data, "N");
        // Keep the last three positions: I's references 3 and 4 become 2 and 3.
        let kept = project_occurrence(&data, &occ, &[1, 2, 3], &all).expect("projects");
        assert_eq!(ints(&kept.render()), vec![3, 20, 30, 40, 2, 2, 3, 2]);
        // X was 2 before and is 2 after: a magnitude, not a position.
        assert_eq!(*ints(&kept.render()).last().unwrap(), 2);
    }

    /// 11. A permutation beside a dynamic bound keeps identity semantics, and
    ///     the bound does not disturb its cascade.
    #[test]
    fn a_permutation_is_unaffected_by_a_neighbouring_dynamic_bound() {
        let text = "int N in 1..10\npermutation P[N]\narray A[N] in 1..N\n";
        let data = build(text, "4\n3 1 4 2\n4 4 4 4\n");

        let (occ, all) = occurrence_for(&data, "N");
        let kept = project_occurrence(&data, &occ, &[0, 2], &all).expect("projects");
        // P keeps its identity: labels 3 and 4 renumber to 1 and 2. A's values
        // are magnitudes and stay 4 -- which is now out of range, so the
        // candidate exists but the guard withholds it.
        assert_eq!(ints(&kept.render()), vec![2, 1, 2, 4, 4]);
        assert!(!dynamic_bounds_hold(&kept));
    }

    /// 12. Three identity producers and a numeric bound in one schema. The
    ///     bound must contribute nothing to propagation, so the masks are
    ///     identical to the same schema without it.
    #[test]
    fn a_dynamic_bound_adds_no_mask_propagation() {
        let with = "int N in 1..10\nint M in 0..20\ngraph E[M] vertices N\n\
                    index I[M] into N\npermutation P[N]\narray W[N] in 1..N\n";
        let without = "int N in 1..10\nint M in 0..20\ngraph E[M] vertices N\n\
                       index I[M] into N\npermutation P[N]\n";
        let input = "4 2\n1 2\n3 4\n1 3\n2 1 4 3\n";

        let a = build(with, &format!("{input}1 2 3 4\n"));
        let b = build(without, input);

        let (occ_a, all_a) = occurrence_for(&a, "N");
        let (occ_b, all_b) = occurrence_for(&b, "N");
        let state_a = propagate(&a, &occ_a, &[0, 1, 2], &all_a);
        let state_b = propagate(&b, &occ_b, &[0, 1, 2], &all_b);

        // Same axes, same masks, same number of worklist updates.
        let masks = |s: &Propagation| -> Vec<(usize, Vec<usize>)> {
            s.masks
                .iter()
                .map(|((_, ax), m)| (*ax, m.clone()))
                .collect()
        };
        assert_eq!(masks(&state_a), masks(&state_b));
        assert_eq!(state_a.updates, state_b.updates);
    }

    fn ints(text: &str) -> Vec<i64> {
        text.split_whitespace()
            .filter_map(|t| t.parse::<i64>().ok())
            .collect()
    }

    /// The whole point of the feature: an oracle that accepts everything still
    /// cannot push the input outside what the problem statement allows.
    #[test]
    fn declared_bounds_floor_both_length_and_values() {
        let data = build(
            "int N in 1..100\narray A[N] in 1..1000000000\n",
            "5\n7 9 11 13 15\n",
        );
        let reduced = reduce(&data, |_| true);
        assert_eq!(ints(&reduced.render()), vec![1, 1]);
    }

    #[test]
    fn without_bounds_the_same_input_collapses_to_nothing() {
        let data = build("int N\narray A[N]\n", "5\n7 9 11 13 15\n");
        let reduced = reduce(&data, |_| true);
        assert_eq!(ints(&reduced.render()), vec![0]);
    }

    #[test]
    fn repeat_blocks_reduce_to_the_iteration_that_matters() {
        let data = build(
            "int T in 1..10\nrepeat T {\n  int N in 1..10\n  array A[N] in -1000..1000\n}\n",
            "3\n2\n5 6\n3\n-7 8 9\n1\n4\n",
        );
        let reduced = reduce(&data, |t| ints(t).iter().any(|v| *v < 0));
        assert_eq!(ints(&reduced.render()), vec![1, 1, -1]);
    }

    #[test]
    fn matrices_reduce_in_both_dimensions() {
        let data = build(
            "int R in 1..50\nint C in 1..50\nmatrix G[R][C] in 0..1\n",
            "3 4\n0 0 0 0\n0 0 1 0\n0 0 0 0\n",
        );
        // The grid must keep a set cell; R and C are the first two tokens.
        let reduced = reduce(&data, |t| ints(t).iter().skip(2).any(|v| *v == 1));
        assert_eq!(ints(&reduced.render()), vec![1, 1, 1]);
    }

    #[test]
    fn a_literal_count_is_fixed_but_its_values_still_shrink() {
        let data = build("array A[3] in 0..9\n", "5 6 7\n");
        let reduced = reduce(&data, |_| true);
        assert_eq!(ints(&reduced.render()), vec![0, 0, 0]);
    }

    #[test]
    fn a_tree_stays_a_tree_and_respects_its_vertex_floor() {
        let data = build("int N in 2..100\ntree E vertices N\n", "4\n1 2\n1 3\n3 4\n");
        let reduced = reduce(&data, |_| true);
        // Two vertices, one edge: the smallest tree the schema permits.
        assert_eq!(ints(&reduced.render()), vec![2, 1, 2]);
    }

    #[test]
    fn graphs_shed_edges_and_vertices() {
        let data = build(
            "int N in 1..100\nint M in 0..1000\ngraph E[M] vertices N\n",
            "4 4\n1 2\n2 3\n3 4\n4 1\n",
        );
        let reduced = reduce(&data, |_| true);
        assert_eq!(ints(&reduced.render()), vec![1, 0]);
    }

    /// Whatever the reducer produces must still read back as the same thing.
    #[test]
    fn reduced_schema_inputs_round_trip() {
        let cases = [
            (
                "int N in 1..100\narray A[N] in 1..1000\n",
                "5\n7 9 11 13 15\n",
            ),
            (
                "int T in 1..10\nrepeat T {\n  int N in 1..10\n  array A[N] in -1000..1000\n}\n",
                "3\n2\n5 6\n3\n-7 8 9\n1\n4\n",
            ),
            (
                "int R in 1..50\nint C in 1..50\nmatrix G[R][C] in 0..1\n",
                "3 4\n0 0 0 0\n0 0 1 0\n0 0 0 0\n",
            ),
            ("int N in 2..100\ntree E vertices N\n", "4\n1 2\n1 3\n3 4\n"),
        ];
        for (schema_text, input) in cases {
            let data = build(schema_text, input);
            for predicate in [0usize, 1] {
                let reduced = reduce(&data, |t| match predicate {
                    0 => true,
                    _ => ints(t).len() >= 3,
                });
                let text = reduced.render();
                let reparsed = parse_input(&data.schema, &text)
                    .unwrap_or_else(|e| panic!("{schema_text:?} produced {text:?}: {e}"));
                assert_eq!(reparsed, reduced, "round trip changed the data");
            }
        }
    }

    #[test]
    fn input_outside_a_declared_range_is_rejected() {
        let schema = parse_schema("int N in 1..10\n").unwrap();
        let err = parse_input(&schema, "50\n").unwrap_err();
        assert!(err.contains("outside the declared range"), "{err}");
    }

    #[test]
    fn leftover_input_is_reported_rather_than_ignored() {
        let schema = parse_schema("int N\narray A[N]\n").unwrap();
        let err = parse_input(&schema, "2\n1 2 3 4\n").unwrap_err();
        assert!(err.contains("left over"), "{err}");
    }

    #[test]
    fn truncated_input_is_reported() {
        let schema = parse_schema("int N\narray A[N]\n").unwrap();
        let err = parse_input(&schema, "5\n1 2\n").unwrap_err();
        assert!(err.contains("input ended"), "{err}");
    }

    /// Two declarations sized by one count are one axis occurrence, so a single
    /// mask projects both. They cannot disagree, because they are never asked
    /// separately.
    ///
    /// The predicate needs position 2 of `A` and position 1 of `B`. With one
    /// shared mask neither can be dropped, so the survivor set is their union
    /// and both arrays keep two elements.
    #[test]
    fn two_declarations_sharing_a_count_select_together() {
        let text = "int N in 1..10\narray A[N] in -100..100\narray B[N] in -100..100\n";
        let data = build(text, "4\n1 2 3 4\n10 20 30 40\n");

        let reduced = reduce(&data, |t| {
            let v = ints(t);
            v.contains(&3) && v.contains(&20)
        });

        // N, then A, then B. One cardinality, two synchronised projections.
        assert_eq!(ints(&reduced.render()), vec![2, 0, 3, 20, 0]);

        let (Value::Int(n), Value::Array(a), Value::Array(b)) =
            (&reduced.values[0], &reduced.values[1], &reduced.values[2])
        else {
            panic!("unexpected shape")
        };
        assert_eq!(*n, 2);
        assert_eq!(a.len(), b.len(), "a shared count cannot desynchronise");
        assert_eq!(a.len() as i64, *n);
        assert_eq!(
            parse_input(&reduced.schema, &reduced.render()).unwrap(),
            reduced
        );
    }

    /// One count indexing both dimensions of a matrix. The same mask applies to
    /// rows and columns, so a square stays square.
    #[test]
    fn a_square_matrix_shares_one_count_across_both_axes() {
        let text = "int N in 1..10\nmatrix G[N][N] in 0..9\n";
        let data = build(text, "3\n1 0 0\n0 5 0\n0 0 9\n");

        let reduced = reduce(&data, |t| {
            let v = ints(t);
            v.contains(&5) && v.contains(&9)
        });

        assert_eq!(ints(&reduced.render()), vec![2, 5, 0, 0, 9]);
        let Value::Matrix(grid) = &reduced.values[1] else {
            panic!("expected a matrix")
        };
        assert_eq!(grid.len(), 2);
        for row in grid {
            assert_eq!(row.len(), grid.len(), "the matrix stopped being square");
        }
    }

    /// The adversarial case: one shared axis, two outer instances, different
    /// survivors in each. This is the relation-era shape of the bug revision 5
    /// exposed -- anything keyed by the static `AxisId` gives both the same
    /// mask.
    #[test]
    fn a_shared_count_selects_independently_in_each_outer_instance() {
        let text = "int T in 1..5\nrepeat T {\n  int N in 1..10\n  \
                    array A[N] in -100..100\n  array B[N] in -100..100\n}\n";
        let data = build(text, "2\n3\n1 7 3\n10 11 12\n3\n4 5 6\n20 21 -9\n");

        // 7 lives at position 1 of the first instance, -9 at position 2 of the
        // second: the two occurrences must keep different positions.
        let reduced = reduce(&data, |t| {
            let v = ints(t);
            v.contains(&7) && v.contains(&-9)
        });

        assert_eq!(ints(&reduced.render()), vec![2, 1, 7, 0, 1, 0, -9]);

        let Value::Repeat(iters) = &reduced.values[1] else {
            panic!("expected a repeat")
        };
        assert_eq!(iters.len(), 2);
        for (k, iter) in iters.iter().enumerate() {
            let (Value::Int(n), Value::Array(a), Value::Array(b)) = (&iter[0], &iter[1], &iter[2])
            else {
                panic!("unexpected shape in instance {k}")
            };
            assert_eq!(a.len(), b.len(), "instance {k} desynchronised");
            assert_eq!(a.len() as i64, *n, "instance {k} count disagrees");
        }
        // The kept positions really did differ: 7 came from index 1, -9 from 2.
        let survivors: Vec<(Vec<i64>, Vec<i64>)> = iters
            .iter()
            .map(|iter| match (&iter[1], &iter[2]) {
                (Value::Array(a), Value::Array(b)) => (a.clone(), b.clone()),
                _ => panic!(),
            })
            .collect();
        assert_eq!(survivors[0], (vec![7], vec![0]));
        assert_eq!(survivors[1], (vec![0], vec![-9]));
    }

    /// The occurrence a graph's vertices live on, plus the occurrences an
    /// induced selection may reach.
    fn vertex_occurrence(data: &SchemaData) -> (Occurrence, Vec<Occurrence>) {
        let mut all = Vec::new();
        occurrences(
            &data.schema,
            &data.schema.items,
            &data.values,
            &Vec::new(),
            &mut all,
        );
        let occ = all
            .iter()
            .find(|o| o.members.iter().any(|m| m.role == Role::GraphVertices))
            .expect("a vertex occurrence")
            .clone();
        let siblings = all
            .iter()
            .filter(|o| o.prefix == occ.prefix)
            .cloned()
            .collect();
        (occ, siblings)
    }

    fn graph_of(data: &SchemaData, at: usize) -> &GraphCase {
        match &data.values[at] {
            Value::Graph(g) => g,
            other => panic!("expected a graph, got {other:?}"),
        }
    }

    /// The section 5 example. A vertex selection induces a selection on the
    /// *edge* axis, and that induced selection projects every member of the
    /// edge axis -- not just the graph that produced it.
    #[test]
    fn a_vertex_selection_induces_the_edge_selection_and_fans_it_out() {
        let text = "int N in 1..10\nint M in 0..20\ngraph E[M] vertices N\n\
                    array W[M] in 0..99\n";
        let data = build(text, "5 5\n1 2\n2 3\n3 4\n4 5\n1 5\n10 20 30 40 50\n");

        // Keep vertices 1, 2, 4, 5 (positions 0, 1, 3, 4). Edges (2,3) and
        // (3,4) each lose an endpoint, so the survivors are positions 0, 3 and
        // 4 -- deliberately non-contiguous.
        assert_eq!(
            induced_edge_keep(graph_of(&data, 2), &[0, 1, 3, 4]),
            vec![0, 3, 4]
        );

        let (occ, siblings) = vertex_occurrence(&data);
        let projected = project_occurrence(&data, &occ, &[0, 1, 3, 4], &siblings).unwrap();

        // N M, three relabelled edges, then exactly the surviving weights.
        assert_eq!(
            ints(&projected.render()),
            vec![4, 3, 1, 2, 3, 4, 1, 4, 10, 40, 50]
        );
        assert_eq!(
            parse_input(&projected.schema, &projected.render()).unwrap(),
            projected
        );
    }

    /// One induced mask, three members of the edge axis, one projection.
    #[test]
    fn an_induced_edge_selection_projects_every_member_identically() {
        let text = "int N in 1..10\nint M in 0..20\ngraph E[M] vertices N\n\
                    array W[M] in 0..99\narray Label[M] in 0..99\n";
        let data = build(
            text,
            "5 5\n1 2\n2 3\n3 4\n4 5\n1 5\n10 20 30 40 50\n71 72 73 74 75\n",
        );

        let (occ, siblings) = vertex_occurrence(&data);
        let projected = project_occurrence(&data, &occ, &[0, 1, 3, 4], &siblings).unwrap();

        let (Value::Array(w), Value::Array(label)) = (&projected.values[3], &projected.values[4])
        else {
            panic!("expected two arrays")
        };
        // Both took positions 0, 3, 4 -- the same induced mask, not two masks.
        assert_eq!(w, &vec![10, 40, 50]);
        assert_eq!(label, &vec![71, 74, 75]);
        assert_eq!(graph_of(&projected, 2).edges.len(), w.len());
        assert_eq!(
            parse_input(&projected.schema, &projected.render()).unwrap(),
            projected
        );
    }

    /// The adversarial version: the same graph declaration in two repeat
    /// instances, whose vertex selections induce *different* edge masks.
    /// Anything cached against the static edge `AxisId` reuses the first.
    #[test]
    fn induction_stays_inside_its_own_repeat_instance() {
        let text = "int T in 1..3\nrepeat T {\n  int N in 1..10\n  int M in 0..20\n  \
                    graph E[M] vertices N\n  array W[M] in 0..99\n}\n";
        // Instance 0 needs weight 30, instance 1 needs weight 61.
        let data = build(
            text,
            "2\n3 3\n1 2\n2 3\n1 3\n10 20 30\n3 3\n1 2\n2 3\n1 3\n60 61 62\n",
        );

        let reduced = reduce(&data, |t| {
            let v = ints(t);
            v.contains(&30) && v.contains(&61)
        });

        let Value::Repeat(iters) = &reduced.values[1] else {
            panic!("expected a repeat")
        };
        assert_eq!(iters.len(), 2);
        for (k, iter) in iters.iter().enumerate() {
            let (Value::Int(n), Value::Int(m), Value::Graph(g), Value::Array(w)) =
                (&iter[0], &iter[1], &iter[2], &iter[3])
            else {
                panic!("unexpected shape in instance {k}")
            };
            assert_eq!(g.edges.len(), w.len(), "instance {k}: W lost sync with E");
            assert_eq!(*m, w.len() as i64, "instance {k}: M disagrees");
            assert_eq!(*n, g.n as i64, "instance {k}: N disagrees");
            for e in &g.edges {
                assert!(e.u >= 1 && e.u <= g.n && e.v >= 1 && e.v <= g.n);
            }
        }
        // The two instances kept different weights, so different edge masks.
        let weights: Vec<Vec<i64>> = iters
            .iter()
            .map(|iter| match &iter[3] {
                Value::Array(w) => w.clone(),
                _ => panic!(),
            })
            .collect();
        assert!(weights[0].contains(&30));
        assert!(weights[1].contains(&61));
        assert_ne!(weights[0], weights[1]);

        // Pinned exactly. The structural assertions above are not enough on
        // their own: a mask replayed from another instance still projects E and
        // W consistently with each other, so only the *identity* of the
        // surviving edge gives it away.
        assert_eq!(
            ints(&reduced.render()),
            vec![2, 2, 1, 1, 2, 30, 2, 1, 1, 2, 61],
            "each instance must keep the edge its own predicate needs"
        );
        assert_eq!(
            parse_input(&reduced.schema, &reduced.render()).unwrap(),
            reduced
        );
    }

    /// The same static edge `AxisId` in two instances, over graphs of different
    /// shapes, induced with different vertex masks. Anything cached against the
    /// axis identifier replays the first instance's mask onto the second.
    #[test]
    fn each_instance_induces_its_own_edge_mask() {
        let text = "int T in 1..3\nrepeat T {\n  int N in 1..10\n  int M in 0..20\n  \
                    graph E[M] vertices N\n  array W[M] in 0..99\n}\n";
        // Instance 0 is a triangle, instance 1 a four-cycle.
        let data = build(
            text,
            "2\n3 3\n1 2\n2 3\n1 3\n10 20 30\n4 4\n1 2\n2 3\n3 4\n1 4\n60 61 62 63\n",
        );

        let mut all = Vec::new();
        occurrences(
            &data.schema,
            &data.schema.items,
            &data.values,
            &Vec::new(),
            &mut all,
        );
        let vertex_occs: Vec<Occurrence> = all
            .iter()
            .filter(|o| o.members.iter().any(|m| m.role == Role::GraphVertices))
            .cloned()
            .collect();
        assert_eq!(vertex_occs.len(), 2, "one vertex occurrence per instance");
        assert_eq!(
            vertex_occs[0].axis, vertex_occs[1].axis,
            "the same static AxisId"
        );
        assert_ne!(
            vertex_occs[0].prefix, vertex_occs[1].prefix,
            "in different instantiations"
        );

        let instance_graph = |k: usize| -> GraphCase {
            let Value::Repeat(iters) = &data.values[1] else {
                panic!()
            };
            match &iters[k][2] {
                Value::Graph(g) => g.clone(),
                other => panic!("expected a graph, got {other:?}"),
            }
        };

        // Keeping vertices 1,2 of the triangle leaves edge (1,2), position 0.
        assert_eq!(induced_edge_keep(&instance_graph(0), &[0, 1]), vec![0]);
        // Keeping vertices 3,4 of the four-cycle leaves edge (3,4), position 2.
        assert_eq!(induced_edge_keep(&instance_graph(1), &[2, 3]), vec![2]);

        let project = |k: usize, keep: &[usize]| {
            let occ = &vertex_occs[k];
            let siblings: Vec<Occurrence> = all
                .iter()
                .filter(|o| o.prefix == occ.prefix)
                .cloned()
                .collect();
            project_occurrence(&data, occ, keep, &siblings).expect("projects")
        };

        let weights_of = |d: &SchemaData, k: usize| -> Vec<i64> {
            let Value::Repeat(iters) = &d.values[1] else {
                panic!()
            };
            match &iters[k][3] {
                Value::Array(w) => w.clone(),
                other => panic!("expected an array, got {other:?}"),
            }
        };

        // Each instance keeps the weight of *its own* surviving edge, and the
        // sibling instance is untouched.
        let first = project(0, &[0, 1]);
        assert_eq!(weights_of(&first, 0), vec![10]);
        assert_eq!(weights_of(&first, 1), vec![60, 61, 62, 63]);

        let second = project(1, &[2, 3]);
        assert_eq!(weights_of(&second, 1), vec![62]);
        assert_eq!(weights_of(&second, 0), vec![10, 20, 30]);
    }

    /// Every vertex count is shareable now that pruning goes through the same
    /// pipeline as everything else. A vertex-labelled tree is the case this
    /// unlocks: the labels follow the pruning because they are members of the
    /// same occurrence.
    #[test]
    fn a_tree_vertex_count_can_be_shared_with_its_labels() {
        let text = "int N in 2..10\ntree E vertices N\narray Colour[N] in 0..99\n";
        let data = build(text, "4\n1 2\n1 3\n3 4\n10 20 30 77\n");

        // Vertex 4 carries colour 77, so it has to survive.
        let reduced = reduce(&data, |t| ints(t).contains(&77));

        let (Value::Int(n), Value::Graph(tree), Value::Array(colour)) =
            (&reduced.values[0], &reduced.values[1], &reduced.values[2])
        else {
            panic!("unexpected shape")
        };
        assert_eq!(tree.n as i64, *n, "the count disagrees with the tree");
        assert_eq!(colour.len(), tree.n, "labels lost sync with vertices");
        assert_eq!(tree.edges.len(), tree.n - 1, "no longer a tree");
        assert!(colour.contains(&77), "the pinned label is gone");
        assert_eq!(
            parse_input(&reduced.schema, &reduced.render()).unwrap(),
            reduced
        );

        parse_schema("int N in 1..10\nint M in 0..20\ngraph E[M] vertices N\narray D[N] in 0..9\n")
            .expect("a graph vertex count is shareable too");
    }

    /// The tree's validity check, exercised directly. Leaf pruning cannot
    /// generate a disconnected subset, so nothing in normal operation reaches
    /// this -- it is a guard on the generator's guarantee, and only a
    /// hand-built mask makes it observable.
    #[test]
    fn a_disconnected_vertex_selection_rejects() {
        let text = "int N in 2..10\ntree E vertices N\n";
        let data = build(text, "3\n1 2\n2 3\n");
        let (occ, all) = occurrence_for(&data, "N");

        // Keeping the two ends of a path drops the middle, so the survivors
        // are no longer connected and no longer a tree.
        assert!(
            project_occurrence(&data, &occ, &[0, 2], &all).is_none(),
            "a disconnected selection must reject"
        );
        // Keeping an end and the middle is still a tree.
        assert!(project_occurrence(&data, &occ, &[0, 1], &all).is_some());
    }

    /// Regression. Pruning used to write the tree directly and call `resync`,
    /// bypassing propagation, so an `index` into the vertex axis was neither
    /// induced nor renumbered: the reduced input referenced vertices that no
    /// longer existed and could not be re-parsed.
    ///
    /// With a literal index length there is no axis to carry an induced
    /// selection, so a prune that kills a referenced vertex has nowhere to put
    /// the loss and is rejected outright.
    #[test]
    fn pruning_a_tree_cannot_strand_an_index_reference() {
        let text = "int N in 2..10\ntree E vertices N\nindex I[2] into N\n";
        let data = build(text, "5\n1 2\n2 3\n3 4\n4 5\n5 1\n");

        let reduced = reduce(&data, |_| true);
        let rendered = reduced.render();

        // Both ends of the path are referenced, so nothing may be pruned.
        assert_eq!(ints(&rendered), vec![5, 1, 2, 2, 3, 3, 4, 4, 5, 5, 1]);
        assert_eq!(
            parse_input(&reduced.schema, &rendered).unwrap(),
            reduced,
            "a stranded reference would fail to re-parse"
        );
    }

    /// The same schema with a *counted* index: the loss now has somewhere to
    /// go, so references are dropped and the survivors renumbered.
    #[test]
    fn pruning_a_tree_drops_and_renumbers_counted_references() {
        let text = "int N in 2..10\ntree E vertices N\nint K in 0..10\n\
                    index I[K] into N\n";
        let data = build(text, "5\n1 2\n2 3\n3 4\n4 5\n2\n5 1\n");

        let reduced = reduce(&data, |_| true);
        let rendered = reduced.render();

        let (Value::Int(n), Value::Graph(tree), Value::Int(k), Value::Array(refs)) = (
            &reduced.values[0],
            &reduced.values[1],
            &reduced.values[2],
            &reduced.values[3],
        ) else {
            panic!("unexpected shape")
        };
        assert_eq!(tree.n as i64, *n);
        assert_eq!(refs.len() as i64, *k);
        for r in refs {
            assert!(*r >= 1 && r <= n, "reference {r} outside 1..={n}");
        }
        assert_eq!(parse_input(&reduced.schema, &rendered).unwrap(), reduced);
    }

    #[test]
    fn a_count_must_be_declared_in_the_same_block() {
        let err = parse_schema("int N in 1..10\nrepeat 2 {\n  array A[N]\n}\n").unwrap_err();
        assert!(err.contains("same block"), "{err}");
    }

    #[test]
    fn schema_syntax_errors_name_the_line() {
        for (text, needle) in [
            ("int N in 1..10\nwidget W\n", "line 2"),
            ("int N in 10..1\n", "is empty"),
            ("array A[N]\n", "not an `int` declared"),
            ("int T\nrepeat T {\n  int N\n", "unterminated"),
            ("tree E[5] vertices 4\n", "no edge count of its own"),
        ] {
            let err = parse_schema(text).unwrap_err();
            assert!(err.contains(needle), "{text:?} gave {err:?}");
        }
    }

    /// Schemas covering every declaration kind, reused by the migration tests.
    fn sample_schemas() -> Vec<&'static str> {
        vec![
            "int N in 1..100\narray A[N] in 1..1000\n",
            "int R in 1..50\nint C in 1..50\nmatrix G[R][C] in 0..1\n",
            "int T in 1..10\nrepeat T {\n  int N in 1..10\n  array A[N] in -1000..1000\n}\n",
            "int N in 2..100\ntree E vertices N\n",
            "int N in 1..100\nint M in 0..1000\ngraph E[M] vertices N\n",
            "int K in 0..100\nint T in 1..10\nrepeat T {\n  int N in 1..10\n  \
             array A[N] in -1000..1000\n}\n",
            "array A[3] in 0..9\n",
        ]
    }

    fn int_names(items: &[Decl], out: &mut Vec<String>) {
        for decl in items {
            match decl {
                Decl::Int { name, .. } => out.push(name.clone()),
                Decl::Repeat { body, .. } => int_names(body, out),
                _ => {}
            }
        }
    }

    /// Migration invariant. The count arena and the `derived` set must describe
    /// exactly the same thing while both exist. When `derived` is finally
    /// deleted this test goes with it.
    #[test]
    fn the_count_arena_agrees_with_the_derived_set() {
        for text in sample_schemas() {
            let schema = parse_schema(text).unwrap_or_else(|e| panic!("{text:?}: {e}"));

            let mut names = Vec::new();
            int_names(&schema.items, &mut names);
            for name in &names {
                assert_eq!(
                    schema.is_derived(name),
                    schema.count_id(name).is_some(),
                    "{text:?}: `{name}` disagrees between the two descriptions"
                );
            }

            let derived_count = names.iter().filter(|n| schema.is_derived(n)).count();
            assert_eq!(
                derived_count,
                schema.count_ids().len(),
                "{text:?}: wrong number of counts"
            );
        }
    }

    /// One axis per count, pointing back at it. Several axes per count is what
    /// shared dimensions add later; this pins the current arity so that change
    /// is visible when it happens.
    #[test]
    fn every_count_has_exactly_one_axis_pointing_back() {
        for text in sample_schemas() {
            let schema = parse_schema(text).unwrap();
            assert_eq!(schema.axes.len(), schema.counts.len(), "{text:?}");
            for id in schema.count_ids() {
                let axis = schema.default_axis(id);
                assert_eq!(
                    schema.axis(axis).count,
                    id,
                    "{text:?}: axis {axis} does not point back at count {id}"
                );
                assert_eq!(schema.count_id(&schema.count(id).name), Some(id));
            }
        }
    }

    /// Identifiers are allocated in declaration order, including inside a
    /// repeat body, so they are stable across runs and reviewable in a diff.
    #[test]
    fn count_ids_follow_declaration_order() {
        let schema = parse_schema(
            "int K in 0..100\nint T in 1..10\nrepeat T {\n  int N in 1..10\n  \
             array A[N] in -1000..1000\n}\n",
        )
        .unwrap();
        // K is not used as a count, so it gets none. T then N, outer first.
        assert_eq!(schema.count_id("K"), None);
        assert_eq!(schema.count_id("T"), Some(0));
        assert_eq!(schema.count_id("N"), Some(1));
    }

    /// Every (enclosing block, declaration) pair, without needing any data.
    fn walk_decls<'a>(items: &'a [Decl], out: &mut Vec<(&'a [Decl], &'a Decl)>) {
        for decl in items {
            out.push((items, decl));
            if let Decl::Repeat { body, .. } = decl {
                walk_decls(body, out);
            }
        }
    }

    fn sizing_refs(decl: &Decl) -> Vec<&Ref> {
        match decl {
            Decl::Int { .. } => vec![],
            Decl::Array { len, .. } => vec![len],
            Decl::Matrix { rows, cols, .. } => vec![rows, cols],
            Decl::Tree { verts, .. } => vec![verts],
            Decl::Graph { edges, verts, .. } => vec![edges, verts],
            Decl::Index { len, .. } => vec![len],
            Decl::Permutation { len, .. } => vec![len],
            Decl::Repeat { count, .. } => vec![count],
        }
    }

    /// Migration invariant. Resolving a size through the axis arena must give
    /// exactly what scanning the enclosing block gave, for every sizing
    /// position in every declaration kind — including inside a repeat body,
    /// where the block-local scan was the whole reason that helper existed.
    #[test]
    fn arena_lookup_matches_the_old_block_scan() {
        let mut checked = 0usize;
        for text in sample_schemas() {
            let schema = parse_schema(text).unwrap();
            let mut pairs = Vec::new();
            walk_decls(&schema.items, &mut pairs);
            for (block, decl) in pairs {
                for r in sizing_refs(decl) {
                    checked += 1;
                    assert_eq!(
                        count_bounds_by_scan(block, r),
                        schema.sizing_bounds(r),
                        "{text:?}: {r:?} resolves differently"
                    );
                }
            }
        }
        assert!(checked >= 10, "only {checked} sizing positions exercised");
    }

    /// A literal size is fixed by the schema, so it has no axis and the
    /// reducer must leave its length alone.
    #[test]
    fn a_literal_size_has_no_axis() {
        let schema = parse_schema(
            "array A[3] in 0..9
",
        )
        .unwrap();
        assert_eq!(schema.sizing_axis(&Ref::Lit(3)), None);
        assert_eq!(schema.sizing_bounds(&Ref::Lit(3)), None);
    }

    /// v0.4 alignment comes from topology: both declarations resolve to the
    /// same axis, rather than being kept in step by a repair pass.
    #[test]
    fn declarations_on_one_count_share_its_single_axis() {
        let schema = parse_schema(
            "int T in 1..10
repeat T {
  int N in 1..10
  array A[N]
}
",
        )
        .unwrap();
        let t_axis = schema.sizing_axis(&Ref::Name("T".into())).unwrap();
        let n_axis = schema.sizing_axis(&Ref::Name("N".into())).unwrap();
        assert_ne!(t_axis, n_axis, "different counts must not share an axis");
        assert_eq!(schema.axis(t_axis).count, schema.count_id("T").unwrap());
        assert_eq!(schema.axis(n_axis).count, schema.count_id("N").unwrap());
    }

    /// Invariant 15 of the design note, and the case that caught revision 5.
    ///
    /// One `int N` declaration, three live instances, three different values.
    /// A `CountId` names the declaration; the cardinality lives in each
    /// iteration's own slot. Anything that stores a cardinality *on* the count
    /// collapses these three into one and fails here.
    #[test]
    fn a_count_inside_a_repeat_differs_per_iteration() {
        let text = "int T in 1..10\nrepeat T {\n  int N in 1..10\n  \
                    array A[N] in -1000..1000\n}\n";
        let data = build(text, "3\n2\n5 6\n3\n-7 8 9\n1\n4\n");

        let ns = |d: &SchemaData| -> Vec<i64> {
            let Value::Repeat(iters) = &d.values[1] else {
                panic!("expected a repeat")
            };
            iters
                .iter()
                .map(|iter| match &iter[0] {
                    Value::Int(v) => *v,
                    other => panic!("expected the count, got {other:?}"),
                })
                .collect()
        };
        let lens = |d: &SchemaData| -> Vec<usize> {
            let Value::Repeat(iters) = &d.values[1] else {
                panic!("expected a repeat")
            };
            iters
                .iter()
                .map(|iter| match &iter[1] {
                    Value::Array(a) => a.len(),
                    other => panic!("expected the array, got {other:?}"),
                })
                .collect()
        };

        assert_eq!(ns(&data), vec![2, 3, 1], "one CountId, three values");
        assert_eq!(lens(&data), vec![2, 3, 1]);

        // Resync must not make the instances agree with one another.
        let mut resynced = data.clone();
        resynced.resync();
        assert_eq!(
            resynced, data,
            "resync disturbed an already consistent model"
        );
        assert_eq!(ns(&resynced), vec![2, 3, 1]);

        // Editing one iteration must leave every other iteration's count alone.
        let mut edited = data.clone();
        let Value::Repeat(iters) = &mut edited.values[1] else {
            panic!()
        };
        iters[1] = vec![Value::Int(3), Value::Array(vec![-7])];
        edited.resync();

        assert_eq!(
            ns(&edited),
            vec![2, 1, 1],
            "only the edited iteration's count should have moved"
        );
        assert_eq!(lens(&edited), vec![2, 1, 1]);

        // And the result is still a legal input that reads back identically.
        let rendered = edited.render();
        assert_eq!(parse_input(&edited.schema, &rendered).unwrap(), edited);
    }

    /// The same independence has to survive an actual reduction, not just a
    /// hand-made edit.
    #[test]
    fn reduction_keeps_repeat_instances_independent() {
        let text = "int T in 1..10\nrepeat T {\n  int N in 1..10\n  \
                    array A[N] in -1000..1000\n}\n";
        let data = build(text, "3\n2\n5 6\n3\n-7 8 9\n1\n4\n");

        // Keep at least two iterations alive so independence is observable.
        let reduced = reduce(&data, |t| {
            let ints = ints(t);
            ints.first() == Some(&3) && ints.iter().any(|v| *v < 0)
        });

        let Value::Repeat(iters) = &reduced.values[1] else {
            panic!("expected a repeat")
        };
        assert_eq!(iters.len(), 3, "the predicate pinned T at 3");
        for (i, iter) in iters.iter().enumerate() {
            let (Value::Int(n), Value::Array(a)) = (&iter[0], &iter[1]) else {
                panic!("unexpected shape in iteration {i}")
            };
            assert_eq!(
                *n as usize,
                a.len(),
                "iteration {i}: count {n} does not match its own array"
            );
        }
        assert_eq!(
            parse_input(&reduced.schema, &reduced.render()).unwrap(),
            reduced
        );
    }

    /// Every count, at every nesting depth, must match the length of the data
    /// in *its own* instantiation. A mask leaking between occurrences shows up
    /// here as a count that describes a sibling's data.
    fn assert_counts_match_own_data(schema: &Schema, items: &[Decl], values: &[Value], at: &str) {
        let expect = |r: &Ref, actual: usize, what: &str| {
            if let Ref::Name(n) = r {
                if let Some(id) = schema.count_id(n) {
                    let Value::Int(declared) = &values[schema.count_slot(id)] else {
                        panic!("{at}: count {n} is not an int slot");
                    };
                    assert_eq!(
                        *declared, actual as i64,
                        "{at}: count {n} says {declared} but {what} has {actual}"
                    );
                }
            }
        };
        for (i, decl) in items.iter().enumerate() {
            match (decl, &values[i]) {
                (Decl::Array { len, .. }, Value::Array(a)) => expect(len, a.len(), "its array"),
                (Decl::Index { len, .. }, Value::Array(r)) => expect(len, r.len(), "its indices"),
                (Decl::Repeat { count, body }, Value::Repeat(iters)) => {
                    expect(count, iters.len(), "its block");
                    for (k, iter) in iters.iter().enumerate() {
                        assert_counts_match_own_data(schema, body, iter, &format!("{at}/iter{k}"));
                    }
                }
                _ => {}
            }
        }
    }

    /// Acceptance criterion for step 2: two instantiations of one declared axis
    /// converge to **different** keep-masks.
    ///
    /// `N` is a single declaration with a single `AxisId`. The predicate needs
    /// a value from position 1 of the first array and position 2 of the second,
    /// so the two occurrences cannot select the same positions. Anything that
    /// stores a mask per `AxisId` gives them the same one and fails.
    #[test]
    fn two_instances_of_one_axis_reach_different_masks() {
        let text = "int T in 1..5\nrepeat T {\n  int N in 1..10\n  \
                    array A[N] in -100..100\n}\n";
        let data = build(text, "2\n4\n1 7 2 3\n4\n4 5 -3 6\n");

        let reduced = reduce(&data, |t| {
            let v = ints(t);
            v.contains(&7) && v.contains(&-3)
        });

        assert_eq!(ints(&reduced.render()), vec![2, 1, 7, 1, -3]);

        let Value::Repeat(iters) = &reduced.values[1] else {
            panic!("expected a repeat")
        };
        let arrays: Vec<&Vec<i64>> = iters
            .iter()
            .map(|it| match &it[1] {
                Value::Array(a) => a,
                other => panic!("expected an array, got {other:?}"),
            })
            .collect();
        // 7 came from position 1, -3 from position 2 of their own arrays.
        assert_eq!(arrays[0], &vec![7]);
        assert_eq!(arrays[1], &vec![-3]);
        assert_ne!(
            arrays[0], arrays[1],
            "the two occurrences kept different positions"
        );
        assert_counts_match_own_data(&reduced.schema, &reduced.schema.items, &reduced.values, "");
    }

    /// Acceptance criterion for step 2: with nested `repeat`s, the same
    /// `AxisId` exists once per outer instance, and selecting inside one outer
    /// instance must not disturb its sibling.
    ///
    /// Both the inner block axis (`G`) and the array axis (`N`) occur twice.
    /// The predicate forces the two outer instances to keep *different*
    /// positions at both levels: outer 0 needs its second inner iteration,
    /// outer 1 needs its first.
    #[test]
    fn nested_repeats_select_independently_per_outer_instance() {
        let text = "int T in 1..5\nrepeat T {\n  int G in 1..5\n  repeat G {\n    \
                    int N in 1..5\n    array A[N] in -100..100\n  }\n}\n";
        let data = build(text, "2\n2\n1\n10\n3\n11 12 13\n2\n3\n20 21 22\n1\n23\n");

        let reduced = reduce(&data, |t| {
            let v = ints(t);
            v.contains(&12) && v.contains(&20)
        });

        assert_eq!(ints(&reduced.render()), vec![2, 1, 1, 12, 1, 1, 20]);

        // Dig out each outer instance's surviving inner array.
        let Value::Repeat(outer) = &reduced.values[1] else {
            panic!("expected the outer repeat")
        };
        assert_eq!(outer.len(), 2, "both outer instances are still needed");
        let inner_arrays: Vec<Vec<i64>> = outer
            .iter()
            .map(|instance| {
                let Value::Repeat(inner) = &instance[1] else {
                    panic!("expected the inner repeat")
                };
                assert_eq!(inner.len(), 1, "each outer kept exactly one inner");
                match &inner[0][1] {
                    Value::Array(a) => a.clone(),
                    other => panic!("expected an array, got {other:?}"),
                }
            })
            .collect();

        // Outer 0 kept inner position 1; outer 1 kept inner position 0. Sharing
        // one mask across the two occurrences cannot produce this.
        assert_eq!(inner_arrays[0], vec![12]);
        assert_eq!(inner_arrays[1], vec![20]);

        assert_counts_match_own_data(&reduced.schema, &reduced.schema.items, &reduced.values, "");
        assert_eq!(
            parse_input(&reduced.schema, &reduced.render()).unwrap(),
            reduced
        );
    }

    /// The merge rule on its own, away from any graph. Two events narrow one
    /// occurrence; the fixed point is their intersection and the order they
    /// arrive in cannot change it.
    ///
    ///     domain 1111
    ///     event  1011
    ///     event  1101
    ///     result 1001
    #[test]
    fn competing_events_converge_regardless_of_order() {
        let domain = vec![0usize, 1, 2, 3];
        let a = vec![0usize, 2, 3];
        let b = vec![0usize, 1, 3];
        let key = (vec![7usize], 0usize);

        let run = |events: &[&Vec<usize>]| -> (Vec<usize>, usize) {
            let mut state = Propagation {
                masks: BTreeMap::new(),
                updates: 0,
            };
            state.masks.insert(key.clone(), domain.clone());
            for incoming in events {
                state.narrow(key.clone(), incoming, &domain);
            }
            (state.masks[&key].clone(), state.updates)
        };

        let (forward, forward_updates) = run(&[&a, &b]);
        let (backward, backward_updates) = run(&[&b, &a]);
        assert_eq!(forward, vec![0, 3]);
        assert_eq!(
            forward, backward,
            "the fixed point depends on arrival order"
        );
        assert_eq!(forward_updates, 2);
        assert_eq!(backward_updates, 2);

        // Re-delivering a mask that changes nothing must not count, and must
        // not enqueue: that is what stops a cycle.
        let mut state = Propagation {
            masks: BTreeMap::new(),
            updates: 0,
        };
        state.masks.insert(key.clone(), domain.clone());
        assert!(state.narrow(key.clone(), &a, &domain));
        assert!(!state.narrow(key.clone(), &a, &domain));
        assert!(!state.narrow(key.clone(), &domain, &domain));
        assert_eq!(state.updates, 1);
    }

    /// Two inducers, one target. Both graphs share the vertex axis *and* the
    /// edge axis, so one vertex selection emits two induced edge masks and the
    /// fixed point is their intersection.
    #[test]
    fn two_inducers_on_one_target_intersect() {
        let text = "int N in 1..10\nint M in 0..20\ngraph E1[M] vertices N\n\
                    graph E2[M] vertices N\n";
        let data = build(text, "4 3\n1 2\n2 3\n3 4\n1 4\n1 3\n2 3\n");

        let mut all = Vec::new();
        occurrences(
            &data.schema,
            &data.schema.items,
            &data.values,
            &Vec::new(),
            &mut all,
        );
        let vertex_occ = all
            .iter()
            .find(|o| o.members.iter().any(|m| m.role == Role::GraphVertices))
            .expect("a vertex occurrence")
            .clone();
        assert_eq!(
            vertex_occ.members.len(),
            2,
            "both graphs share the vertices"
        );

        // Keeping vertices 1, 2, 3 leaves edges {0,1} of E1 and {1,2} of E2.
        let state = propagate(&data, &vertex_occ, &[0, 1, 2], &all);
        let edge_axis = state
            .masks
            .iter()
            .find(|((_, axis), _)| *axis != vertex_occ.axis)
            .map(|(_, mask)| mask.clone())
            .expect("the edge axis was narrowed");
        assert_eq!(edge_axis, vec![1], "the fixed point is the intersection");

        // Every successful narrowing strictly removes a position, so the count
        // is bounded by the positions in play. A re-enqueue cycle would blow
        // past this rather than hang.
        let total: usize = all.iter().filter_map(|o| occurrence_extent(&data, o)).sum();
        assert!(
            state.updates <= total,
            "{} updates over {total} positions",
            state.updates
        );

        let projected = project_fixpoint(&data, &state, &all).expect("projects");
        // N M, then E1's surviving edge, then E2's.
        assert_eq!(ints(&projected.render()), vec![3, 1, 2, 3, 1, 3]);
        assert_eq!(
            parse_input(&projected.schema, &projected.render()).unwrap(),
            projected
        );
    }

    /// The top-level occurrence of a named count, plus every occurrence.
    fn occurrence_for(data: &SchemaData, count: &str) -> (Occurrence, Vec<Occurrence>) {
        let axis = data
            .schema
            .sizing_axis(&Ref::Name(count.to_string()))
            .unwrap_or_else(|| panic!("no axis for {count}"));
        let mut all = Vec::new();
        occurrences(
            &data.schema,
            &data.schema.items,
            &data.values,
            &Vec::new(),
            &mut all,
        );
        let occ = all
            .iter()
            .find(|o| o.axis == axis && o.prefix.is_empty())
            .expect("a top-level occurrence")
            .clone();
        (occ, all)
    }

    /// Dropping a reference and renumbering one are different things, and both
    /// have to happen.
    ///
    /// Keeping positions 1 and 3 of `A`: the reference to old label 1 is
    /// *dropped*, because what it named is gone. The references to old labels 2
    /// and 4 are *renumbered* to 1 and 2, because what they name survived and
    /// moved. Neither is clipped, and nothing is repointed at a neighbour.
    #[test]
    fn an_index_drops_dead_references_and_renumbers_live_ones() {
        let text = "int N in 1..10\narray A[N] in 0..999\nint K in 0..10\n\
                    index I[K] into N\n";
        let data = build(text, "4\n10 20 30 40\n3\n2 4 1\n");

        let (occ, all) = occurrence_for(&data, "N");
        let projected = project_occurrence(&data, &occ, &[1, 3], &all).expect("projects");

        // N, A, K, I. Old label 2 -> 1 and old label 4 -> 2; old label 1 gone.
        assert_eq!(ints(&projected.render()), vec![2, 20, 40, 2, 1, 2]);
        assert_eq!(
            parse_input(&projected.schema, &projected.render()).unwrap(),
            projected
        );
    }

    /// Two `index` declarations sharing a length count both induce on that one
    /// occurrence, and the fixed point is their intersection -- the same merge
    /// the graph cascades go through.
    #[test]
    fn two_index_relations_on_one_target_intersect() {
        let text = "int N in 1..10\narray A[N] in 0..999\nint K in 0..10\n\
                    index I1[K] into N\nindex I2[K] into N\n";
        let data = build(text, "3\n10 20 30\n3\n1 2 3\n3 2 1\n");

        let (occ, all) = occurrence_for(&data, "N");
        // Keeping labels 1 and 2: I1 loses position 2, I2 loses position 0.
        let state = propagate(&data, &occ, &[0, 1], &all);
        let k_axis = data
            .schema
            .sizing_axis(&Ref::Name("K".into()))
            .expect("K has an axis");
        assert_eq!(
            state.masks.get(&(Vec::new(), k_axis)),
            Some(&vec![1usize]),
            "the fixed point is the intersection, not either mask alone"
        );

        let projected = project_fixpoint(&data, &state, &all).expect("projects");
        assert_eq!(ints(&projected.render()), vec![2, 10, 20, 1, 2, 2]);
    }

    /// A graph cascade and an index cascade aiming at the same occurrence. Two
    /// producers, one target, one merge.
    #[test]
    fn graph_and_index_cascades_converge_on_one_occurrence() {
        let text = "int N in 1..10\nint M in 0..20\ngraph E[M] vertices N\n\
                    index I[M] into N\n";
        let data = build(text, "4 3\n1 2\n2 3\n3 4\n4 3 1\n");

        let (occ, all) = occurrence_for(&data, "N");
        // Keeping vertices 1, 2, 3: the graph keeps edges {0,1}, the index
        // keeps positions {1,2}. Their intersection is {1}.
        let state = propagate(&data, &occ, &[0, 1, 2], &all);
        let m_axis = data
            .schema
            .sizing_axis(&Ref::Name("M".into()))
            .expect("M has an axis");
        assert_eq!(state.masks.get(&(Vec::new(), m_axis)), Some(&vec![1usize]));

        let projected = project_fixpoint(&data, &state, &all).expect("projects");
        // N M, the surviving edge, then the surviving reference renumbered.
        assert_eq!(ints(&projected.render()), vec![3, 1, 2, 3, 3]);
        assert_eq!(
            parse_input(&projected.schema, &projected.render()).unwrap(),
            projected
        );
    }

    /// Two index hops in series. This is the case that needs the worklist to
    /// run a *second* round: narrowing `N` narrows `I`'s own axis, and only
    /// once that has happened can `J`'s references into `I` be seen to dangle.
    /// Every other cascade in this suite converges in one round, so without
    /// this test the iteration itself is unexercised.
    #[test]
    fn a_two_hop_index_chain_needs_a_second_round() {
        let text = "int N in 1..10
array A[N] in 0..999
int K in 0..10
                    index I[K] into N
int L in 0..10
index J[L] into K
";
        let data = build(
            text,
            "4
10 20 30 40
3
1 3 4
2
1 2
",
        );

        let (occ, all) = occurrence_for(&data, "N");
        // Drop label 3. Round 1: `I` loses its middle entry, so `K`'s axis
        // becomes {0,2}. Round 2: `J`'s second reference named that entry, so
        // `L`'s axis becomes {0}.
        let state = propagate(&data, &occ, &[0, 1, 3], &all);
        let axis_of = |n: &str| {
            data.schema
                .sizing_axis(&Ref::Name(n.into()))
                .expect("declared count")
        };
        assert_eq!(
            state.masks.get(&(Vec::new(), axis_of("K"))),
            Some(&vec![0usize, 2])
        );
        assert_eq!(
            state.masks.get(&(Vec::new(), axis_of("L"))),
            Some(&vec![0usize])
        );

        let projected = project_fixpoint(&data, &state, &all).expect("projects");
        // N, A, K, the surviving references renumbered, L, then J.
        assert_eq!(
            ints(&projected.render()),
            vec![3, 10, 20, 40, 2, 1, 3, 1, 1]
        );
        assert_eq!(
            parse_input(&projected.schema, &projected.render()).unwrap(),
            projected
        );
    }

    /// A surviving reference whose target did not survive is a bug in the
    /// induction. Projection rejects the candidate rather than clipping it,
    /// renumbering it onto a neighbour, or emitting a dangling reference.
    #[test]
    fn a_dangling_reference_rejects_the_candidate() {
        let text = "int N in 1..10\narray A[N] in 0..999\nint K in 0..10\n\
                    index I[K] into N\n";
        let data = build(text, "3\n10 20 30\n2\n1 3\n");

        let (_, all) = occurrence_for(&data, "N");
        let n_axis = data.schema.sizing_axis(&Ref::Name("N".into())).unwrap();
        let k_axis = data.schema.sizing_axis(&Ref::Name("K".into())).unwrap();

        // Hand-built and deliberately inconsistent: only label 1 of N survives,
        // yet both references are kept -- and I[1] points at label 3.
        let mut state = Propagation {
            masks: BTreeMap::new(),
            updates: 0,
        };
        state.masks.insert((Vec::new(), n_axis), vec![0]);
        state.masks.insert((Vec::new(), k_axis), vec![0, 1]);

        assert!(
            project_fixpoint(&data, &state, &all).is_none(),
            "a dangling reference must reject, not clip"
        );
    }

    /// Two instances of one `index` declaration induce independently.
    #[test]
    fn index_induction_stays_inside_its_own_repeat_instance() {
        let text = "int T in 1..3\nrepeat T {\n  int N in 1..10\n  \
                    array A[N] in 0..999\n  int K in 0..10\n  index I[K] into N\n}\n";
        let data = build(text, "2\n3\n10 20 30\n2\n1 3\n3\n40 50 60\n2\n2 3\n");

        let mut all = Vec::new();
        occurrences(
            &data.schema,
            &data.schema.items,
            &data.values,
            &Vec::new(),
            &mut all,
        );
        let n_axis = data.schema.sizing_axis(&Ref::Name("N".into())).unwrap();
        let instances: Vec<Occurrence> = all.iter().filter(|o| o.axis == n_axis).cloned().collect();
        assert_eq!(instances.len(), 2, "one N occurrence per instance");

        // Instance 0 keeps label 1: reference 1 lives, reference 3 dies.
        let first = project_occurrence(&data, &instances[0], &[0], &all).expect("projects");
        assert_eq!(
            ints(&first.render()),
            vec![2, 1, 10, 1, 1, 3, 40, 50, 60, 2, 2, 3],
            "only instance 0 moved"
        );

        // Instance 1 keeps label 2: reference 2 lives and renumbers to 1.
        let second = project_occurrence(&data, &instances[1], &[1], &all).expect("projects");
        assert_eq!(
            ints(&second.render()),
            vec![2, 3, 10, 20, 30, 2, 1, 3, 1, 50, 1, 1],
            "only instance 1 moved"
        );
    }

    /// References are structure, not magnitude: the value pass must leave them
    /// alone. Driving one toward zero would put it out of range.
    #[test]
    fn the_value_pass_does_not_shrink_references() {
        let text = "int N in 1..10\narray A[N] in 0..999\nint K in 0..10\n\
                    index I[K] into N\n";
        let data = build(text, "3\n10 20 30\n2\n2 3\n");

        // Accept everything, so the value pass takes whatever it can get.
        let reduced = reduce(&data, |_| true);
        let text_out = reduced.render();
        assert_eq!(
            parse_input(&reduced.schema, &text_out).unwrap(),
            reduced,
            "a shrunk reference would fall outside 1..=N and fail to re-parse"
        );
        let Value::Array(refs) = &reduced.values[3] else {
            panic!("expected the index array")
        };
        let Value::Int(n) = &reduced.values[0] else {
            panic!()
        };
        for r in refs {
            assert!(*r >= 1 && r <= n, "reference {r} outside 1..={n}");
        }
    }

    /// Both relation forms flow through one seam, in a defined order, and each
    /// contributes its own positional survivors before anything is merged.
    #[test]
    fn both_relation_forms_come_out_of_one_induce_seam() {
        let text = "int N in 1..10\nint M in 0..20\ngraph E[M] vertices N\n\
                    index I[M] into N\n";
        let data = build(text, "4 3\n1 2\n2 3\n3 4\n4 3 1\n");
        let (occ, _) = occurrence_for(&data, "N");
        let m_axis = data.schema.sizing_axis(&Ref::Name("M".into())).unwrap();

        // Keeping vertices 1, 2, 3.
        let induced = induced_selections(&data.schema, &data, &occ, &[0, 1, 2]);
        assert_eq!(induced.len(), 2, "one from each form");
        // Graph first, then index -- the order the open-coded loops used.
        assert_eq!(induced[0].axis, m_axis);
        assert_eq!(
            induced[0].keep,
            vec![0, 1],
            "edges with both endpoints alive"
        );
        assert_eq!(induced[1].axis, m_axis);
        assert_eq!(induced[1].keep, vec![1, 2], "references to live vertices");

        // Neither rule merges; that is the emit path's job, and it intersects.
        let state = propagate(&data, &occ, &[0, 1, 2], &data.all_occurrences());
        assert_eq!(state.masks.get(&(Vec::new(), m_axis)), Some(&vec![1usize]));
    }

    /// The shared validate rule, on the graph side: a kept edge whose endpoint
    /// is gone rejects the candidate instead of being dropped.
    ///
    /// Dropping it would leave the edge count describing data that is no longer
    /// there -- and if that count is shared, the co-sized members would keep
    /// the longer mask. Same discipline as a dangling index reference.
    #[test]
    fn a_kept_edge_with_a_dead_endpoint_rejects_the_candidate() {
        let text = "int N in 1..10\nint M in 0..20\ngraph E[M] vertices N\n";
        let data = build(text, "3 2\n1 2\n2 3\n");
        let all = data.all_occurrences();
        let n_axis = data.schema.sizing_axis(&Ref::Name("N".into())).unwrap();
        let m_axis = data.schema.sizing_axis(&Ref::Name("M".into())).unwrap();

        // Hand-built and deliberately inconsistent: only vertex 1 survives, yet
        // edge 0 -- which needs vertex 2 -- is still kept.
        let mut state = Propagation {
            masks: BTreeMap::new(),
            updates: 0,
        };
        state.masks.insert((Vec::new(), n_axis), vec![0]);
        state.masks.insert((Vec::new(), m_axis), vec![0]);

        assert!(
            project_fixpoint(&data, &state, &all).is_none(),
            "a dangling endpoint must reject, not silently drop the edge"
        );
    }

    /// The latent case the validate rule also fixes. With a literal edge count
    /// there is no edge axis, so nothing can carry an induced selection, and a
    /// vertex selection that kills an edge would previously have emitted a
    /// graph with fewer edges than the format declares.
    #[test]
    fn a_vertex_selection_that_breaks_a_literal_edge_count_rejects() {
        let text = "int N in 2..10\ngraph E[2] vertices N\n";
        let data = build(text, "3\n1 2\n2 3\n");
        let (occ, all) = occurrence_for(&data, "N");

        // Dropping vertex 3 kills edge (2,3), but the count is fixed at 2.
        assert!(
            project_occurrence(&data, &occ, &[0, 1], &all).is_none(),
            "the edge count cannot absorb the loss, so the candidate is invalid"
        );
        // Keeping every vertex leaves both edges intact, so it still projects.
        let whole = project_occurrence(&data, &occ, &[0, 1, 2], &all).expect("projects");
        assert_eq!(ints(&whole.render()), vec![3, 1, 2, 2, 3]);
    }

    /// The section 9 litmus schema: a permutation with data on each side.
    fn litmus() -> SchemaData {
        let text = "int N in 1..10\npermutation P[N]\narray Colour[N] in 0..999\n\
                    array Weight[P.values] in 0..999\n";
        build(text, "5\n3 5 1 4 2\n11 12 13 14 15\n71 72 73 74 75\n")
    }

    fn axis_occurrence(data: &SchemaData, r: &Ref) -> (Occurrence, Vec<Occurrence>) {
        let axis = data
            .schema
            .sizing_axis(r)
            .expect("the reference has an axis");
        let mut all = Vec::new();
        occurrences(
            &data.schema,
            &data.schema.items,
            &data.values,
            &Vec::new(),
            &mut all,
        );
        let occ = all
            .iter()
            .find(|o| o.axis == axis && o.prefix.is_empty())
            .expect("a top-level occurrence")
            .clone();
        (occ, all)
    }

    /// 1. It parses, and it round-trips.
    #[test]
    fn a_permutation_parses_and_round_trips() {
        let data = litmus();
        assert_eq!(
            ints(&data.render()),
            vec![5, 3, 5, 1, 4, 2, 11, 12, 13, 14, 15, 71, 72, 73, 74, 75]
        );
        assert_eq!(parse_input(&data.schema, &data.render()).unwrap(), data);

        // Two axes on one count: same cardinality, different identity.
        let domain = data.schema.sizing_axis(&Ref::Name("N".into())).unwrap();
        let codomain = data.schema.sizing_axis(&Ref::Values("P".into())).unwrap();
        assert_ne!(domain, codomain);
        assert_eq!(
            data.schema.count_of(&Ref::Name("N".into())),
            data.schema.count_of(&Ref::Values("P".into()))
        );
    }

    /// 2, 4, 5, 6. Selecting domain positions on a nontrivial mapping. The
    /// expected outputs are section 9's hand-computed table: domain data
    /// follows positions, codomain data follows values, and the survivor is
    /// still a permutation.
    #[test]
    fn selecting_positions_induces_the_image() {
        let data = litmus();
        let (occ, all) = axis_occurrence(&data, &Ref::Name("N".into()));

        // Keep positions 1, 3, 5 (zero-based 0, 2, 4).
        let kept = project_occurrence(&data, &occ, &[0, 2, 4], &all).expect("projects");
        assert_eq!(
            ints(&kept.render()),
            vec![3, 3, 1, 2, 11, 13, 15, 71, 72, 73]
        );

        // Keep positions 2 and 4 (zero-based 1, 3).
        let kept = project_occurrence(&data, &occ, &[1, 3], &all).expect("projects");
        assert_eq!(ints(&kept.render()), vec![2, 2, 1, 12, 14, 74, 75]);
        assert_eq!(parse_input(&kept.schema, &kept.render()).unwrap(), kept);
    }

    /// 3. Selecting codomain values propagates back through the preimage.
    #[test]
    fn selecting_values_induces_the_preimage() {
        let data = litmus();
        let (occ, all) = axis_occurrence(&data, &Ref::Values("P".into()));

        // Drop value 4 -- keep values 1, 2, 3, 5 (zero-based 0, 1, 2, 4).
        let kept = project_occurrence(&data, &occ, &[0, 1, 2, 4], &all).expect("projects");
        assert_eq!(
            ints(&kept.render()),
            vec![4, 3, 4, 1, 2, 11, 12, 13, 15, 71, 72, 73, 75]
        );

        // Keep values 1 and 2 (zero-based 0, 1).
        let kept = project_occurrence(&data, &occ, &[0, 1], &all).expect("projects");
        assert_eq!(ints(&kept.render()), vec![2, 1, 2, 13, 15, 71, 72]);
        assert_eq!(parse_input(&kept.schema, &kept.render()).unwrap(), kept);
    }

    /// 2 again, through the reducer rather than a hand-built mask: whatever it
    /// accepts is still a permutation.
    #[test]
    fn every_accepted_candidate_is_still_a_permutation() {
        let data = litmus();
        let reduced = reduce(&data, |t| ints(t).contains(&74));

        let (Value::Int(n), Value::Array(mapping)) = (&reduced.values[0], &reduced.values[1])
        else {
            panic!("unexpected shape")
        };
        assert_eq!(mapping.len() as i64, *n);
        assert!(
            is_permutation(mapping),
            "{mapping:?} is not a permutation of 1..={n}"
        );
        assert_eq!(
            parse_input(&reduced.schema, &reduced.render()).unwrap(),
            reduced
        );
    }

    /// 7. Two instances of one permutation declaration inside a repeat select
    ///    independently; neither mask reaches the other.
    #[test]
    fn permutation_instances_do_not_leak_masks() {
        let text = "int T in 1..3\nrepeat T {\n  int N in 1..10\n  permutation P[N]\n}\n";
        let data = build(text, "2\n3\n2 3 1\n3\n3 1 2\n");

        let mut all = Vec::new();
        occurrences(
            &data.schema,
            &data.schema.items,
            &data.values,
            &Vec::new(),
            &mut all,
        );
        let domain = data.schema.sizing_axis(&Ref::Name("N".into())).unwrap();
        let instances: Vec<Occurrence> = all.iter().filter(|o| o.axis == domain).cloned().collect();
        assert_eq!(instances.len(), 2, "one domain occurrence per instance");

        // Instance 0 keeps only its first position; instance 1 is untouched.
        let kept = project_occurrence(&data, &instances[0], &[0], &all).expect("projects");
        assert_eq!(ints(&kept.render()), vec![2, 1, 1, 3, 3, 1, 2]);
        assert_eq!(parse_input(&kept.schema, &kept.render()).unwrap(), kept);
    }

    /// 8. A bounded integer array stays a bounded integer array. Its values are
    ///    magnitudes and the value pass clamps them; nothing infers
    ///    references.
    ///
    ///    The literal `array A[N] in 1..N` of the design note is not
    ///    expressible yet -- bounds take integers, not counts -- so this is
    ///    the static-bound analogue of the same point.
    #[test]
    fn a_bounded_array_is_not_a_permutation() {
        let text = "int N in 1..10\narray A[N] in 1..5\n";
        let data = build(text, "3\n3 1 2\n");

        // No permutation, so no codomain axis exists to reference.
        assert!(data.schema.sizing_axis(&Ref::Values("A".into())).is_none());
        assert!(parse_schema("int N in 1..10\narray A[N] in 1..5\narray B[A.values]\n").is_err());

        // The values clamp toward the declared floor rather than being treated
        // as identities.
        let reduced = reduce(&data, |t| ints(t).len() >= 2);
        let Value::Array(a) = &reduced.values[1] else {
            panic!()
        };
        assert!(
            a.iter().all(|v| *v == 1),
            "{a:?} should have clamped to the bound"
        );
    }

    /// 9. A mapping that is not a bijection is rejected when read.
    #[test]
    fn a_non_bijective_permutation_is_rejected() {
        let schema = parse_schema("int N in 1..10\npermutation P[N]\n").unwrap();
        for bad in ["3\n1 1 2\n", "3\n1 2 4\n", "3\n0 1 2\n"] {
            let err = parse_input(&schema, bad).unwrap_err();
            assert!(err.contains("not a permutation"), "{bad:?}: {err}");
        }
        parse_input(&schema, "3\n2 3 1\n").expect("a real permutation is fine");
    }

    /// 10. Graph, index and bijection all aimed at one schema, reaching a
    ///     single fixed point through the existing worklist.
    #[test]
    fn graph_index_and_bijection_reach_one_fixed_point() {
        let text = "int N in 1..10\nint M in 0..20\ngraph E[M] vertices N\n\
                    index I[M] into N\npermutation P[N]\n";
        let data = build(text, "4 2\n1 2\n3 4\n1 3\n2 1 4 3\n");
        let (occ, all) = axis_occurrence(&data, &Ref::Name("N".into()));

        // Keeping vertices 1 and 2: the graph keeps edge 0, the index keeps
        // reference 0, and the permutation's image is {1, 2}. One pass.
        let state = propagate(&data, &occ, &[0, 1], &all);
        let m_axis = data.schema.sizing_axis(&Ref::Name("M".into())).unwrap();
        let codomain = data.schema.sizing_axis(&Ref::Values("P".into())).unwrap();
        assert_eq!(state.masks.get(&(Vec::new(), m_axis)), Some(&vec![0usize]));
        assert_eq!(
            state.masks.get(&(Vec::new(), codomain)),
            Some(&vec![0usize, 1])
        );

        let kept = project_fixpoint(&data, &state, &all).expect("projects");
        assert_eq!(ints(&kept.render()), vec![2, 1, 1, 2, 1, 2, 1]);
        assert_eq!(parse_input(&kept.schema, &kept.render()).unwrap(), kept);
    }

    #[test]
    fn comments_and_loose_spacing_are_accepted() {
        let schema = parse_schema(
            "# leading comment\nint N in 1 .. 5   # trailing\narray A [ N ] in 0..9\n",
        );
        let schema = schema.expect("should parse");
        assert_eq!(schema.items.len(), 2);
        assert!(schema.is_derived("N"));
    }
}
