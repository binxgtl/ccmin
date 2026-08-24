//! ccmin -- find a failing test, then shrink it until it is small enough to read.

mod demo;
mod model;
mod oracle;
mod proc;
mod shrink;
mod term;
mod toolchain;

use model::{Model, ParseOptions, Shape};
use oracle::{CheckerConfig, FailKind, Oracle};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Args {
    sol: PathBuf,
    brute: PathBuf,
    gen: PathBuf,
    iters: u32,
    timeout: Duration,
    out: PathBuf,
    save: bool,
    demo: bool,
    shape: Shape,
    n_index: Option<usize>,
    guess_header: bool,
    cpp_standard: String,
    cxxflags: Vec<String>,
    compare_mode: proc::CompareMode,
    compare_explicit: bool,
    checker: Option<PathBuf>,
    checker_args: Vec<String>,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            sol: "sol.cpp".into(),
            brute: "brute.cpp".into(),
            gen: "gen.cpp".into(),
            iters: 200,
            timeout: Duration::from_millis(3000),
            out: "minimal.in".into(),
            save: true,
            demo: false,
            shape: Shape::Auto,
            n_index: None,
            guess_header: false,
            cpp_standard: "gnu++20".into(),
            cxxflags: Vec::new(),
            compare_mode: proc::CompareMode::Exact,
            compare_explicit: false,
            checker: None,
            checker_args: Vec::new(),
        }
    }
}

fn main() {
    // Initialise before argument parsing so even usage errors respect TTY and
    // --no-color. `run` reuses this state.
    term::init(std::env::args_os().any(|arg| arg == "--no-color"));
    match run() {
        Ok(found_bug) => std::process::exit(if found_bug { 1 } else { 0 }),
        Err(e) => {
            eprintln!("{} {e}", term::red("error:"));
            std::process::exit(2);
        }
    }
}

