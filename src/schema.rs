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
// This checkpoint introduces the identifiers and their storage only. Nothing
// reads them to make a decision yet; the old `derived` set still drives every
// behaviour, and a test asserts the two descriptions agree. Keeping both paths
// alive means a benchcase that moves later can be blamed on one of them.

pub type CountId = usize;
pub type AxisId = usize;

/// An `int` that some declaration is sized by. Cardinality lives here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Count {
    pub name: String,
    pub bounds: Bounds,
    /// Every count has exactly one axis for now. Several axes per count is
    /// what makes shared dimensions possible, and is not this step.
    pub axis: AxisId,
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
    /// Names bound to a count or vertex total. These are recomputed from the
    /// data rather than shrunk on their own.
    ///
    /// Still authoritative. `counts` describes the same set and is checked
    /// against it, but does not yet drive anything.
    derived: HashSet<String>,
    counts: Vec<Count>,
    axes: Vec<Axis>,
    count_by_name: HashMap<String, CountId>,
}

impl Schema {
    pub fn is_derived(&self, name: &str) -> bool {
        self.derived.contains(name)
    }
}

// Read by the migration tests now, and by the reducer at the next checkpoint
// when declarations start resolving through axes instead of names. The allow
// is scoped to this block so it disappears with the scaffolding.
#[allow(dead_code)]
impl Schema {
    pub fn count_id(&self, name: &str) -> Option<CountId> {
        self.count_by_name.get(name).copied()
    }

    pub fn count(&self, id: CountId) -> &Count {
        &self.counts[id]
    }

    pub fn count_ids(&self) -> std::ops::Range<CountId> {
        0..self.counts.len()
    }

