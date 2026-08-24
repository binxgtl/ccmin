//! Decides whether a given input still exhibits the bug.
//!
//! The shrinking invariant is deliberately stricter than "the two programs
//! disagree": we require the *same kind* of failure as the original. A shrink
//! that turns a wrong-answer into a crash is almost always a sign that the
//! input became malformed or left the problem's constraints, and accepting it
//! would produce a reduced case that does not reproduce the real bug.

use crate::proc::{self, CompareMode, RunOutput};
use std::io;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FailKind {
    /// Both ran cleanly, outputs differ.
    WrongAnswer,
    SolCrashed,
    SolTimedOut,
    SolOutputLimit,
    BruteCrashed,
    BruteTimedOut,
    BruteOutputLimit,
    /// Not shrinkable: the reference did not remain a trustworthy oracle.
    BothFailed,
}

impl FailKind {
    pub fn describe(&self) -> &'static str {
        match self {
            FailKind::WrongAnswer => "wrong answer (outputs differ)",
            FailKind::SolCrashed => "solution crashed",
            FailKind::SolTimedOut => "solution timed out",
            FailKind::SolOutputLimit => "solution exceeded the output limit",
            FailKind::BruteCrashed => "brute force crashed",
            FailKind::BruteTimedOut => "brute force timed out",
            FailKind::BruteOutputLimit => "brute force exceeded the output limit",
            FailKind::BothFailed => "solution and brute force both failed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Failure {
    pub kind: FailKind,
    pub sol_output: String,
    pub brute_output: String,
    pub note: String,
}

#[derive(Clone, Debug)]
pub struct CheckerConfig {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub scratch_dir: PathBuf,
}

pub struct Oracle {
    pub sol: PathBuf,
    pub brute: PathBuf,
    pub timeout: Duration,
    pub compare_mode: CompareMode,
    pub checker: Option<CheckerConfig>,
    pub program_runs: usize,
}

impl Oracle {
    pub fn new(
        sol: PathBuf,
        brute: PathBuf,
        timeout: Duration,
        compare_mode: CompareMode,
        checker: Option<CheckerConfig>,
    ) -> Self {
        Oracle {
            sol,
            brute,
            timeout,
            compare_mode,
            checker,
            program_runs: 0,
        }
    }

    /// `None` means the input passes.
    pub fn judge(&mut self, input: &str) -> std::io::Result<Option<Failure>> {
        self.program_runs += 2;

        let sol = proc::run(&self.sol, &[], input, self.timeout)?;
        // Always run the reference too. For a solution crash/timeout to be a
        // useful counterexample, the reference must still accept the input.
        let brute = proc::run(&self.brute, &[], input, self.timeout)?;
        if let Some(failure) = classify_process_failures(&sol, &brute) {
            return Ok(Some(failure));
        }

        if let Some(checker) = &self.checker {
            self.program_runs += 1;
            return run_checker(checker, input, &sol, &brute, self.timeout);
        }

        Ok(classify_outputs(&sol, &brute, self.compare_mode))
    }

    /// The shrinking predicate: does this candidate reproduce the same failure?
    pub fn preserves(&mut self, input: &str, target: FailKind) -> io::Result<bool> {
        if target == FailKind::BothFailed {
            return Ok(false);
        }
        Ok(matches!(self.judge(input)?, Some(f) if f.kind == target))
    }

    /// Guard against flaky solutions (uninitialised memory, hash iteration
    /// order). Shrinking a nondeterministic failure chases ghosts for minutes
    /// and yields a reduced case that does not reproduce.
    pub fn is_stable(&mut self, input: &str, target: FailKind, tries: usize) -> io::Result<bool> {
        for _ in 0..tries {
            if !self.preserves(input, target)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
fn classify(sol: &RunOutput, brute: &RunOutput, compare_mode: CompareMode) -> Option<Failure> {
    classify_process_failures(sol, brute).or_else(|| classify_outputs(sol, brute, compare_mode))
}

fn classify_process_failures(sol: &RunOutput, brute: &RunOutput) -> Option<Failure> {
    let sol_failure = run_failure(
        sol,
        FailKind::SolOutputLimit,
        FailKind::SolTimedOut,
        FailKind::SolCrashed,
    );
    let brute_failure = run_failure(
        brute,
        FailKind::BruteOutputLimit,
        FailKind::BruteTimedOut,
        FailKind::BruteCrashed,
    );

    match (sol_failure, brute_failure) {
        (Some(sol_kind), Some(brute_kind)) => Some(Failure {
            kind: FailKind::BothFailed,
            sol_output: sol.stdout.clone(),
            brute_output: brute.stdout.clone(),
            note: format!(
                "{}; {}",
                detail(sol_kind, &sol.stderr),
                detail(brute_kind, &brute.stderr)
            ),
        }),
        (Some(kind), None) => Some(Failure {
            kind,
            sol_output: sol.stdout.clone(),
            brute_output: brute.stdout.clone(),
            note: detail(kind, &sol.stderr),
        }),
        (None, Some(kind)) => Some(Failure {
            kind,
            sol_output: sol.stdout.clone(),
            brute_output: brute.stdout.clone(),
            note: detail(kind, &brute.stderr),
        }),
        (None, None) => None,
    }
}

fn classify_outputs(
    sol: &RunOutput,
    brute: &RunOutput,
    compare_mode: CompareMode,
) -> Option<Failure> {
    if proc::output_eq(&sol.stdout, &brute.stdout, compare_mode) {
        None
    } else {
        Some(wrong_answer(sol, brute, String::new()))
    }
}

fn run_checker(
    checker: &CheckerConfig,
    input: &str,
    sol: &RunOutput,
    brute: &RunOutput,
    timeout: Duration,
) -> io::Result<Option<Failure>> {
    let input_path = checker.scratch_dir.join("checker-input.txt");
    let actual_path = checker.scratch_dir.join("checker-actual.txt");
    let expected_path = checker.scratch_dir.join("checker-expected.txt");
    for (path, contents) in [
        (&input_path, input),
        (&actual_path, sol.stdout.as_str()),
        (&expected_path, brute.stdout.as_str()),
    ] {
        std::fs::write(path, contents).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("cannot write checker file {}: {e}", path.display()),
            )
        })?;
    }

