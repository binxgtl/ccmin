//! Decides whether a given input still exhibits the bug.
//!
//! The shrinking invariant is deliberately stricter than "the two programs
//! disagree": we require the *same kind* of failure as the original. A shrink
//! that turns a wrong-answer into a crash is almost always a sign that the
//! input became malformed or left the problem's constraints, and accepting it
//! would produce a minimal case that does not reproduce the real bug.

use crate::proc::{self, RunOutput};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FailKind {
    /// Both ran cleanly, outputs differ.
    WrongAnswer,
    SolCrashed,
    SolTimedOut,
    BruteCrashed,
    BruteTimedOut,
}

impl FailKind {
    pub fn describe(&self) -> &'static str {
        match self {
            FailKind::WrongAnswer => "wrong answer (outputs differ)",
            FailKind::SolCrashed => "solution crashed",
            FailKind::SolTimedOut => "solution timed out",
            FailKind::BruteCrashed => "brute force crashed",
            FailKind::BruteTimedOut => "brute force timed out",
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
    pub calls: usize,
}

impl Oracle {
    pub fn new(sol: PathBuf, brute: PathBuf, timeout: Duration) -> Self {
        Oracle {
            sol,
            brute,
            timeout,
            calls: 0,
        }
    }

    /// `None` means the input passes.
    pub fn judge(&mut self, input: &str) -> std::io::Result<Option<Failure>> {
        self.calls += 1;

        let sol = proc::run(&self.sol, &[], input, self.timeout)?;
        if let Some(kind) = crash_kind(&sol, FailKind::SolTimedOut, FailKind::SolCrashed) {
            return Ok(Some(Failure {
                kind,
                sol_output: sol.stdout.clone(),
                brute_output: String::new(),
                note: first_line(&sol.stderr),
            }));
        }

        let brute = proc::run(&self.brute, &[], input, self.timeout)?;
        if let Some(kind) = crash_kind(&brute, FailKind::BruteTimedOut, FailKind::BruteCrashed) {
            return Ok(Some(Failure {
                kind,
                sol_output: sol.stdout.clone(),
                brute_output: brute.stdout.clone(),
                note: first_line(&brute.stderr),
            }));
        }

        if proc::output_eq(&sol.stdout, &brute.stdout) {
            return Ok(None);
        }

        Ok(Some(Failure {
            kind: FailKind::WrongAnswer,
            sol_output: proc::normalize(&sol.stdout),
            brute_output: proc::normalize(&brute.stdout),
            note: String::new(),
        }))
    }

    /// The shrinking predicate: does this candidate reproduce the same failure?
    pub fn preserves(&mut self, input: &str, target: FailKind) -> bool {
        matches!(self.judge(input), Ok(Some(f)) if f.kind == target)
    }

    /// Guard against flaky solutions (uninitialised memory, hash iteration
    /// order). Shrinking a nondeterministic failure chases ghosts for minutes
    /// and yields a "minimal" case that does not reproduce.
    pub fn is_stable(&mut self, input: &str, target: FailKind, tries: usize) -> bool {
        (0..tries).all(|_| self.preserves(input, target))
    }
}

fn crash_kind(r: &RunOutput, on_timeout: FailKind, on_crash: FailKind) -> Option<FailKind> {
    if r.timed_out {
        Some(on_timeout)
    } else if !r.exited_cleanly() {
        Some(on_crash)
    } else {
        None
    }
}

fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}
