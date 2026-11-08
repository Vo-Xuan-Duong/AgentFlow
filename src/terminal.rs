//! Foreground-only terminal lifecycle. No daemon, background worker or detached child.
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn stop_requested() -> bool {
    STOP_REQUESTED.load(Ordering::SeqCst)
}


#[cfg(windows)]
mod windows {
    use super::*;
    use anyhow::bail;
    use windows_sys::Win32::System::Console::{
        SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT,
    };

    // SAFETY: The Windows console invokes this callback. Only an atomic flag is
    // modified in the callback; no blocking I/O or allocation is performed.
    unsafe extern "system" fn control_handler(event: u32) -> i32 {
        match event {
            CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT => {
                STOP_REQUESTED.store(true, Ordering::SeqCst);
                1
            }
            _ => 0,
        }
    }

    pub(super) fn install() -> Result<()> {
        // SAFETY: control_handler has the required Windows callback signature.
        let installed = unsafe { SetConsoleCtrlHandler(Some(control_handler), 1) };
        if installed == 0 {
            bail!("could not install CMD Ctrl+C handler")
        }
        Ok(())
    }
}

/// Install only for foreground commands which spawn supervised subprocesses.
/// A console-close event can terminate a process before state is saved.
/// The Windows Job Object closes when the process exits and kills its children;
/// a later 'status' call can mark the interrupted phase as needs_attention.
pub fn install() -> Result<()> {
    #[cfg(windows)]
    windows::install()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_is_initially_unset() {
        assert!(!stop_requested());
    }
}
