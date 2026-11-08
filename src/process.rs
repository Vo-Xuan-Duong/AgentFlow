//! Bounded, logged subprocess execution. A Windows Job Object prevents agent process
//! trees from being left behind when the supervisor exits or terminates a command.
use crate::terminal;
use anyhow::{bail, Context, Result};
use std::{
    cell::Cell,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

thread_local! {
    static QUIET: Cell<bool> = const { Cell::new(false) };
}

/// Used by the full-screen UI worker so regular CLI output never corrupts
/// the alternate screen. The child still writes its full logs to disk.
pub fn with_quiet<T>(run: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            QUIET.with(|quiet| quiet.set(self.0));
        }
    }
    let previous = QUIET.with(|quiet| quiet.replace(true));
    let _reset = Reset(previous);
    run()
}

pub fn is_quiet() -> bool {
    QUIET.with(Cell::get)
}

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const MAX_STDERR_PREVIEW: usize = 320;

/// Only show bounded, printable stderr lines in the terminal. Full logs are
/// retained on disk. Antigravity's JSON stdout remains intact for validation.
fn show_stderr_updates(
    source: &mut fs::File,
    pending: &mut Vec<u8>,
    name: &str,
) -> Result<()> {
    let mut buffer = [0_u8; 2048];
    for _ in 0..8 {
        let count = source.read(&mut buffer)?;
        if count == 0 { break; }
        for &byte in &buffer[..count] {
            if byte == b'\n' {
                if !pending.is_empty() {
                    let raw = String::from_utf8_lossy(pending);
                    let printable: String = raw.chars()
                        .map(|ch| if ch.is_control() { ' ' } else { ch })
                        .collect();
                    if !is_quiet() { eprintln!("[AgentFlow][{name}] {}", printable.trim()); }
                    pending.clear();
                }
            } else if pending.len() < MAX_STDERR_PREVIEW {
                pending.push(byte);
            }
        }
        if count < buffer.len() { break; }
    }
    Ok(())
}

fn duration_label(duration: Duration) -> String {
    let secs = duration.as_secs();
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
}


pub fn run_logged(
    mut command: Command,
    dir: &Path,
    name: &str,
    input: Option<&str>,
    timeout: Duration,
    cancel_marker: Option<&Path>,
) -> Result<()> {
    fs::create_dir_all(dir)?;
    let stdout = fs::File::create(dir.join(format!("{name}.stdout.log")))?;
    let stderr_path = dir.join(format!("{name}.stderr.log"));
    let stderr = fs::File::create(&stderr_path)?;
    command.stdout(Stdio::from(stdout)).stderr(Stdio::from(stderr));

    // A file-backed stdin avoids blocking the supervisor on a pipe write before
    // it begins enforcing the deadline. Remove it after the process exits.
    let input_file: Option<PathBuf> = input
        .map(|text| -> Result<PathBuf> {
            let path = dir.join(format!("{name}.stdin.tmp"));
            fs::write(&path, text)?;
            Ok(path)
        })
        .transpose()?;
    if let Some(ref path) = input_file {
        command.stdin(Stdio::from(fs::File::open(path)?));
    } else {
        command.stdin(Stdio::null());
    }

    let result = (|| -> Result<()> {
        if terminal::stop_requested() || cancel_marker.is_some_and(Path::exists) {
            bail!("{name} cancelled before launch");
        }
        let mut child = command.spawn().with_context(|| format!("cannot start {name}"))?;
        let job = match ProcessJob::attach(&child) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("could not supervise child process safely");
            }
        };
        let started = Instant::now();
        let mut last_heartbeat = started;
        let mut stderr_reader = fs::File::open(&stderr_path)?;
        let mut stderr_pending = Vec::new();
        if !is_quiet() { eprintln!("[AgentFlow] RUNNING  {name} | PID {} | timeout {} | Ctrl+C to stop",
            child.id(), duration_label(timeout)); }
        loop {
            show_stderr_updates(&mut stderr_reader, &mut stderr_pending, name)?;
            match child.try_wait() {
                Ok(Some(status)) => {
                    show_stderr_updates(&mut stderr_reader, &mut stderr_pending, name)?;
                    if !stderr_pending.is_empty() {
                        let printable = String::from_utf8_lossy(&stderr_pending);
                        let clean: String = printable.chars()
                            .map(|ch| if ch.is_control() { ' ' } else { ch })
                            .collect();
                        if !is_quiet() { eprintln!("[AgentFlow][{name}] {}", clean.trim()); }
                    }
                    if !status.success() {
                        if !is_quiet() { eprintln!("[AgentFlow] FAILED   {name} | exit {status}"); }
                        bail!("{name} failed: {status}; inspect logs in {}", dir.display());
                    }
                    if !is_quiet() { eprintln!("[AgentFlow] DONE     {name} | elapsed {}",
                        duration_label(started.elapsed())); }
                    return Ok(());
                }
                Err(error) => {
                    terminate_and_reap(&mut child, &job)?;
                    return Err(error).context("could not wait for child process");
                }
                Ok(None) => {}
            }

            if terminal::stop_requested() || cancel_marker.is_some_and(Path::exists) {
                if !is_quiet() { eprintln!("[AgentFlow] STOPPING {name} | terminating child process tree..."); }
                terminate_and_reap(&mut child, &job)?;
                if !is_quiet() { eprintln!("[AgentFlow] CANCELLED {name} | logs in {}", dir.display()); }
                bail!("{name} cancelled; inspect logs in {}", dir.display());
            }
            if started.elapsed() >= timeout {
                if !is_quiet() { eprintln!("[AgentFlow] TIMEOUT  {name} | terminating child process tree..."); }
                terminate_and_reap(&mut child, &job)?;
                bail!("{name} timed out after {} seconds; inspect logs in {}", timeout.as_secs(), dir.display());
            }
            if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
                if !is_quiet() { eprintln!("[AgentFlow] RUNNING  {name} | {} elapsed | PID {} | Ctrl+C to stop",
                    duration_label(started.elapsed()), child.id()); }
                last_heartbeat = Instant::now();
            }
            thread::sleep(POLL_INTERVAL);
        }
    })();

    if let Some(path) = input_file {
        let _ = fs::remove_file(path);
    }
    result
}

