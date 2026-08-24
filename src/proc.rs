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

#[cfg(unix)]
use std::os::unix::process::CommandExt;

/// Cap on how much of a child's output ccmin will buffer, applied independently
/// to stdout and stderr.
///
/// This bounds *our own* memory when a contestant program prints without limit.
/// It does not constrain what the child allocates: ccmin sets no address-space
/// or RSS limit, so a program that allocates unboundedly still can. See the
/// sandboxing note in the README.
pub const OUTPUT_LIMIT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CompareMode {
    #[default]
    Exact,
    Tokens,
}

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

    let mut command = Command::new(exe);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Descendants inherit this process group. On Windows the equivalent job
    // object is attached immediately after spawn.
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn()?;
    let process_tree = ProcessTree::attach(&child);

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
                    hit_output_limit = true;
                    break;
                }
                if start.elapsed() > timeout {
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

    // Also clean up descendants after a normal parent exit. A background child
    // can otherwise keep the captured pipes open and survive the test run.
    process_tree.terminate();
    let _ = child.kill();
    let _ = child.wait();

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

#[cfg(unix)]
struct ProcessTree {
    group: i32,
}

#[cfg(unix)]
impl ProcessTree {
    fn attach(child: &std::process::Child) -> Self {
        Self {
            group: child.id() as i32,
        }
    }

    fn terminate(&self) {
        const SIGKILL: i32 = 9;
        extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        // A negative pid targets the entire process group.
        unsafe {
            let _ = kill(-self.group, SIGKILL);
        }
    }
}

#[cfg(windows)]
struct ProcessTree {
    job: Option<windows_job::Job>,
}

#[cfg(windows)]
impl ProcessTree {
    fn attach(child: &std::process::Child) -> Self {
        // Job assignment may be rejected by an unusually restrictive parent
        // job. Parent-only termination remains as a safe fallback in that case.
        Self {
            job: windows_job::Job::attach(child).ok(),
        }
    }

    fn terminate(&self) {
        if let Some(job) = &self.job {
            job.terminate();
        }
    }
}

#[cfg(not(any(unix, windows)))]
struct ProcessTree;

#[cfg(not(any(unix, windows)))]
impl ProcessTree {
    fn attach(_: &std::process::Child) -> Self {
        Self
    }

    fn terminate(&self) {}
}

#[cfg(windows)]
#[allow(non_snake_case)]
mod windows_job {
    use std::ffi::c_void;
    use std::io;
    use std::mem::{size_of, zeroed};
    use std::os::windows::io::AsRawHandle;

    type Handle = *mut c_void;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: i32 = 9;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;

    #[repr(C)]
    struct BasicLimitInformation {
        PerProcessUserTimeLimit: i64,
        PerJobUserTimeLimit: i64,
        LimitFlags: u32,
        MinimumWorkingSetSize: usize,
        MaximumWorkingSetSize: usize,
        ActiveProcessLimit: u32,
        Affinity: usize,
        PriorityClass: u32,
        SchedulingClass: u32,
    }

    #[repr(C)]
    struct IoCounters {
        ReadOperationCount: u64,
        WriteOperationCount: u64,
        OtherOperationCount: u64,
        ReadTransferCount: u64,
        WriteTransferCount: u64,
        OtherTransferCount: u64,
    }

    #[repr(C)]
    struct ExtendedLimitInformation {
        BasicLimitInformation: BasicLimitInformation,
        IoInfo: IoCounters,
        ProcessMemoryLimit: usize,
        JobMemoryLimit: usize,
        PeakProcessMemoryUsed: usize,
        PeakJobMemoryUsed: usize,
    }

    extern "system" {
        fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(
            job: Handle,
            class: i32,
            info: *const c_void,
            length: u32,
        ) -> i32;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn TerminateJobObject(job: Handle, exit_code: u32) -> i32;
        fn CloseHandle(object: Handle) -> i32;
    }

    pub struct Job(Handle);

    impl Job {
        pub fn attach(child: &std::process::Child) -> io::Result<Self> {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
                if handle.is_null() {
                    return Err(io::Error::last_os_error());
                }

                let mut limits: ExtendedLimitInformation = zeroed();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    handle,
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                    &limits as *const _ as *const c_void,
                    size_of::<ExtendedLimitInformation>() as u32,
                ) == 0
                    || AssignProcessToJobObject(handle, child.as_raw_handle() as Handle) == 0
                {
                    let error = io::Error::last_os_error();
                    CloseHandle(handle);
                    return Err(error);
                }
                Ok(Self(handle))
            }
        }

        pub fn terminate(&self) {
            unsafe {
                let _ = TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
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

pub fn output_eq(a: &str, b: &str, mode: CompareMode) -> bool {
    match mode {
        // Preserve ccmin's original behaviour: ignore line-ending whitespace
        // and trailing blank lines, but keep internal whitespace significant.
        CompareMode::Exact => normalize(a) == normalize(b),
        // Typical token-based CP judging: all runs of whitespace are separators.
        CompareMode::Tokens => a.split_whitespace().eq(b.split_whitespace()),
    }
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

    #[test]
    fn exact_comparison_preserves_internal_whitespace() {
        assert!(!output_eq("1  2\n", "1 2\n", CompareMode::Exact));
        assert!(output_eq("1 2  \n\n", "1 2\n", CompareMode::Exact));
    }

    #[test]
    fn token_comparison_ignores_all_whitespace_runs() {
        assert!(output_eq(" 1  2\n3\t4 \n", "1\n2 3 4", CompareMode::Tokens));
        assert!(!output_eq("1 2", "1 3", CompareMode::Tokens));
    }
}
