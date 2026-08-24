# ccmin

**Stress testing finds a failing test. `ccmin` shrinks it until you can read it.**

Every stress-testing script stops at the moment your solution and your brute
force disagree — and hands you a 100-element array to squint at. `ccmin` keeps
going, deleting elements and pulling values toward zero for as long as the bug
survives, until what is left is small enough to reason about.

```
$ ccmin
[1/3] compiling with g++... done
[2/3] stress testing (up to 200 cases)...
      failed on test #2 -- wrong answer (outputs differ)
      initial counterexample: 100 values, shape: array
[3/3] shrinking...
      size   100 -> 50 -> 25 -> 12 -> 6 -> 3 -> 1
      values avg magnitude 450,715,494 -> 1
      shrunk in 377ms (32 total program runs)

----------------------------------------------------------
REDUCED FAILING INPUT
1
-1
----------------------------------------------------------
expected (brute): -1
actual   (sol):   0
----------------------------------------------------------
wrote minimal.in
```

A hundred random values with an average magnitude of 450 million, reduced to a
single `-1`. The bug is a Kadane implementation that starts `best` at zero and
therefore allows the empty subarray. You can see that from the reduced input.
You could not have seen it from the original.

## Try it in 30 seconds

No setup, no files to write:

```bash
ccmin --demo
```

That builds a worked example — a buggy solution, a reference implementation and
a generator — in a temporary directory and shrinks a real counterexample.

## Install

```bash
cargo install ccmin
```

Or grab a prebuilt binary from [Releases](../../releases). Single file, no
runtime, nothing to configure.

## Use

Put three files in a directory, the same ones you already write when stress
testing:

| file | what it does |
| --- | --- |
| `sol.cpp` | the solution you suspect |
| `brute.cpp` | a slow implementation you trust |
| `gen.cpp` | a generator; receives a seed as `argv[1]` |

Then:

```bash
ccmin
```

Explicit paths if you name things differently:

```bash
ccmin -s solution.cpp -b slow.cpp -g generator.cpp -n 1000
```

```
-s, --sol <FILE>       solution to test        [default: sol.cpp]
-b, --brute <FILE>     reference solution      [default: brute.cpp]
-g, --gen <FILE>       test generator          [default: gen.cpp]
-n, --iters <N>        stress cases to try     [default: 200]
-t, --timeout <MS>     per-run timeout         [default: 3000]
-o, --out <FILE>       where to save the case  [default: minimal.in]
    --no-save          do not write minimal.in
    --shape <SHAPE>    auto, array, multitest, or raw
    --n-index <INDEX>  length field in an array header (zero-based)
    --strict           disable heuristic extended-header detection
    --demo             run the built-in example
    --no-color         disable ANSI colour
```

Exit codes: `0` nothing found, `1` counterexample found, `2` something broke.
Usable in a script.

## The part that is actually hard

Shrinking a test input is easy to do wrong, and doing it wrong is worse than
not doing it at all.

The naive approach treats the input as text and deletes tokens. Delete two
numbers from an array but leave `N = 5` unchanged, and both programs read past
the end of their buffers. They now disagree because of undefined behaviour
rather than because of your bug, the shrinker happily accepts it, and you are
handed a "minimal counterexample" that cannot occur in real judge data. You
lose an hour chasing a bug that is not there.

`ccmin` parses the input into a shape it understands and shrinks the *shape*,
re-rendering the text from scratch for every candidate. The length prefix is
correct by construction — a desynchronised `N` is unrepresentable. Recognised
shapes:

- `N` followed by `N` integers, optionally with extra header scalars (`N K`)
- `T` followed by `T` independent `N` + `N integers` cases

Auto-detection is convenient but cannot prove a problem's schema. In
particular, extended headers are inferred when one of their first three values
happens to equal the remaining token count. `ccmin` warns when it makes this
heuristic match. Override it when you know the format:

```bash
ccmin --shape array                 # confirm an array model
ccmin --shape array --n-index 1     # e.g. K N, with N at index 1
ccmin --shape multitest             # T blocks of N + N integers
ccmin --shape raw                   # never update a length field
ccmin --strict                      # auto-detect only simple N / T forms
```

Two further guards:

- **Failure kind is preserved.** A shrink that turns a wrong answer into a
  crash is rejected. That is almost always a sign the input drifted out of the
  problem's constraints, and the smaller case would not reproduce the real bug.
  Both programs are always run: a solution crash or timeout is retained only
  while the brute force still exits successfully.
- **Flaky failures are detected.** The counterexample must reproduce three
  times before shrinking starts. Nondeterministic solutions — uninitialised
  memory, `unordered_map` iteration order — otherwise send the shrinker chasing
  ghosts and yield a reduced case that does not fail when you run it yourself.

Anything `ccmin` cannot classify still shrinks at the token level, but it says
so, because in that mode the guarantee above does not hold.

The result is a **small, locally reduced counterexample**, not a proof of the
globally smallest possible input. Structural delta debugging and bounded value
shrinking can stop at a local minimum.

## Windows works without a developer prompt

`cl.exe` is not on `PATH` outside a Visual Studio Developer Command Prompt,
which is why most tools in this space simply fail on a stock Windows machine.
`ccmin` locates your install with `vswhere` and bootstraps the environment
itself. `g++` and `clang++` are preferred when present, since GNU G++ is what
Codeforces actually runs.

Note that `vcvars64.bat` takes several seconds, so the first compile step is
noticeably slower on MSVC than on `g++`. All three programs are built in a
single batch so you pay that cost once rather than three times.

## What it does not do yet

Being specific about this, because a shrinker that quietly mangles your input
is worse than no shrinker:

- **Graphs, trees and geometry are not modelled.** They fall back to token-level
  shrinking with a warning. Trees are the most-requested shape and are next.
- **Constraints are not read.** If the problem says `1 <= a_i <= 10^9`, `ccmin`
  may shrink a value to `0`. Nothing parses the statement. A `--min-value` flag
  is the likely fix.
- **Only the failure kind is preserved, not the specific wrong answer.** A
  shrink that changes *how* the outputs differ is still accepted.
- **Single tokens are not shrunk.** A long string stays a long string; only
  whole tokens are removed.
- **Checker-based problems are unsupported.** Outputs are compared literally
  (modulo trailing whitespace), so problems with multiple valid answers will
  report false failures.

## Zero dependencies

The `[dependencies]` section of `Cargo.toml` is empty, and stays that way.
`ccmin` runs compilers and executes binaries on your machine; that is a bad
place to inherit a supply chain. Everything here is `std`, including the
Windows console handling and the process timeouts.

Contestant stdout and stderr are each capped at 16 MiB. Exceeding that cap is
reported as an output-limit failure. `ccmin` is not a sandbox: the compiler,
generator, solution and brute-force binaries run with your normal user
permissions, so do not use it for untrusted code.

The only file it writes to your working directory is `minimal.in`, and
`--no-save` turns that off. Build artifacts go to a temp directory.

## Building from source

```bash
cargo build --release
cargo test
```

## License

MIT
