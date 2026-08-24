//! Running a child process with piped stdin, captured output, and a wall-clock
//! timeout. Uses only `std`: reader threads drain stdout/stderr so a chatty
//! child can never deadlock against a full pipe buffer.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

pub struct RunOutput {
    pub stdout: String,
    pub stderr: String,
    /// `None` when the process was killed by a signal or by our timeout.
    pub code: Option<i32>,
    pub timed_out: bool,
}

impl RunOutput {
    pub fn exited_cleanly(&self) -> bool {
        !self.timed_out && self.code == Some(0)
    }
}

pub fn run(
    exe: &Path,
    args: &[String],
    stdin_data: &str,
    timeout: Duration,
) -> std::io::Result<RunOutput> {
    let start = Instant::now();

    let mut child = Command::new(exe)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Feed stdin from its own thread. If the child exits without reading all
    // of its input we get EPIPE here, which is expected and ignored.
    if let Some(mut sink) = child.stdin.take() {
        let data = stdin_data.as_bytes().to_vec();
        thread::spawn(move || {
            let _ = sink.write_all(&data);
        });
    }

    let (tx_out, rx_out) = mpsc::channel();
    if let Some(mut src) = child.stdout.take() {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = src.read_to_end(&mut buf);
            let _ = tx_out.send(buf);
        });
    }

    let (tx_err, rx_err) = mpsc::channel();
    if let Some(mut src) = child.stderr.take() {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = src.read_to_end(&mut buf);
            let _ = tx_err.send(buf);
        });
    }

    let mut timed_out = false;
    let mut code = None;
    let mut backoff = Duration::from_micros(200);
    loop {
        match child.try_wait()? {
            Some(status) => {
                code = status.code();
                break;
            }
            None => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break;
                }
                thread::sleep(backoff);
                // Ramp the poll interval so short programs stay fast while long
                // ones don't spin a core.
                if backoff < Duration::from_millis(4) {
                    backoff *= 2;
                }
            }
        }
    }

    let grace = Duration::from_millis(500);
    let stdout = rx_out.recv_timeout(grace).unwrap_or_default();
    let stderr = rx_err.recv_timeout(grace).unwrap_or_default();

    Ok(RunOutput {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        code,
        timed_out,
    })
}

/// Compare two program outputs the way a judge would: ignore trailing
/// whitespace on each line and any number of trailing blank lines.
pub fn output_eq(a: &str, b: &str) -> bool {
    normalize(a) == normalize(b)
}

pub fn normalize(s: &str) -> String {
    let mut lines: Vec<&str> = s.lines().map(|l| l.trim_end()).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}
