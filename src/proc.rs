//! Running a child process with piped stdin, captured output, and a wall-clock
//! timeout. Uses only `std`: reader threads drain stdout/stderr so a chatty
//! child can never deadlock against a full pipe buffer.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

/// A runaway contestant program must not be able to consume unbounded memory.
/// The limit applies independently to stdout and stderr.
pub const OUTPUT_LIMIT_BYTES: usize = 16 * 1024 * 1024;

pub struct RunOutput {
    pub stdout: String,
    pub stderr: String,
    /// `None` when the process was killed by a signal or by our timeout.
    pub code: Option<i32>,
    pub timed_out: bool,
    pub output_limited: bool,
}

impl RunOutput {
    pub fn exited_cleanly(&self) -> bool {
        !self.timed_out && !self.output_limited && self.code == Some(0)
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

    let output_limited = Arc::new(AtomicBool::new(false));

    let (tx_out, rx_out) = mpsc::channel();
    if let Some(mut src) = child.stdout.take() {
        let limited = Arc::clone(&output_limited);
        thread::spawn(move || {
            let buf = read_capped(&mut src, OUTPUT_LIMIT_BYTES, &limited);
            let _ = tx_out.send(buf);
        });
    }

    let (tx_err, rx_err) = mpsc::channel();
    if let Some(mut src) = child.stderr.take() {
        let limited = Arc::clone(&output_limited);
        thread::spawn(move || {
            let buf = read_capped(&mut src, OUTPUT_LIMIT_BYTES, &limited);
            let _ = tx_err.send(buf);
        });
    }

    let mut timed_out = false;
    let mut hit_output_limit = false;
    let mut code = None;
    let mut backoff = Duration::from_micros(200);
    loop {
        match child.try_wait()? {
            Some(status) => {
                code = status.code();
                break;
            }
            None => {
                if output_limited.load(Ordering::Relaxed) {
                    let _ = child.kill();
                    let _ = child.wait();
                    hit_output_limit = true;
                    break;
                }
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
    hit_output_limit |= output_limited.load(Ordering::Relaxed);

    Ok(RunOutput {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        code,
        timed_out,
        output_limited: hit_output_limit,
    })
}

fn read_capped(src: &mut impl Read, cap: usize, limited: &AtomicBool) -> Vec<u8> {
    let mut out = Vec::with_capacity(cap.min(64 * 1024));
    let mut chunk = [0u8; 8192];
    loop {
        match src.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let available = cap.saturating_sub(out.len());
                out.extend_from_slice(&chunk[..n.min(available)]);
                if n > available {
                    limited.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn capped_reader_stops_at_limit_and_marks_it() {
        let mut src = Cursor::new(vec![b'x'; 100]);
        let limited = AtomicBool::new(false);
        let out = read_capped(&mut src, 16, &limited);
        assert_eq!(out.len(), 16);
        assert!(limited.load(Ordering::Relaxed));
    }

    #[test]
    fn capped_reader_accepts_output_exactly_at_limit() {
        let mut src = Cursor::new(vec![b'x'; 16]);
        let limited = AtomicBool::new(false);
        let out = read_capped(&mut src, 16, &limited);
        assert_eq!(out.len(), 16);
        assert!(!limited.load(Ordering::Relaxed));
    }
}
