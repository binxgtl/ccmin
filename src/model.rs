//! A semantic model of the test input.
//!
//! This is the heart of the tool's correctness argument. Naive shrinkers treat
//! the input as raw text and delete tokens, which happily produces a file whose
//! declared `N` no longer matches the number of elements that follow. Both
//! programs then read past the end of their arrays, disagree because of
//! undefined behaviour, and the shrinker reports a "counterexample" that cannot
//! occur in real judge data.
//!
//! Instead we parse the input into a shape we understand and shrink the *shape*.
//! Every candidate is re-rendered from the model, so the length prefix is
//! correct by construction and malformed inputs are unrepresentable.
//!
//! Anything we cannot classify falls back to `Raw`, where we do token-level
//! shrinking but tell the user the result is unverified.

/// `header` holds the leading scalars (e.g. `N`, or `N K`); `header[n_idx]` is
/// the one that must equal `arr.len()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArrayCase {
    pub header: Vec<i64>,
    pub n_idx: usize,
    pub arr: Vec<i64>,
}

impl ArrayCase {
    fn render_into(&self, out: &mut String) {
        let header: Vec<String> = self.header.iter().map(|v| v.to_string()).collect();
        out.push_str(&header.join(" "));
        out.push('\n');
        let arr: Vec<String> = self.arr.iter().map(|v| v.to_string()).collect();
        out.push_str(&arr.join(" "));
        out.push('\n');
    }

    /// Keep the declared length in sync with reality. Called after every edit.
    pub fn resync(&mut self) {
        self.header[self.n_idx] = self.arr.len() as i64;
    }

    pub fn with_arr(&self, arr: Vec<i64>) -> ArrayCase {
        let mut c = self.clone();
        c.arr = arr;
        c.resync();
        c
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    pub u: usize,
    pub v: usize,
}

/// A one-based, unweighted graph. `Tree` uses the same representation with the
/// additional invariant that the graph is connected and has `n - 1` edges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphCase {
    pub n: usize,
    pub edges: Vec<Edge>,
}

impl GraphCase {
    fn render_tree_into(&self, out: &mut String) {
        out.push_str(&self.n.to_string());
        out.push('\n');
        self.render_edges_into(out);
    }

    fn render_graph_into(&self, out: &mut String) {
        out.push_str(&format!("{} {}\n", self.n, self.edges.len()));
        self.render_edges_into(out);
    }

    fn render_edges_into(&self, out: &mut String) {
        for edge in &self.edges {
            out.push_str(&format!("{} {}\n", edge.u, edge.v));
        }
    }

    pub fn with_edges(&self, edges: Vec<Edge>) -> Self {
        Self { n: self.n, edges }
    }

    /// Keep an induced subgraph and compact its vertex labels back to 1..=N.
    pub fn induced(&self, kept: &[usize]) -> Self {
        let mut remap = vec![0usize; self.n + 1];
        for (new, old) in kept.iter().copied().enumerate() {
            if old <= self.n {
                remap[old] = new + 1;
            }
        }
        let edges = self
            .edges
            .iter()
            .filter_map(|edge| {
                let u = remap[edge.u];
                let v = remap[edge.v];
                (u != 0 && v != 0).then_some(Edge { u, v })
            })
            .collect();
        Self {
            n: kept.len(),
            edges,
        }
    }

