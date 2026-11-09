//! Orderly exit on Ctrl+C. The default handler kills the process without
//! running destructors, which would leave the virtual monitor attached.

use std::sync::atomic::{AtomicBool, Ordering};

static REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn requested() -> bool {
    REQUESTED.load(Ordering::Relaxed)
}

pub fn request() {
    REQUESTED.store(true, Ordering::Relaxed);
}

/// Routes Ctrl+C, Ctrl+Break and console close to [`requested`].
#[cfg(windows)]
pub fn install_handler() {
    use windows::Win32::Foundation::BOOL;
    use windows::Win32::System::Console::SetConsoleCtrlHandler;

    unsafe extern "system" fn handler(_ctrl_type: u32) -> BOOL {
        request();
        // Handled: the main loop exits on its own and cleans up.
        BOOL::from(true)
    }
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(handler), true);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn request_is_observable() {
        // Process-wide state; this is the only test that touches it.
        assert!(!super::requested());
        super::request();
        assert!(super::requested());
    }
}