    let mut args = checker.args.clone();
    args.extend(
        [&input_path, &actual_path, &expected_path]
            .iter()
            .map(|path| path.to_string_lossy().into_owned()),
    );
    let output = proc::run(&checker.program, &args, "", timeout).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot run checker {}: {e}", checker.program.display()),
        )
    })?;

    checker_outcome(&output, sol, brute)
}

fn checker_outcome(
    output: &RunOutput,
    sol: &RunOutput,
    brute: &RunOutput,
) -> io::Result<Option<Failure>> {
    if output.output_limited {
        return Err(checker_error("exceeded the output limit", output));
    }
    if output.timed_out {
        return Err(checker_error("timed out", output));
    }
    match output.code {
        Some(0) => Ok(None),
        Some(1) => Ok(Some(wrong_answer(sol, brute, checker_note(output)))),
        Some(code) => Err(checker_error(&format!("exited with code {code}"), output)),
        None => Err(checker_error("terminated without an exit code", output)),
    }
}

fn wrong_answer(sol: &RunOutput, brute: &RunOutput, note: String) -> Failure {
    Failure {
        kind: FailKind::WrongAnswer,
        sol_output: proc::normalize(&sol.stdout),
        brute_output: proc::normalize(&brute.stdout),
        note,
    }
}

fn checker_error(reason: &str, output: &RunOutput) -> io::Error {
    let note = checker_note(output);
    let suffix = if note.is_empty() {
        String::new()
    } else {
        format!(": {note}")
    };
    io::Error::other(format!("custom checker {reason}{suffix}"))
}

fn checker_note(output: &RunOutput) -> String {
    let stderr = first_line(&output.stderr);
    if stderr.is_empty() {
        first_line(&output.stdout)
    } else {
        stderr
    }
}

fn run_failure(
    r: &RunOutput,
    on_output_limit: FailKind,
    on_timeout: FailKind,
    on_crash: FailKind,
) -> Option<FailKind> {
    if r.output_limited {
        Some(on_output_limit)
    } else if r.timed_out {
        Some(on_timeout)
    } else if !r.exited_cleanly() {
        Some(on_crash)
    } else {
        None
    }
}

fn detail(kind: FailKind, stderr: &str) -> String {
    let line = first_line(stderr);
    if line.is_empty() {
        kind.describe().to_string()
    } else {
        format!("{}: {line}", kind.describe())
    }
}

fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(code: Option<i32>) -> RunOutput {
        RunOutput {
            stdout: String::new(),
            stderr: String::new(),
            code,
            timed_out: false,
            output_limited: false,
        }
    }

    #[test]
    fn solution_crash_requires_clean_brute() {
        let failure = classify(&run(Some(1)), &run(Some(0)), CompareMode::Exact).unwrap();
        assert_eq!(failure.kind, FailKind::SolCrashed);

        let failure = classify(&run(Some(1)), &run(Some(1)), CompareMode::Exact).unwrap();
        assert_eq!(failure.kind, FailKind::BothFailed);
    }

    #[test]
    fn output_limit_has_its_own_failure_kind() {
        let mut sol = run(None);
        sol.output_limited = true;
        let failure = classify(&sol, &run(Some(0)), CompareMode::Exact).unwrap();
        assert_eq!(failure.kind, FailKind::SolOutputLimit);
    }

    #[test]
    fn configured_comparison_mode_controls_wrong_answer() {
        let mut sol = run(Some(0));
        sol.stdout = "1  2\n".into();
        let mut brute = run(Some(0));
        brute.stdout = "1 2\n".into();

        assert_eq!(
            classify(&sol, &brute, CompareMode::Exact).unwrap().kind,
            FailKind::WrongAnswer
        );
        assert!(classify(&sol, &brute, CompareMode::Tokens).is_none());
    }

    #[test]
    fn checker_exit_contract_distinguishes_wa_from_checker_errors() {
        let sol = run(Some(0));
        let brute = run(Some(0));

        assert!(checker_outcome(&run(Some(0)), &sol, &brute)
            .unwrap()
            .is_none());

        let mut rejected = run(Some(1));
        rejected.stderr = "not optimal\nmore detail".into();
        let failure = checker_outcome(&rejected, &sol, &brute).unwrap().unwrap();
        assert_eq!(failure.kind, FailKind::WrongAnswer);
        assert_eq!(failure.note, "not optimal");

        let error = checker_outcome(&run(Some(2)), &sol, &brute).unwrap_err();
        assert!(error.to_string().contains("exited with code 2"));
    }

    #[test]
    fn checker_timeout_is_a_tool_error_not_a_preserved_failure() {
        let sol = run(Some(0));
        let brute = run(Some(0));
        let mut checker = run(None);
        checker.timed_out = true;
        let error = checker_outcome(&checker, &sol, &brute).unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }
}