fn run() -> Result<bool, String> {
    let args = parse_args()?;

    // Build artifacts go to a temp directory: ccmin never writes to your
    // working tree except for the final reduced input.
    let work_dir = TempWorkDir::new()?;
    let work = work_dir.path();

    let (sol_src, brute_src, gen_src) = if args.demo {
        println!(
            "{}",
            term::dim("running the built-in demo (buggy Kadane vs O(n^2) reference)")
        );
        let s = work.join("sol.cpp");
        let b = work.join("brute.cpp");
        let g = work.join("gen.cpp");
        write_file(&s, demo::SOL)?;
        write_file(&b, demo::BRUTE)?;
        write_file(&g, demo::GEN)?;
        (s, b, g)
    } else {
        for (p, what) in [
            (&args.sol, "solution"),
            (&args.brute, "brute force"),
            (&args.gen, "generator"),
        ] {
            if !p.exists() {
                return Err(format!(
                    "{what} not found: {}\n\nExpected sol.cpp, brute.cpp and gen.cpp in the \
                     current directory.\nPass explicit paths with -s/-b/-g, or try `ccmin --demo`.",
                    p.display()
                ));
            }
        }
        (args.sol.clone(), args.brute.clone(), args.gen.clone())
    };

    // --- 1. compile -------------------------------------------------------
    let cc = toolchain::detect()?;
    let t0 = Instant::now();
    print!(
        "{} compiling with {}... ",
        term::dim("[1/3]"),
        term::cyan(&cc.label())
    );
    flush();

    let compile_options = toolchain::CompileOptions {
        standard: args.cpp_standard.clone(),
        flags: args.cxxflags.clone(),
    };
    let exes = cc.compile_all(
        &[
            ("sol".into(), sol_src),
            ("brute".into(), brute_src),
            ("gen".into(), gen_src),
        ],
        work,
        &compile_options,
    )?;
    let (sol, brute, gen) = (exes[0].clone(), exes[1].clone(), exes[2].clone());
    println!(
        "{}",
        term::green(&format!("done ({}ms)", t0.elapsed().as_millis()))
    );

    let checker = args.checker.clone().map(|program| CheckerConfig {
        program,
        args: args.checker_args.clone(),
        scratch_dir: work.to_path_buf(),
    });
    let mut oracle = Oracle::new(sol, brute, args.timeout, args.compare_mode, checker);

    // --- 2. stress --------------------------------------------------------
    println!(
        "{} stress testing (up to {} cases)...",
        term::dim("[2/3]"),
        args.iters
    );
    let t1 = Instant::now();
    let mut found: Option<(String, oracle::Failure, u32)> = None;

    for seed in 1..=args.iters {
        let input = generate(&gen, seed, args.timeout)?;
        if let Some(f) = oracle
            .judge(&input)
            .map_err(|e| format!("running programs: {e}"))?
        {
            found = Some((input, f, seed));
            break;
        }
        if seed % 10 == 0 && term::interactive() {
            print!("\r      {} cases passed", seed);
            flush();
        }
    }
    print!("{}", term::clear_line());

    let Some((input, failure, seed)) = found else {
        println!(
            "      {} in {} cases ({:.1}s)",
            term::green("no counterexample found"),
            args.iters,
            t1.elapsed().as_secs_f64()
        );
        println!(
            "\n{}",
            term::dim("Try more cases with -n, or widen the generator's range.")
        );
        return Ok(false);
    };

    let parsed = model::parse_with(
        &input,
        ParseOptions {
            shape: args.shape,
            n_index: args.n_index,
            guess_header: args.guess_header,
        },
    )?;
    println!(
        "      {} on test #{seed} -- {}",
        term::red("failed"),
        term::bold(failure.kind.describe())
    );
    println!(
        "      initial counterexample: {}, shape: {}",
        term::bold(&plural(parsed.size(), parsed.size_unit())),
        parsed.shape_name()
    );

    if parsed.is_raw() {
        let reason = if args.shape == Shape::Raw {
            "raw input shape selected; shrinking at token level (result may not satisfy the problem's constraints)"
        } else {
            "input shape not recognised; shrinking at token level (result may not satisfy the problem's constraints)"
        };
        println!("      {}", term::yellow(&format!("note: {reason}")));
    } else if args.shape == Shape::Auto
        && args.guess_header
        && matches!(&parsed, Model::Array(c) if c.header.len() > 1)
    {
        println!(
            "      {}",
            term::yellow(
                "note: array shape was inferred from an extended header; use --shape array to confirm it or omit --guess-header to keep auto detection conservative"
            )
        );
    }

    // --- 3. shrink --------------------------------------------------------
    if failure.kind == FailKind::BothFailed {
        println!(
            "\n{} both programs failed on the generated input; the brute force must exit successfully before crash reduction is safe. Shrinking was skipped.",
            term::yellow("warning:")
        );
        report(&input, &failure, &args, false)?;
        return Ok(true);
    }

    if !oracle
        .is_stable(&input, failure.kind, 3)
        .map_err(|e| format!("rechecking counterexample: {e}"))?
    {
        println!(
            "\n{} the failure is not reproducible across runs.\n{}",
            term::yellow("warning:"),
            term::dim(
                "  Your solution is likely nondeterministic (uninitialised memory, \
                 unordered_map order,\n  or reading uninitialised values). Shrinking would \
                 chase ghosts, so it was skipped."
            )
        );
        report(&input, &failure, &args, false)?;
        return Ok(true);
    }

    println!("{} shrinking...", term::dim("[3/3]"));
    let t2 = Instant::now();
    let before_size = parsed.size();
    let before_mag = parsed.avg_magnitude();

    let mut chain: Vec<usize> = vec![before_size];
    let reduced = {
        let mut on_step = |m: &Model| {
            let s = m.size();
            if chain.last() != Some(&s) {
                chain.push(s);
            }
            if term::interactive() {
                print!("\r      {}", plural(s, m.size_unit()));
                let _ = std::io::stdout().flush();
            }
        };
        let mut sh = shrink::Shrinker::new(&mut oracle, failure.kind, &mut on_step);
        sh.run(&parsed)
            .map_err(|e| format!("shrinking counterexample: {e}"))?
    };
    print!("{}", term::clear_line());

    let text = reduced.render();
    if !oracle
        .is_stable(&text, failure.kind, 3)
        .map_err(|e| format!("rechecking reduced counterexample: {e}"))?
    {
        println!(
            "\n{} the reduced input did not reproduce the same failure three times; the original stable counterexample will be reported instead.",
            term::yellow("warning:")
        );
        report(&input, &failure, &args, false)?;
        return Ok(true);
    }
    let final_failure = oracle
        .judge(&text)
        .map_err(|e| format!("verifying reduced case: {e}"))?
        .ok_or_else(|| "internal error: reduced case no longer fails".to_string())?;
    if final_failure.kind != failure.kind {
        return Err(format!(
            "internal error: reduced case changed failure kind from {} to {}",
            failure.kind.describe(),
            final_failure.kind.describe()
        ));
    }

    println!("      size   {}", arrow_chain(&chain));
    let after_mag = reduced.avg_magnitude();
    if before_mag > after_mag {
        println!(
            "      values {}",
            term::dim(&format!(
                "avg magnitude {} -> {}",
                fmt_mag(before_mag),
                fmt_mag(after_mag)
            ))
        );
    }
    println!(
        "      {} in {:.0}ms ({} total program runs)",
        term::green("shrunk"),
        t2.elapsed().as_secs_f64() * 1000.0,
        oracle.program_runs
    );

    report(&text, &final_failure, &args, true)?;
    Ok(true)
}

