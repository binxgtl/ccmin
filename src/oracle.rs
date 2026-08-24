//! Decides whether a given input still exhibits the bug.
//!
//! The shrinking invariant is deliberately stricter than "the two programs
//! disagree": we require the *same kind* of failure as the original. A shrink
//! that turns a wrong-answer into a crash is almost always a sign that the
//! input became malformed or left the problem's constraints, and accepting it
//! would produce a reduced case that does not reproduce the real bug.

use crate::proc::{self, CompareMode, RunOutput};
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

pub struct Oracle {
    pub sol: PathBuf,
    pub brute: PathBuf,
    pub timeout: Duration,
    pub compare_mode: CompareMode,
    pub program_runs: usize,
}

impl Oracle {
    pub fn new(sol: PathBuf, brute: PathBuf, timeout: Duration, compare_mode: CompareMode) -> Self {
        Oracle {
            sol,
            brute,
            timeout,
            compare_mode,
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
        Ok(classify(&sol, &brute, self.compare_mode))
    }

    /// The shrinking predicate: does this candidate reproduce the same failure?
    pub fn preserves(&mut self, input: &str, target: FailKind) -> bool {
        target != FailKind::BothFailed
            && matches!(self.judge(input), Ok(Some(f)) if f.kind == target)
    }

    /// Guard against flaky solutions (uninitialised memory, hash iteration
    /// order). Shrinking a nondeterministic failure chases ghosts for minutes
    /// and yields a reduced case that does not reproduce.
    pub fn is_stable(&mut self, input: &str, target: FailKind, tries: usize) -> bool {
        (0..tries).all(|_| self.preserves(input, target))
    }
}

fn classify(sol: &RunOutput, brute: &RunOutput, compare_mode: CompareMode) -> Option<Failure> {
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
        (None, None) if proc::output_eq(&sol.stdout, &brute.stdout, compare_mode) => None,
        (None, None) => Some(Failure {
            kind: FailKind::WrongAnswer,
            sol_output: proc::normalize(&sol.stdout),
            brute_output: proc::normalize(&brute.stdout),
            note: String::new(),
        }),
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
}