    pub fn leaves(&self) -> Vec<usize> {
        let mut degree = vec![0usize; self.n + 1];
        for edge in &self.edges {
            degree[edge.u] += 1;
            degree[edge.v] += 1;
        }
        (1..=self.n).filter(|v| degree[*v] <= 1).collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Model {
    /// `N` followed by `N` integers, optionally with extra scalars in the header.
    Array(ArrayCase),
    /// `T` followed by `T` independent array cases.
    MultiTest(Vec<ArrayCase>),
    /// `N` followed by exactly `N - 1` unweighted edges.
    Tree(GraphCase),
    /// `N M` followed by exactly `M` unweighted edges.
    Graph(GraphCase),
    /// A grammar the user declared, with constraints. See `schema.rs`.
    Schema(crate::schema::SchemaData),
    /// Unrecognised shape. Shrunk at the token level, validity not guaranteed.
    Raw(Vec<Vec<String>>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Shape {
    #[default]
    Auto,
    Array,
    MultiTest,
    Tree,
    Graph,
    Raw,
}

#[derive(Clone, Debug, Default)]
pub struct ParseOptions {
    pub shape: Shape,
    pub n_index: Option<usize>,
    /// When set, the input is read against this declared grammar and no
    /// inference happens at all.
    pub schema: Option<std::rc::Rc<crate::schema::Schema>>,
    /// In auto mode, also try two- and three-field array headers. This is
    /// deliberately opt-in because a coincidental count can corrupt a format
    /// that is not actually an array.
    pub guess_header: bool,
}

impl Model {
    pub fn render(&self) -> String {
        let mut out = String::new();
        match self {
            Model::Array(c) => c.render_into(&mut out),
            Model::MultiTest(tests) => {
                out.push_str(&tests.len().to_string());
                out.push('\n');
                for t in tests {
                    t.render_into(&mut out);
                }
            }
            Model::Tree(tree) => tree.render_tree_into(&mut out),
            Model::Graph(graph) => graph.render_graph_into(&mut out),
            Model::Schema(data) => out.push_str(&data.render()),
            Model::Raw(lines) => {
                for l in lines {
                    out.push_str(&l.join(" "));
                    out.push('\n');
                }
            }
        }
        out
    }

    /// A single number for progress reporting: how many values are in play.
    pub fn size(&self) -> usize {
        match self {
            Model::Array(c) => c.arr.len(),
            Model::MultiTest(tests) => tests.iter().map(|t| t.arr.len()).sum::<usize>(),
            Model::Tree(tree) | Model::Graph(tree) => tree.n + tree.edges.len(),
            Model::Schema(data) => data.size(),
            Model::Raw(lines) => lines.iter().map(|l| l.len()).sum(),
        }
    }

    pub fn size_unit(&self) -> &'static str {
        match self {
            Model::Array(_) | Model::MultiTest(_) => "value",
            Model::Tree(_) | Model::Graph(_) => "graph item",
            Model::Schema(_) => "value",
            Model::Raw(_) => "token",
        }
    }

    pub fn shape_name(&self) -> &'static str {
        match self {
            Model::Array(_) => "array",
            Model::MultiTest(_) => "multi-test",
            Model::Tree(_) => "tree",
            Model::Graph(_) => "graph",
            Model::Schema(_) => "schema",
            Model::Raw(_) => "raw tokens",
        }
    }

    pub fn is_raw(&self) -> bool {
        matches!(self, Model::Raw(_))
    }

    /// Mean absolute magnitude of the integers, used to report value shrinking.
    pub fn avg_magnitude(&self) -> f64 {
        let mut sum = 0f64;
        let mut n = 0usize;
        let mut acc = |vals: &[i64]| {
            for v in vals {
                sum += (*v as f64).abs();
                n += 1;
            }
        };
        match self {
            Model::Array(c) => acc(&c.arr),
            Model::MultiTest(tests) => tests.iter().for_each(|t| acc(&t.arr)),
            Model::Tree(_) | Model::Graph(_) => {}
            Model::Schema(data) => return data.avg_magnitude(),
            Model::Raw(lines) => {
                for l in lines {
                    for tok in l {
                        if let Ok(v) = tok.parse::<i64>() {
                            sum += (v as f64).abs();
                            n += 1;
                        }
                    }
                }
            }
        }
        if n == 0 {
            0.0
        } else {
            sum / n as f64
        }
    }
}

pub fn parse_with(text: &str, options: ParseOptions) -> Result<Model, String> {
    if let Some(schema) = &options.schema {
        return crate::schema::parse_input(schema, text).map(Model::Schema);
    }

    let lines: Vec<Vec<String>> = text
        .lines()
        .map(|l| l.split_whitespace().map(str::to_string).collect())
        .collect();

    let tokens: Vec<&str> = text.split_whitespace().collect();
    if options.shape == Shape::Raw {
        return Ok(Model::Raw(lines));
    }
    if tokens.is_empty() {
        return match options.shape {
            Shape::Auto => Ok(Model::Raw(lines)),
            _ => Err("input is empty and cannot match the requested shape".into()),
        };
    }

    // Every token must be an integer for the structured shapes to apply.
    let ints: Option<Vec<i64>> = tokens.iter().map(|t| t.parse::<i64>().ok()).collect();
    let Some(ints) = ints else {
        return match options.shape {
            Shape::Auto => Ok(Model::Raw(lines)),
            _ => Err(format!(
                "input contains non-integer tokens and cannot match --shape {}",
                shape_label(options.shape)
            )),
        };
    };

    match options.shape {
        Shape::Raw => unreachable!(),
        Shape::Array => {
            let n_idx = options.n_index.unwrap_or(0);
            return try_explicit_array(&ints, n_idx)
                .map(Model::Array)
                .ok_or_else(|| {
                    format!(
                        "input does not match --shape array with --n-index {n_idx}: the selected header value must equal the number of data values"
                    )
                });
        }
        Shape::MultiTest => {
            if options.n_index.is_some() {
                return Err("--n-index is currently supported only with --shape array".into());
            }
            return try_multitest(&ints).map(Model::MultiTest).ok_or_else(|| {
                "input does not match --shape multitest (expected T, then T blocks of N + N integers)"
                    .into()
            });
        }
        Shape::Tree => {
            if options.n_index.is_some() {
                return Err("--n-index is supported only with --shape array".into());
            }
            return try_tree(&ints).map(Model::Tree).ok_or_else(|| {
                "input does not match --shape tree (expected N, then N-1 valid one-based edges forming a tree)"
                    .into()
            });
        }
        Shape::Graph => {
            if options.n_index.is_some() {
                return Err("--n-index is supported only with --shape array".into());
            }
            return try_graph(&ints).map(Model::Graph).ok_or_else(|| {
                "input does not match --shape graph (expected N M, then M valid one-based edges)"
                    .into()
            });
        }
        Shape::Auto => {}
    }

    // Preferred shape: a bare `N` followed by exactly N values.
    if let Some(c) = try_array(&ints, 1, 0) {
        return Ok(Model::Array(c));
    }

    if let Some(tests) = try_multitest(&ints) {
        return Ok(Model::MultiTest(tests));
    }

    // Optionally try headers like `N K` / `N M K`, with N in any header
    // position. Conservative auto detection is the default.
    if options.guess_header {
        for h in 2..=3usize {
            for i in 0..h {
                if let Some(c) = try_array(&ints, h, i) {
                    return Ok(Model::Array(c));
                }
            }
        }
    }

    Ok(Model::Raw(lines))
}

/// When auto-detection has fallen back to raw, is there a shape the input
/// *would* match if it were asked for by name?
///
/// Detection stays conservative on purpose: guessing `graph` wrongly would
/// reduce confidently into a structure the problem does not have, which is
/// worse than reducing honestly at token level. But staying silent leaves the
/// structural reducers undiscoverable -- ccmin's own `tree` and `graph`
/// fixtures land in raw mode -- so the difference is offered to the user
/// instead of taken on their behalf.
///
/// Only shapes whose parser *validates* something are suggested. `tree`
/// checks connectivity and acyclicity, `multitest` checks that every block
/// lines up, and `graph` checks that the edge count is exact and every
/// endpoint is in range. `--guess-header` is deliberately not suggested: it
/// matches on arithmetic alone across five header positions, so almost any
/// integer soup satisfies one of them, and pointing a user at it would be
/// noise at best and a wrong reduction at worst.
pub fn suggest_shape(text: &str) -> Option<&'static str> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    let ints: Vec<i64> = tokens
        .iter()
        .map(|t| t.parse::<i64>().ok())
        .collect::<Option<_>>()?;

    if try_tree(&ints).is_some() {
        return Some("--shape tree");
    }
    if try_multitest(&ints).is_some() {
        return Some("--shape multitest");
    }
    if try_graph(&ints).is_some() {
        return Some("--shape graph");
    }
    None
}

fn shape_label(shape: Shape) -> &'static str {
    match shape {
        Shape::Auto => "auto",
        Shape::Array => "array",
        Shape::MultiTest => "multitest",
        Shape::Tree => "tree",
        Shape::Graph => "graph",
        Shape::Raw => "raw",
    }
}