fn terminate_and_reap(child: &mut Child, job: &ProcessJob) -> Result<()> {
    let termination = job.terminate(child);
    let reaped = child.wait();
    termination.context("could not terminate supervised process")?;
    reaped.context("could not reap supervised process")?;
    Ok(())
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{io, mem, os::windows::io::AsRawHandle, ptr};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
            TerminateJobObject, JobObjectExtendedLimitInformation,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
    };

    pub(super) struct ProcessJob(HANDLE);

    impl ProcessJob {
        pub(super) fn attach(child: &Child) -> Result<Self> {
            // SAFETY: null parameters create an unnamed Job Object with default security.
            let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
            if raw.is_null() {
                return Err(io::Error::last_os_error()).context("CreateJobObjectW failed");
            }
            let job = Self(raw);
            // SAFETY: zero is a valid baseline for this Windows POD structure.
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: job is live and limits points to a correctly sized structure.
            let configured = unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const _,
                    mem::size_of_val(&limits) as u32,
                )
            };
            if configured == 0 {
                return Err(io::Error::last_os_error()).context("SetInformationJobObject failed");
            }
            // SAFETY: child owns a valid process handle while it is alive.
            let assigned = unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) };
            if assigned == 0 {
                return Err(io::Error::last_os_error()).context("AssignProcessToJobObject failed");
            }
            Ok(job)
        }

        pub(super) fn terminate(&self, child: &mut Child) -> Result<()> {
            // SAFETY: the job handle is owned by self and remains open here.
            let stopped = unsafe { TerminateJobObject(self.0, 1) };
            if stopped == 0 {
                let error = io::Error::last_os_error();
                let _ = child.kill();
                return Err(error).context("TerminateJobObject failed");
            }
            Ok(())
        }
    }

    impl Drop for ProcessJob {
        fn drop(&mut self) {
            // SAFETY: this is the final close of this owned Job Object handle.
            // The KILL_ON_JOB_CLOSE limit also reaps any surviving descendants.
            unsafe { CloseHandle(self.0) };
        }
    }
}
#[cfg(windows)]
use windows::ProcessJob;

#[cfg(not(windows))]
struct ProcessJob;

#[cfg(not(windows))]
impl ProcessJob {
    fn attach(_child: &Child) -> Result<Self> {
        Ok(Self)
    }

    fn terminate(&self, child: &mut Child) -> Result<()> {
        child.kill().context("could not kill child process")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_before_spawn_does_not_run_command() {
        let dir = std::env::temp_dir().join(format!("agentflow-cancel-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("cancel.request");
        fs::write(&marker, b"cancel").unwrap();
        let cmd = Command::new("this-program-must-not-exist-agentflow");
        let result = run_logged(cmd, &dir, "cancel", None, Duration::from_secs(3), Some(&marker));
        assert!(format!("{:#}", result.unwrap_err()).contains("cancelled before launch"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn elapsed_time_is_human_readable() {
        assert_eq!(duration_label(Duration::from_secs(3661)), "01:01:01");
    }

    #[cfg(windows)]
    #[test]
    fn timeout_terminates_windows_process() {
        let dir = std::env::temp_dir().join(format!("agentflow-timeout-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut cmd = Command::new("powershell.exe");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 8"]);
        let result = run_logged(cmd, &dir, "timeout", None, Duration::from_millis(300), None);
        assert!(format!("{:#}", result.unwrap_err()).contains("timed out"));
        let _ = fs::remove_dir_all(dir);
    }
}