fn report(
    input: &str,
    failure: &oracle::Failure,
    args: &Args,
    reduced: bool,
) -> Result<(), String> {
    println!("\n{}", term::rule());
    println!(
        "{}",
        term::bold(if reduced {
            "REDUCED FAILING INPUT"
        } else {
            "FAILING INPUT"
        })
    );
    println!("{}", input.trim_end());
    println!("{}", term::rule());

    match failure.kind {
        FailKind::WrongAnswer => {
            println!(
                "{} {}",
                term::green("expected (brute):"),
                failure.brute_output.trim_end()
            );
            println!(
                "{} {}",
                term::red("actual   (sol):  "),
                failure.sol_output.trim_end()
            );
            if !failure.note.is_empty() {
                println!("{} {}", term::dim("checker:"), failure.note);
            }
        }
        _ => {
            println!("{} {}", term::red("failure:"), failure.kind.describe());
            if !failure.note.is_empty() {
                println!("{} {}", term::dim("detail: "), failure.note);
            }
        }
    }
    println!("{}", term::rule());

    if args.save {
        std::fs::write(&args.out, input)
            .map_err(|e| format!("cannot write {}: {e}", args.out.display()))?;
        println!("{}", term::dim(&format!("wrote {}", args.out.display())));
    }
    Ok(())
}

fn generate(gen: &Path, seed: u32, timeout: Duration) -> Result<String, String> {
    let out = proc::run(gen, &[seed.to_string()], "", timeout)
        .map_err(|e| format!("running generator: {e}"))?;
    if out.output_limited {
        return Err(format!(
            "generator exceeded the {} MiB output limit on seed {seed}",
            proc::OUTPUT_LIMIT_BYTES / (1024 * 1024)
        ));
    }
    if out.timed_out {
        return Err(format!("generator timed out on seed {seed}"));
    }
    if !out.exited_cleanly() {
        return Err(format!(
            "generator exited with {:?} on seed {seed}\n{}",
            out.code,
            out.stderr.trim()
        ));
    }
    if out.stdout.trim().is_empty() {
        return Err(format!("generator produced no output on seed {seed}"));
    }
    Ok(out.stdout)
}

fn arrow_chain(chain: &[usize]) -> String {
    // Keep the display readable when there were many steps.
    let shown: Vec<String> = if chain.len() <= 8 {
        chain.iter().map(|s| s.to_string()).collect()
    } else {
        let mut v: Vec<String> = chain.iter().take(4).map(|s| s.to_string()).collect();
        v.push("...".into());
        v.extend(chain.iter().rev().take(3).rev().map(|s| s.to_string()));
        v
    };
    shown.join(&term::dim(" -> "))
}

