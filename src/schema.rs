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
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

// ---- grammar ------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bounds {
    pub lo: Option<i64>,
    pub hi: Option<i64>,
}

impl Bounds {
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
    Repeat {
        count: Ref,
        body: Vec<Decl>,
    },
}

impl Decl {
    fn name(&self) -> Option<&str> {
        match self {
            Decl::Int { name, .. }
            | Decl::Array { name, .. }
            | Decl::Matrix { name, .. }
            | Decl::Tree { name, .. }
            | Decl::Graph { name, .. } => Some(name),
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

    let mut uses = Uses::default();
    let mut all_names = HashSet::new();
    validate(&items, &mut uses, &mut all_names)?;
    let derived = uses.derived;

    let mut counts = Vec::new();
    let mut axes = Vec::new();
    let mut count_by_name = HashMap::new();
    build_arenas(&items, &derived, &mut counts, &mut axes, &mut count_by_name);

    Ok(Rc::new(Schema {
        items,
        derived,
        counts,
        axes,
        count_by_name,
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
        items.push(parse_decl(*no, tokens, lines, cursor)?);
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
) -> Result<Decl, String> {
    let at = |msg: String| format!("line {no}: {msg}");
    match tokens[0].as_str() {
        "int" => {
            if tokens.len() < 2 {
                return Err(at("`int` needs a name".into()));
            }
            let name = ident(no, &tokens[1])?;
            let bounds = parse_bounds(no, &tokens[2..])?;
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
            let bounds = parse_bounds(no, rest)?;
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
            let bounds = parse_bounds(no, rest)?;
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
            "unknown declaration `{other}` (expected int, array, matrix, tree, graph or repeat)"
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

fn parse_bounds(no: usize, tokens: &[String]) -> Result<Bounds, String> {
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
    let lo = parse_bound_side(no, lo_text)?;
    let hi = parse_bound_side(no, hi_text)?;
    if let (Some(lo), Some(hi)) = (lo, hi) {
        if lo > hi {
            return Err(format!("line {no}: range `{spec}` is empty ({lo} > {hi})"));
        }
    }
    Ok(Bounds { lo, hi })
}

fn parse_bound_side(no: usize, text: &str) -> Result<Option<i64>, String> {
    if text.is_empty() {
        return Ok(None);
    }
    text.parse::<i64>()
        .map(Some)
        .map_err(|_| format!("line {no}: `{text}` is not an integer"))
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
    if let Ok(v) = token.parse::<i64>() {
        if v < 0 {
            return Err(format!("line {no}: count `{v}` cannot be negative"));
        }
        return Ok(Ref::Lit(v));
    }
    ident(no, token).map(Ref::Name)
}

// ---- validation ----------------------------------------------------------

/// What each name is used for while validating. Sharing a count is legal;
/// sharing one that *cascades* into another axis is not, yet.
#[derive(Default)]
struct Uses {
    derived: HashSet<String>,
    cascading: HashSet<String>,
}

fn validate(
    items: &[Decl],
    uses: &mut Uses,
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

        let mut use_count =
            |r: &Ref, role: &str, owner: &str, cascades: bool| -> Result<(), String> {
                let Ref::Name(n) = r else { return Ok(()) };
                if !ints_here.contains_key(n) {
                    return Err(format!(
                        "`{owner}` uses `{n}` as its {role}, but `{n}` is not an `int` declared \
                         earlier in the same block"
                    ));
                }
                let shared = !uses.derived.insert(n.clone());
                if cascades {
                    uses.cascading.insert(n.clone());
                }
                // Sharing is what lets `array A[N]` and `array B[N]` coexist:
                // one axis, one selection, both projected together. A graph's
                // vertex selection additionally induces one on its edge axis,
                // which is handled. A tree's is not: pruning runs a sequence of
                // selections against a changing leaf set, and fanning that out
                // to co-sized members is a separate job.
                if shared && uses.cascading.contains(n) {
                    return Err(format!(
                        "`{n}` is a tree's vertex count and also sizes something else; tree \
                         pruning is a sequence of selections against a changing leaf set, and \
                         fanning that out is not implemented yet"
                    ));
                }
                Ok(())
            };

        match decl {
            Decl::Int { name, bounds } => {
                ints_here.insert(name.clone(), *bounds);
            }
            Decl::Array { name, len, .. } => use_count(len, "length", name, false)?,
            Decl::Matrix {
                name, rows, cols, ..
            } => {
                use_count(rows, "row count", name, false)?;
                use_count(cols, "column count", name, false)?;
            }
            Decl::Tree { name, verts } => use_count(verts, "vertex count", name, true)?,
            Decl::Graph { name, edges, verts } => {
                use_count(edges, "edge count", name, false)?;
                // A graph vertex selection induces one on the edge axis, so
                // sharing it is fine.
                use_count(verts, "vertex count", name, false)?;
            }
            Decl::Repeat { count, body } => {
                use_count(count, "repeat count", "repeat", false)?;
                validate(body, uses, all_names)?;
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
        out.push(read_decl(schema, decl, cursor, current)?);
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
    };
    usize::try_from(v).map_err(|_| format!("`{owner}`: count {v} is negative"))
}

fn read_decl(
    schema: &Schema,
    decl: &Decl,
    cursor: &mut Cursor,
    current: &mut [i64],
) -> Result<Value, String> {
    match decl {
        Decl::Int { name, bounds } => {
            let v = cursor.take(name)?;
            check_bound(v, bounds, name)?;
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
                check_bound(v, bounds, name)?;
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
                    check_bound(v, bounds, name)?;
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

fn check_bound(v: i64, bounds: &Bounds, name: &str) -> Result<(), String> {
    if bounds.contains(v) {
        Ok(())
    } else {
        Err(format!(
            "`{name}`: value {v} is outside the declared range {}",
            bounds.describe()
        ))
    }
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
            (Decl::Repeat { count, .. }, Value::Repeat(iters)) => sizes.push((count, iters.len())),
            _ => {}
        }
    }

    // A shared count is written once per member. They must already agree,
    // because a shared dimension is projected by a single mask; disagreement
    // would be the silent last-write-wins that made v0.4 reject sharing.
    #[cfg(debug_assertions)]
    {
        let mut seen: HashMap<&str, usize> = HashMap::new();
        for (r, size) in &sizes {
            if let Ref::Name(n) = r {
                if let Some(previous) = seen.insert(n.as_str(), *size) {
                    debug_assert_eq!(
                        previous, *size,
                        "members sharing count {n} disagree: {previous} vs {size}"
                    );
                }
            }
        }
    }

    for (r, size) in sizes {
        let Ref::Name(n) = r else { continue };
        let Some(id) = schema.count_id(n) else {
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
        let mut data = self.clone();
        let schema = Rc::clone(&data.schema);
        for path in data.all_sites() {
            let Some(decl) = decl_at(&schema.items, &path) else {
                continue;
            };
            let Some(value) = value_at(&data.values, &path).cloned() else {
                continue;
            };
            match (decl, value) {
                (Decl::Int { name, bounds }, Value::Int(v)) => {
                    if schema.is_derived(name) {
                        continue;
                    }
                    let reduced = shrink_value_toward(v, bounds.target(), |cand| {
                        let mut trial = data.clone();
                        put(&mut trial, &path, Value::Int(cand));
                        accept(&trial)
                    });
                    if reduced != v {
                        put(&mut data, &path, Value::Int(reduced));
                    }
                }
                (Decl::Array { bounds, .. }, Value::Array(arr)) => {
                    let reduced = shrink_ints_toward(&arr, bounds.target(), |cand| {
                        let mut trial = data.clone();
                        put(&mut trial, &path, Value::Array(cand.to_vec()));
                        accept(&trial)
                    });
                    if reduced != arr {
                        put(&mut data, &path, Value::Array(reduced));
                    }
                }
                (Decl::Matrix { bounds, .. }, Value::Matrix(grid)) => {
                    let mut next = grid.clone();
                    for r in 0..next.len() {
                        let row = next[r].clone();
                        let reduced = shrink_ints_toward(&row, bounds.target(), |cand| {
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
        Decl::Repeat { count, .. } => vec![(count, Role::Iterations)],
    }
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
        _ => return None,
    })
}

/// Rebuild one member from the surviving positions.
fn project_member(value: &Value, role: Role, keep: &[usize]) -> Option<Value> {
    Some(match (value, role) {
        (Value::Array(a), Role::Elements) => {
            Value::Array(keep.iter().filter_map(|&i| a.get(i).copied()).collect())
        }
        (Value::Matrix(g), Role::Rows) => {
            Value::Matrix(keep.iter().filter_map(|&i| g.get(i).cloned()).collect())
        }
        (Value::Matrix(g), Role::Cols) => Value::Matrix(select_columns(g, keep)),
        (Value::Graph(g), Role::Edges) => Value::Graph(
            g.with_edges(
                keep.iter()
                    .filter_map(|&i| g.edges.get(i).copied())
                    .collect(),
            ),
        ),
        // Selecting vertices also drops every edge with a removed endpoint and
        // relabels the survivors. That is a cascade into the edge-count axis,
        // which is why a vertex count may not yet be shared (see `validate`).
        (Value::Graph(g), Role::GraphVertices) => {
            let labels: Vec<usize> = keep.iter().map(|&i| i + 1).collect();
            Value::Graph(g.induced(&labels))
        }
        (Value::Repeat(iters), Role::Iterations) => {
            Value::Repeat(keep.iter().filter_map(|&i| iters.get(i).cloned()).collect())
        }
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

/// Build the candidate in which this occurrence keeps only `keep`.
///
/// Every member is projected with the *same* mask, which is what makes a shared
/// dimension stay consistent: the declarations cannot disagree because they are
/// never asked separately.
fn project_occurrence(
    data: &SchemaData,
    occ: &Occurrence,
    keep: &[usize],
    siblings: &[Occurrence],
) -> Option<SchemaData> {
    let schema = Rc::clone(&data.schema);
    let mut trial = data.clone();
    let mut written: Vec<Path> = Vec::new();
    // Selections this projection induces on *other* occurrences in the same
    // block instantiation. The first concrete instance of
    //     selection on occurrence A  ->  induced selection on occurrence B
    let mut induced: Vec<(AxisId, Vec<usize>)> = Vec::new();

    for member in &occ.members {
        let path = occ.path_of(member);
        let current = value_at(&trial.values, &path)?.clone();

        if member.role == Role::GraphVertices {
            if let (Value::Graph(graph), Some(Decl::Graph { edges, .. })) =
                (&current, decl_at(&schema.items, &path))
            {
                if let Some(edge_axis) = schema.sizing_axis(edges) {
                    induced.push((edge_axis, induced_edge_keep(graph, keep)));
                }
            }
        }

        let projected = project_member(&current, member.role, keep)?;
        put(&mut trial, &path, projected);
        written.push(path);
    }

    // Induction happens once, at the occurrence, and fans out to every member.
    // No declaration recomputes a mask of its own.
    for (axis, edge_keep) in induced {
        let Some(target) = siblings.iter().find(|o| o.axis == axis) else {
            continue;
        };
        debug_assert_eq!(
            target.prefix, occ.prefix,
            "induction escaped its block instantiation"
        );
        for member in &target.members {
            let path = target.path_of(member);
            if written.contains(&path) {
                // The graph's own edge list was already projected alongside its
                // vertices, because one `Value::Graph` holds both.
                continue;
            }
            let current = value_at(&trial.values, &path)?.clone();
            let projected = project_member(&current, member.role, &edge_keep)?;
            put(&mut trial, &path, projected);
            written.push(path);
        }
    }

    // Downstream bookkeeping only: resync observes members that are already
    // consistent, it does not decide which positions survived.
    trial.resync();
    Some(trial)
}

fn shrink_occurrence(
    data: &mut SchemaData,
    occ: &Occurrence,
    accept: &mut dyn FnMut(&SchemaData) -> bool,
) -> bool {
    let schema = Rc::clone(&data.schema);
    let bounds = schema.axis_bounds(occ.axis);
    // Occurrences an induced selection may reach: the same block instantiation.
    let siblings: Vec<Occurrence> = data
        .all_occurrences()
        .into_iter()
        .filter(|o| o.prefix == occ.prefix)
        .collect();

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
    let mut test = |candidate: &[usize]| match project_occurrence(data, occ, candidate, &siblings) {
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
    match project_occurrence(data, occ, &kept, &siblings) {
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

fn prune_tree(
    data: &mut SchemaData,
    occ: &Occurrence,
    min: usize,
    accept: &mut dyn FnMut(&SchemaData) -> bool,
) -> bool {
    let Some(member) = occ.members.first() else {
        return false;
    };
    let path = occ.path_of(member);
    let Some(Value::Graph(tree)) = value_at(&data.values, &path).cloned() else {
        return false;
    };

    let mut current = tree;
    let mut changed = false;
    loop {
        let leaves = current.leaves();
        if leaves.len() < 2 || current.n <= min {
            break;
        }
        let mut is_leaf = vec![false; current.n + 1];
        for leaf in &leaves {
            is_leaf[*leaf] = true;
        }
        let internal: Vec<usize> = (1..=current.n).filter(|v| !is_leaf[*v]).collect();
        let base = current.clone();
        // Keep enough leaves to satisfy the declared vertex floor.
        let min_leaves = min.saturating_sub(internal.len());
        let build = |keep: &[usize]| {
            let mut kept = internal.clone();
            kept.extend(keep.iter().filter_map(|&i| leaves.get(i).copied()));
            kept.sort_unstable();
            base.induced(&kept)
        };

        let positions: Vec<usize> = (0..leaves.len()).collect();
        let kept_leaves = ddmin_min_len(&positions, min_leaves, |candidate| {
            let mut trial = data.clone();
            put(&mut trial, &path, Value::Graph(build(candidate)));
            trial.resync();
            accept(&trial)
        });
        if kept_leaves.len() == leaves.len() {
            break;
        }
        current = build(&kept_leaves);
        changed = true;
    }

    if changed {
        put(data, &path, Value::Graph(current));
        data.resync();
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

    /// A graph's vertex count may now be shared, because the vertex selection
    /// induces the edge one. A tree's may not: pruning is a sequence of
    /// selections against a changing leaf set.
    #[test]
    fn graph_vertex_counts_may_be_shared_but_tree_ones_may_not() {
        parse_schema("int N in 1..10\nint M in 0..20\ngraph E[M] vertices N\narray D[N] in 0..9\n")
            .expect("a graph vertex count is shareable");

        let err =
            parse_schema("int N in 2..10\ntree E vertices N\narray D[N] in 0..9\n").unwrap_err();
        assert!(err.contains("not implemented yet"), "{err}");
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