    /// The single axis of a count. Plural axes per count arrive with shared
    /// dimensions; until then this is total.
    pub fn default_axis(&self, id: CountId) -> AxisId {
        self.counts[id].axis
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
    for decl in items {
        match decl {
            Decl::Int { name, bounds } if derived.contains(name) => {
                let count = counts.len();
                let axis = axes.len();
                counts.push(Count {
                    name: name.clone(),
                    bounds: *bounds,
                    axis,
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

        let mut use_count = |r: &Ref, role: &str, owner: &str| -> Result<(), String> {
            let Ref::Name(n) = r else { return Ok(()) };
            if !ints_here.contains_key(n) {
                return Err(format!(
                    "`{owner}` uses `{n}` as its {role}, but `{n}` is not an `int` declared \
                     earlier in the same block"
                ));
            }
            if !derived.insert(n.clone()) {
                return Err(format!(
                    "`{n}` is used as a count by more than one declaration; that is not \
                     supported, because the two would have to shrink together"
                ));
            }
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
            Decl::Repeat { count, body } => {
                use_count(count, "repeat count", "repeat")?;
                validate(body, derived, all_names)?;
            }
        }
    }
    Ok(())
}

/// Bounds of the int a count refers to, or `None` when the count is a literal
/// (in which case the size is fixed and must not be reduced at all).
fn count_bounds(items: &[Decl], r: &Ref) -> Option<Bounds> {
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
    let mut env: HashMap<String, i64> = HashMap::new();
    let values = read_block(&schema.items, &mut cursor, &mut env)?;

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
    items: &[Decl],
    cursor: &mut Cursor,
    env: &mut HashMap<String, i64>,
) -> Result<Vec<Value>, String> {
    let mut out = Vec::with_capacity(items.len());
    for decl in items {
        out.push(read_decl(decl, cursor, env)?);
    }
    Ok(out)
}

fn resolve(r: &Ref, env: &HashMap<String, i64>, owner: &str) -> Result<usize, String> {
    let v = match r {
        Ref::Lit(v) => *v,
        Ref::Name(n) => *env
            .get(n)
            .ok_or_else(|| format!("`{owner}`: `{n}` has no value yet"))?,
    };
    usize::try_from(v).map_err(|_| format!("`{owner}`: count {v} is negative"))
}

fn read_decl(
    decl: &Decl,
    cursor: &mut Cursor,
    env: &mut HashMap<String, i64>,
) -> Result<Value, String> {
    match decl {
        Decl::Int { name, bounds } => {
            let v = cursor.take(name)?;
            check_bound(v, bounds, name)?;
            env.insert(name.clone(), v);
            Ok(Value::Int(v))
        }
        Decl::Array { name, len, bounds } => {
            let n = resolve(len, env, name)?;
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
            let r = resolve(rows, env, name)?;
            let c = resolve(cols, env, name)?;
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
            let n = resolve(verts, env, name)?;
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
            let n = resolve(verts, env, name)?;
            let m = resolve(edges, env, name)?;
            let list = read_edges(cursor, m, n, name)?;
            Ok(Value::Graph(GraphCase { n, edges: list }))
        }
        Decl::Repeat { count, body } => {
            let k = resolve(count, env, "repeat")?;
            let mut iters = Vec::with_capacity(k);
            for _ in 0..k {
                iters.push(read_block(body, cursor, env)?);
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
        let items = Rc::clone(&self.schema);
        resync_block(&items.items, &mut self.values);
    }
}

fn resync_block(items: &[Decl], values: &mut [Value]) {
    // name -> index of its Int slot in this block
    let mut slot: HashMap<&str, usize> = HashMap::new();
    for (i, decl) in items.iter().enumerate() {
        if let Decl::Int { name, .. } = decl {
            slot.insert(name.as_str(), i);
        }
    }

    let mut updates: Vec<(usize, i64)> = Vec::new();
    for (i, decl) in items.iter().enumerate() {
        let mut set = |r: &Ref, v: usize| {
            if let Ref::Name(n) = r {
                if let Some(idx) = slot.get(n.as_str()) {
                    updates.push((*idx, v as i64));
                }
            }
        };
        match (decl, &values[i]) {
            (Decl::Array { len, .. }, Value::Array(arr)) => set(len, arr.len()),
            (Decl::Matrix { rows, cols, .. }, Value::Matrix(grid)) => {
                set(rows, grid.len());
                set(cols, grid.first().map_or(0, |r| r.len()));
            }
            (Decl::Tree { verts, .. }, Value::Graph(g)) => set(verts, g.n),
            (Decl::Graph { edges, verts, .. }, Value::Graph(g)) => {
                set(edges, g.edges.len());
                set(verts, g.n);
            }
            (Decl::Repeat { count, .. }, Value::Repeat(iters)) => set(count, iters.len()),
            _ => {}
        }
    }
    for (idx, v) in updates {
        values[idx] = Value::Int(v);
    }

    // Recurse after the counts in this block are settled.
    for (i, decl) in items.iter().enumerate() {
        if let (Decl::Repeat { body, .. }, Value::Repeat(iters)) = (decl, &mut values[i]) {
            for iter in iters {
                resync_block(body, iter);
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

/// The block a path's declaration lives in, needed to resolve its count fields.
fn block_at<'a>(items: &'a [Decl], path: &[usize]) -> Option<&'a [Decl]> {
    if path.len() == 1 {
        return Some(items);
    }
    let Decl::Repeat { body, .. } = items.get(*path.first()?)? else {
        return None;
    };
    block_at(body, &path[2..])
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

    /// Delete data: array elements, matrix rows and columns, edges, vertices,
    /// and whole repeat iterations. Declared counts are recomputed after each
    /// accepted edit, and no count is taken below its declared minimum.
    pub fn structural_pass(&self, accept: &mut dyn FnMut(&SchemaData) -> bool) -> SchemaData {
        let mut data = self.clone();
        // Every accepted edit strictly shrinks the input, so this terminates;
        // the counter only bounds pathological schemas.
        for _ in 0..512 {
            let mut changed = false;
            for path in data.all_sites() {
                if shrink_site(&mut data, &path, accept) {
                    changed = true;
                    // Removing an iteration invalidates deeper paths.
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

fn shrink_site(
    data: &mut SchemaData,
    path: &[usize],
    accept: &mut dyn FnMut(&SchemaData) -> bool,
) -> bool {
    let schema = Rc::clone(&data.schema);
    let (Some(decl), Some(block)) = (decl_at(&schema.items, path), block_at(&schema.items, path))
    else {
        return false;
    };
    let Some(value) = value_at(&data.values, path).cloned() else {
        return false;
    };

    match (decl, value) {
        (Decl::Array { len, .. }, Value::Array(arr)) => {
            // A literal length is fixed by the schema and must not move.
            let Some(bounds) = count_bounds(block, len) else {
                return false;
            };
            let min = bounds.min_count();
            if arr.len() <= min {
                return false;
            }
            let reduced = ddmin_floor(&arr, min, |cand| {
                try_put(data, path, Value::Array(cand.to_vec()), accept)
            });
            let shrank = reduced.len() != arr.len();
            commit(data, path, Value::Array(reduced), shrank)
        }

        (Decl::Matrix { rows, cols, .. }, Value::Matrix(grid)) => {
            if let Some(bounds) = count_bounds(block, rows) {
                let min = bounds.min_count();
                if grid.len() > min {
                    let reduced = ddmin_floor(&grid, min, |cand| {
                        try_put(data, path, Value::Matrix(cand.to_vec()), accept)
                    });
                    if reduced.len() != grid.len() {
                        return commit(data, path, Value::Matrix(reduced), true);
                    }
                }
            }
            if let Some(bounds) = count_bounds(block, cols) {
                let min = bounds.min_count();
                let width = grid.first().map_or(0, |r| r.len());
                if width > min {
                    let columns: Vec<usize> = (0..width).collect();
                    let kept = ddmin_floor(&columns, min, |cand| {
                        try_put(
                            data,
                            path,
                            Value::Matrix(select_columns(&grid, cand)),
                            accept,
                        )
                    });
                    if kept.len() != width {
                        let narrowed = select_columns(&grid, &kept);
                        return commit(data, path, Value::Matrix(narrowed), true);
                    }
                }
            }
            false
        }

        (Decl::Graph { edges, verts, .. }, Value::Graph(graph)) => {
            if let Some(bounds) = count_bounds(block, edges) {
                let min = bounds.min_count();
                if graph.edges.len() > min {
                    let reduced = ddmin_floor(&graph.edges, min, |cand| {
                        try_put(
                            data,
                            path,
                            Value::Graph(graph.with_edges(cand.to_vec())),
                            accept,
                        )
                    });
                    if reduced.len() != graph.edges.len() {
                        let thinned = graph.with_edges(reduced);
                        return commit(data, path, Value::Graph(thinned), true);
                    }
                }
            }
            if let Some(bounds) = count_bounds(block, verts) {
                let min = bounds.min_count().max(1);
                if graph.n > min {
                    let vertices: Vec<usize> = (1..=graph.n).collect();
                    let kept = ddmin_min_len(&vertices, min, |cand| {
                        try_put(data, path, Value::Graph(graph.induced(cand)), accept)
                    });
                    if kept.len() != graph.n {
                        return commit(data, path, Value::Graph(graph.induced(&kept)), true);
                    }
                }
            }
            false
        }

        (Decl::Tree { verts, .. }, Value::Graph(tree)) => {
            let Some(bounds) = count_bounds(block, verts) else {
                return false;
            };
            let min = bounds.min_count().max(1);
            let mut current = tree.clone();
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
                let kept_leaves = ddmin_min_len(&leaves, min_leaves, |candidate| {
                    let mut kept = internal.clone();
                    kept.extend_from_slice(candidate);
                    kept.sort_unstable();
                    !kept.is_empty()
                        && try_put(data, path, Value::Graph(base.induced(&kept)), accept)
                });
                if kept_leaves.len() == leaves.len() {
                    break;
                }
                let mut kept = internal;
                kept.extend(kept_leaves);
                kept.sort_unstable();
                current = base.induced(&kept);
                changed = true;
            }
            commit(data, path, Value::Graph(current), changed)
        }

        (Decl::Repeat { count, .. }, Value::Repeat(iters)) => {
            let Some(bounds) = count_bounds(block, count) else {
                return false;
            };
            let min = bounds.min_count();
            if iters.len() <= min {
                return false;
            }
            let reduced = ddmin_floor(&iters, min, |cand| {
                try_put(data, path, Value::Repeat(cand.to_vec()), accept)
            });
            let shrank = reduced.len() != iters.len();
            commit(data, path, Value::Repeat(reduced), shrank)
        }

        _ => false,
    }
}

fn select_columns(grid: &[Vec<i64>], keep: &[usize]) -> Vec<Vec<i64>> {
    grid.iter()
        .map(|row| keep.iter().filter_map(|c| row.get(*c).copied()).collect())
        .collect()
}

/// Render a candidate edit and ask the oracle, without disturbing `data`.
fn try_put(
    data: &SchemaData,
    path: &[usize],
    value: Value,
    accept: &mut dyn FnMut(&SchemaData) -> bool,
) -> bool {
    let mut trial = data.clone();
    put(&mut trial, path, value);
    trial.resync();
    accept(&trial)
}

fn commit(data: &mut SchemaData, path: &[usize], value: Value, changed: bool) -> bool {
    if changed {
        put(data, path, value);
        data.resync();
    }
    changed
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

    #[test]
    fn a_count_cannot_be_shared_by_two_declarations() {
        // The two arrays would have to shrink in lockstep; refusing is honest.
        let err = parse_schema("int N in 1..10\narray A[N]\narray B[N]\n").unwrap_err();
        assert!(err.contains("more than one declaration"), "{err}");
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