fn fmt_mag(v: f64) -> String {
    if v >= 1000.0 {
        let s = format!("{:.0}", v);
        // Thousands separators, because 1000000000 is unreadable.
        let mut out = String::new();
        for (i, c) in s.chars().enumerate() {
            if i > 0 && (s.len() - i) % 3 == 0 {
                out.push(',');
            }
            out.push(c);
        }
        out
    } else {
        format!("{:.0}", v)
    }
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

fn write_file(p: &Path, contents: &str) -> Result<(), String> {
    std::fs::write(p, contents).map_err(|e| format!("cannot write {}: {e}", p.display()))
}

fn flush() {
    let _ = std::io::stdout().flush();
}

struct TempWorkDir {
    path: PathBuf,
}

impl TempWorkDir {
    fn new() -> Result<Self, String> {
        let base = std::env::temp_dir();
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for attempt in 0..100u8 {
            let path = base.join(format!("ccmin-{}-{nonce}-{attempt}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("cannot create {}: {e}", path.display())),
            }
        }
        Err("cannot create a unique temporary working directory".into())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempWorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args::default();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        let next = |i: &mut usize, what: &str| -> Result<String, String> {
            *i += 1;
            argv.get(*i)
                .cloned()
                .ok_or_else(|| format!("{what} needs a value"))
        };
        match arg {
            "-h" | "--help" => {
                println!("{}", help());
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("ccmin {VERSION}");
                std::process::exit(0);
            }
            "--demo" => a.demo = true,
            "--no-color" => {}
            "--no-save" => a.save = false,
            // Kept as a backwards-compatible no-op: conservative auto mode is
            // now the default.
            "--strict" => a.guess_header = false,
            "--guess-header" => a.guess_header = true,
            "--std" => a.cpp_standard = next(&mut i, "--std")?,
            "--cxxflag" => a.cxxflags.push(next(&mut i, "--cxxflag")?),
            "--compare" => {
                let value = next(&mut i, "--compare")?;
                a.compare_explicit = true;
                a.compare_mode = match value.as_str() {
                    "exact" => proc::CompareMode::Exact,
                    "tokens" => proc::CompareMode::Tokens,
                    _ => {
                        return Err(format!(
                            "unknown comparison mode `{value}` (expected exact or tokens)"
                        ))
                    }
                };
            }
            "--checker" => a.checker = Some(next(&mut i, "--checker")?.into()),
            "--checker-arg" => a.checker_args.push(next(&mut i, "--checker-arg")?),
            "--shape" => {
                let value = next(&mut i, "--shape")?;
                a.shape = match value.as_str() {
                    "auto" => Shape::Auto,
                    "array" => Shape::Array,
                    "multitest" | "multi-test" => Shape::MultiTest,
                    "tree" => Shape::Tree,
                    "graph" => Shape::Graph,
                    "raw" => Shape::Raw,
                    _ => {
                        return Err(format!(
                            "unknown shape `{value}` (expected auto, array, multitest, tree, graph, or raw)"
                        ))
                    }
                };
            }
            "--n-index" => {
                a.n_index = Some(
                    next(&mut i, "--n-index")?
                        .parse()
                        .map_err(|_| "--n-index wants a zero-based integer")?,
                );
            }
            "-s" | "--sol" => a.sol = next(&mut i, "--sol")?.into(),
            "-b" | "--brute" => a.brute = next(&mut i, "--brute")?.into(),
            "-g" | "--gen" => a.gen = next(&mut i, "--gen")?.into(),
            "-o" | "--out" => a.out = next(&mut i, "--out")?.into(),
            "-n" | "--iters" => {
                a.iters = next(&mut i, "--iters")?
                    .parse()
                    .map_err(|_| "--iters wants a number")?
            }
            "-t" | "--timeout" => {
                let ms: u64 = next(&mut i, "--timeout")?
                    .parse()
                    .map_err(|_| "--timeout wants milliseconds")?;
                a.timeout = Duration::from_millis(ms);
            }
            other => return Err(format!("unknown option `{other}` (try --help)")),
        }
        i += 1;
    }
    let cli_cxxflags = std::mem::take(&mut a.cxxflags);
    if let Ok(flags) = std::env::var("CXXFLAGS") {
        a.cxxflags.extend(toolchain::split_flags(&flags)?);
    }
    a.cxxflags.extend(cli_cxxflags);
    if a.n_index.is_some() && a.shape != Shape::Array {
        return Err("--n-index requires --shape array".into());
    }
    if a.checker.is_none() && !a.checker_args.is_empty() {
        return Err("--checker-arg requires --checker".into());
    }
    if a.checker.is_some() && a.compare_explicit {
        return Err("--checker and --compare are mutually exclusive".into());
    }
    Ok(a)
}

fn help() -> String {
    format!(
        "ccmin {VERSION}
Find a failing test for your solution, then shrink it to a small input that
still fails.

USAGE:
    ccmin [OPTIONS]

    With no arguments, uses sol.cpp, brute.cpp and gen.cpp from the current
    directory. The generator receives a seed as argv[1].

OPTIONS:
    -s, --sol <FILE>       solution to test        [default: sol.cpp]
    -b, --brute <FILE>     reference solution      [default: brute.cpp]
    -g, --gen <FILE>       test generator          [default: gen.cpp]
    -n, --iters <N>        stress cases to try     [default: 200]
    -t, --timeout <MS>     per-run timeout         [default: 3000]
    -o, --out <FILE>       where to save the case  [default: minimal.in]
        --shape <SHAPE>    auto, array, multitest, tree, graph, or raw
                          [default: auto]
        --n-index <INDEX>  length field in an array header (zero-based)
        --guess-header     heuristically detect 2-3 field array headers
        --std <STD>        C++ language standard       [default: gnu++20]
        --cxxflag <FLAG>   extra compiler argument (repeatable)
        --compare <MODE>   exact or tokens             [default: exact]
        --checker <PROG>   custom checker executable
        --checker-arg <A>  argument before checker file paths (repeatable)
        --no-save          do not write the reduced input to disk
        --demo             run a built-in worked example
        --no-color         disable ANSI colour
    -h, --help             print this help
    -V, --version          print version

EXIT CODES:
    0  no counterexample found
    1  a counterexample was found
    2  something went wrong (compile error, missing file)"
    )
}