fn try_explicit_array(ints: &[i64], n_idx: usize) -> Option<ArrayCase> {
    let declared = usize::try_from(*ints.get(n_idx)?).ok()?;
    let header_len = ints.len().checked_sub(declared)?;
    if header_len == 0 || n_idx >= header_len {
        return None;
    }
    Some(ArrayCase {
        header: ints[..header_len].to_vec(),
        n_idx,
        arr: ints[header_len..].to_vec(),
    })
}

fn try_array(ints: &[i64], header_len: usize, n_idx: usize) -> Option<ArrayCase> {
    if ints.len() < header_len {
        return None;
    }
    let declared = ints[n_idx];
    if declared < 0 {
        return None;
    }
    let rest = ints.len() - header_len;
    if declared as usize != rest || rest == 0 {
        return None;
    }
    Some(ArrayCase {
        header: ints[..header_len].to_vec(),
        n_idx,
        arr: ints[header_len..].to_vec(),
    })
}

fn try_multitest(ints: &[i64]) -> Option<Vec<ArrayCase>> {
    let t = *ints.first()?;
    if t <= 0 || t > 10_000 {
        return None;
    }
    let mut pos = 1usize;
    let mut tests = Vec::with_capacity(t as usize);
    for _ in 0..t {
        let n = *ints.get(pos)?;
        if n < 0 {
            return None;
        }
        pos += 1;
        let n = usize::try_from(n).ok()?;
        let end = pos.checked_add(n)?;
        if end > ints.len() {
            return None;
        }
        tests.push(ArrayCase {
            header: vec![n as i64],
            n_idx: 0,
            arr: ints[pos..end].to_vec(),
        });
        pos = end;
    }
    if pos == ints.len() {
        Some(tests)
    } else {
        None
    }
}

