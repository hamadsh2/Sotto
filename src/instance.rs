// SPDX-License-Identifier: GPL-3.0-only
//! "Is another Sotto already running?" — shared by the window app and `sotto-cli`.
//!
//! Two copies would both write to CABLE Input, so whichever starts first holds a named
//! Windows mutex for as long as it runs, and the other one refuses to start its audio.
//! "Local\" names are per Windows session (per signed-in user). Windows releases the
//! mutex by itself when the process ends, even after a crash.
//!
//! Python analogy: a lock file that the OS deletes for you when the program exits.

use std::ffi::c_void;

type Handle = *mut c_void;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateMutexW(attr: *const c_void, initial_owner: i32, name: *const u16) -> Handle;
    fn GetLastError() -> u32;
    fn CloseHandle(h: Handle) -> i32;
}

const MUTEX: &str = r"Local\Sotto.Instance";
const ERROR_ALREADY_EXISTS: u32 = 183;

/// Proof that this process is the only Sotto. Keep it alive for the whole run; dropping
/// it lets another copy start.
pub struct Claim(Handle);

// SAFETY: the handle is only closed in `drop`; Windows handles may be used from any
// thread.
unsafe impl Send for Claim {}

impl Drop for Claim {
    fn drop(&mut self) {
        // SAFETY: our own handle, closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

pub enum Outcome {
    /// We are the only one.
    Alone(Claim),
    /// Another Sotto (the app or `sotto-cli`) is running.
    Taken,
    /// Windows couldn't tell us (very unlikely); don't block starting.
    Unknown,
}

pub fn claim() -> Outcome {
    let name: Vec<u16> = MUTEX.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: plain Win32 calls with a valid zero-terminated name.
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if h.is_null() {
            return Outcome::Unknown;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            CloseHandle(h);
            return Outcome::Taken;
        }
        Outcome::Alone(Claim(h))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_claim_at_a_time() {
        // Other tests or a running Sotto could hold it; only check when we get it.
        if let Outcome::Alone(first) = claim() {
            assert!(matches!(claim(), Outcome::Taken));
            drop(first);
            assert!(matches!(claim(), Outcome::Alone(_)));
        }
    }
}