fn try_tree(ints: &[i64]) -> Option<GraphCase> {
    let n = positive_usize(*ints.first()?)?;
    let edge_count = n.checked_sub(1)?;
    let data_len = edge_count.checked_mul(2)?;
    if ints.len() != 1usize.checked_add(data_len)? {
        return None;
    }
    let graph = GraphCase {
        n,
        edges: parse_edges(&ints[1..], n)?,
    };
    is_tree(&graph).then_some(graph)
}

fn try_graph(ints: &[i64]) -> Option<GraphCase> {
    let n = positive_usize(*ints.first()?)?;
    let m = usize::try_from(*ints.get(1)?).ok()?;
    let data_len = m.checked_mul(2)?;
    if ints.len() != 2usize.checked_add(data_len)? {
        return None;
    }
    Some(GraphCase {
        n,
        edges: parse_edges(&ints[2..], n)?,
    })
}

fn positive_usize(value: i64) -> Option<usize> {
    let value = usize::try_from(value).ok()?;
    (value > 0).then_some(value)
}

fn parse_edges(ints: &[i64], n: usize) -> Option<Vec<Edge>> {
    let mut edges = Vec::with_capacity(ints.len() / 2);
    let mut chunks = ints.chunks_exact(2);
    for pair in &mut chunks {
        let u = positive_usize(pair[0])?;
        let v = positive_usize(pair[1])?;
        if u > n || v > n {
            return None;
        }
        edges.push(Edge { u, v });
    }
    chunks.remainder().is_empty().then_some(edges)
}

fn is_tree(graph: &GraphCase) -> bool {
    if graph.n == 0 || graph.edges.len() != graph.n - 1 {
        return false;
    }
    let mut parent: Vec<usize> = (0..=graph.n).collect();
    for edge in &graph.edges {
        let u = find_root(&mut parent, edge.u);
        let v = find_root(&mut parent, edge.v);
        if u == v {
            return false;
        }
        parent[u] = v;
    }
    true
}

fn find_root(parent: &mut [usize], mut v: usize) -> usize {
    while parent[v] != v {
        parent[v] = parent[parent[v]];
        v = parent[v];
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Model {
        parse_with(text, ParseOptions::default()).unwrap()
    }

    #[test]
    fn parses_simple_array() {
        let m = parse("3\n4 -5 2\n");
        assert_eq!(
            m,
            Model::Array(ArrayCase {
                header: vec![3],
                n_idx: 0,
                arr: vec![4, -5, 2]
            })
        );
    }

    #[test]
    fn parses_header_with_extra_scalar() {
        let m = parse_with(
            "5 2\n1 2 3 4 5\n",
            ParseOptions {
                guess_header: true,
                ..ParseOptions::default()
            },
        )
        .unwrap();
        match m {
            Model::Array(c) => {
                assert_eq!(c.header, vec![5, 2]);
                assert_eq!(c.n_idx, 0);
                assert_eq!(c.arr.len(), 5);
            }
            other => panic!("expected Array, got {other:?}"),
        }
    }

    #[test]
    fn parses_multitest() {
        let m = parse("2\n3\n1 2 3\n2\n4 5\n");
        match m {
            Model::MultiTest(tests) => {
                assert_eq!(tests.len(), 2);
                assert_eq!(tests[0].arr, vec![1, 2, 3]);
                assert_eq!(tests[1].arr, vec![4, 5]);
            }
            other => panic!("expected MultiTest, got {other:?}"),
        }
    }

    #[test]
    fn falls_back_to_raw_on_strings() {
        assert!(parse("3\nabc def\n").is_raw());
    }

    #[test]
    fn render_round_trips_and_resyncs_n() {
        let m = parse("3\n4 -5 2\n");
        let Model::Array(c) = m else { panic!() };
        let smaller = c.with_arr(vec![4, -5]);
        assert_eq!(smaller.header[0], 2);
        assert_eq!(Model::Array(smaller).render(), "2\n4 -5\n");
    }

    #[test]
    fn extended_header_guessing_is_opt_in() {
        let conservative = parse("2 99\n1 2\n");
        assert!(conservative.is_raw());

        let guessed = parse_with(
            "2 99\n1 2\n",
            ParseOptions {
                guess_header: true,
                ..ParseOptions::default()
            },
        )
        .unwrap();
        assert!(matches!(guessed, Model::Array(_)));

        // This graph-like input used to false-positive as an array because the
        // first of three guessed header values happened to equal the tail.
        assert!(parse("3 2\n1 2\n2 3\n").is_raw());
    }

    /// Auto-detection stays conservative, so the structural shapes have to be
    /// discoverable some other way. These are the inputs ccmin's own fixtures
    /// produce, which fell back to raw token shrinking with no explanation.
    #[test]
    fn a_raw_fallback_names_the_shape_that_would_have_matched() {
        // The `tree` fixture: 8 vertices, 7 edges around a star at 2.
        let tree = "8\n2 1\n2 3\n2 4\n2 5\n2 6\n2 7\n2 8\n";
        assert_eq!(suggest_shape(tree), Some("--shape tree"));

        // The `graph` fixture: `N M` then M edges.
        let graph = "7 5\n2 3\n3 4\n4 2\n5 6\n6 7\n";
        assert_eq!(suggest_shape(graph), Some("--shape graph"));

        // A multitest: T, then T blocks of N followed by N values.
        assert_eq!(
            suggest_shape("2\n3\n1 2 3\n1\n9\n"),
            Some("--shape multitest")
        );
    }

    /// It must stay quiet when there is nothing to suggest, or the hint is
    /// just noise attached to every raw reduction.
    #[test]
    fn no_shape_is_suggested_when_none_fits() {
        // Non-integer input: genuinely raw, like the `strings` fixture.
        assert_eq!(suggest_shape("abc\nde f\n"), None);
        assert_eq!(suggest_shape(""), None);
        assert_eq!(suggest_shape("   \n"), None);
        // Integers that form no shape: 9 vertices with 4 declared edges, but
        // only two pairs follow.
        assert_eq!(suggest_shape("9 4\n1 2\n3 4\n"), None);
        // An endpoint outside 1..=N is not a graph.
        assert_eq!(suggest_shape("2 1\n1 9\n"), None);
        // Numeric coincidence alone is not enough: this matches an array with
        // a three-value header, which is exactly what is not suggested.
        assert_eq!(suggest_shape("3 9\n10 20 30\n"), None);
    }

    /// A tree is checked for connectivity, so a disconnected edge set with the
    /// right count is not mistaken for one.
    #[test]
    fn a_disconnected_edge_set_is_not_suggested_as_a_tree() {
        // 4 vertices, 3 edges, but 1-2, 3-4 and 3-4 again: a cycle and a split.
        assert_eq!(suggest_shape("4\n1 2\n3 4\n3 4\n"), None);
    }

    #[test]
    fn explicit_shape_and_n_index_override_inference() {
        let parsed = parse_with(
            "99 3\n1 2 3\n",
            ParseOptions {
                schema: None,
                shape: Shape::Array,
                n_index: Some(1),
                guess_header: false,
            },
        )
        .unwrap();
        let Model::Array(case) = parsed else { panic!() };
        assert_eq!(case.header, vec![99, 3]);
        assert_eq!(case.n_idx, 1);
        assert_eq!(case.arr, vec![1, 2, 3]);
    }

    #[test]
    fn explicit_raw_never_guesses_a_shape() {
        let parsed = parse_with(
            "3\n1 2 3\n",
            ParseOptions {
                schema: None,
                shape: Shape::Raw,
                ..ParseOptions::default()
            },
        )
        .unwrap();
        assert!(parsed.is_raw());
    }

    #[test]
    fn explicit_array_reports_mismatch() {
        let result = parse_with(
            "4\n1 2 3\n",
            ParseOptions {
                schema: None,
                shape: Shape::Array,
                ..ParseOptions::default()
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn parses_and_renders_explicit_tree() {
        let parsed = parse_with(
            "4\n1 2\n2 3\n2 4\n",
            ParseOptions {
                schema: None,
                shape: Shape::Tree,
                ..ParseOptions::default()
            },
        )
        .unwrap();
        let Model::Tree(tree) = parsed else { panic!() };
        assert_eq!(tree.n, 4);
        assert_eq!(tree.edges.len(), 3);
        assert_eq!(Model::Tree(tree).render(), "4\n1 2\n2 3\n2 4\n");
    }

    #[test]
    fn tree_parser_rejects_cycles_and_disconnected_vertices() {
        let result = parse_with(
            "4\n1 2\n2 3\n3 1\n",
            ParseOptions {
                schema: None,
                shape: Shape::Tree,
                ..ParseOptions::default()
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn parses_graph_and_resyncs_m_after_edge_deletion() {
        let parsed = parse_with(
            "4 3\n1 2\n2 3\n4 4\n",
            ParseOptions {
                schema: None,
                shape: Shape::Graph,
                ..ParseOptions::default()
            },
        )
        .unwrap();
        let Model::Graph(graph) = parsed else {
            panic!()
        };
        let smaller = graph.with_edges(vec![graph.edges[0]]);
        assert_eq!(Model::Graph(smaller).render(), "4 1\n1 2\n");
    }

    #[test]
    fn induced_subgraph_compacts_vertex_labels() {
        let graph = GraphCase {
            n: 5,
            edges: vec![
                Edge { u: 1, v: 3 },
                Edge { u: 3, v: 5 },
                Edge { u: 2, v: 4 },
            ],
        };
        assert_eq!(
            graph.induced(&[1, 3, 5]),
            GraphCase {
                n: 3,
                edges: vec![Edge { u: 1, v: 2 }, Edge { u: 2, v: 3 }],
            }
        );
    }

    #[test]
    fn graph_parser_rejects_out_of_range_endpoint() {
        let result = parse_with(
            "3 1\n1 4\n",
            ParseOptions {
                schema: None,
                shape: Shape::Graph,
                ..ParseOptions::default()
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn array_length_stays_synced_for_many_candidate_sizes() {
        let case = ArrayCase {
            header: vec![99, 6],
            n_idx: 1,
            arr: vec![10, 20, 30, 40, 50, 60],
        };
        for len in 0..=case.arr.len() {
            let candidate = case.with_arr(case.arr[..len].to_vec());
            assert_eq!(candidate.header[candidate.n_idx], len as i64);
            let reparsed = parse_with(
                &Model::Array(candidate.clone()).render(),
                ParseOptions {
                    schema: None,
                    shape: Shape::Array,
                    n_index: Some(1),
                    guess_header: false,
                },
            )
            .unwrap();
            assert_eq!(reparsed, Model::Array(candidate));
        }
    }

    #[test]
    fn every_induced_graph_subset_has_compact_valid_endpoints() {
        let graph = GraphCase {
            n: 5,
            edges: vec![
                Edge { u: 1, v: 2 },
                Edge { u: 2, v: 5 },
                Edge { u: 4, v: 4 },
                Edge { u: 5, v: 3 },
            ],
        };
        for mask in 1usize..(1usize << graph.n) {
            let kept: Vec<usize> = (1..=graph.n)
                .filter(|vertex| mask & (1 << (vertex - 1)) != 0)
                .collect();
            let candidate = graph.induced(&kept);
            assert_eq!(candidate.n, kept.len());
            assert!(candidate
                .edges
                .iter()
                .all(|edge| (1..=candidate.n).contains(&edge.u)
                    && (1..=candidate.n).contains(&edge.v)));
            let reparsed = parse_with(
                &Model::Graph(candidate.clone()).render(),
                ParseOptions {
                    schema: None,
                    shape: Shape::Graph,
                    ..ParseOptions::default()
                },
            )
            .unwrap();
            assert_eq!(reparsed, Model::Graph(candidate));
        }
    }

    #[test]
    fn pruning_any_original_leaf_subset_preserves_tree_invariant() {
        let tree = GraphCase {
            n: 6,
            edges: vec![
                Edge { u: 1, v: 2 },
                Edge { u: 2, v: 3 },
                Edge { u: 3, v: 4 },
                Edge { u: 3, v: 5 },
                Edge { u: 2, v: 6 },
            ],
        };
        let leaves = tree.leaves();
        let internal: Vec<usize> = (1..=tree.n)
            .filter(|vertex| !leaves.contains(vertex))
            .collect();
        for mask in 0usize..(1usize << leaves.len()) {
            let mut kept = internal.clone();
            kept.extend(
                leaves
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| mask & (1 << index) != 0)
                    .map(|(_, vertex)| *vertex),
            );
            kept.sort_unstable();
            let candidate = tree.induced(&kept);
            assert!(is_tree(&candidate));
            assert_eq!(candidate.edges.len(), candidate.n - 1);
        }
    }
}
