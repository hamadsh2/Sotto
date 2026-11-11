// SPDX-License-Identifier: GPL-3.0-only
//! The few Win32 calls the window app needs, declared directly (like `stats.rs` does)
//! instead of adding a crate: find our own window, hide it, show it again.
//!
//! These only affect Sotto's own window. Nothing here touches audio or system settings.

use std::ffi::c_void;

type Hwnd = *mut c_void;

#[link(name = "user32")]
unsafe extern "system" {
    fn EnumWindows(callback: extern "system" fn(Hwnd, isize) -> i32, lparam: isize) -> i32;
    fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
    fn GetClassNameW(hwnd: Hwnd, buf: *mut u16, len: i32) -> i32;
    fn IsWindow(hwnd: Hwnd) -> i32;
    fn ShowWindow(hwnd: Hwnd, cmd: i32) -> i32;
    fn IsIconic(hwnd: Hwnd) -> i32;
    fn SetForegroundWindow(hwnd: Hwnd) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcessId() -> u32;
}

const SW_HIDE: i32 = 0;
const SW_SHOW: i32 = 5;
const SW_RESTORE: i32 = 9;
/// The window class GPUI registers for its windows.
const GPUI_CLASS: &str = "Zed::Window";

/// A window handle stored as a plain number (a raw pointer can't be shared freely in
/// Rust; the number is only ever handed back to Windows).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct WindowId(isize);

impl WindowId {
    fn hwnd(self) -> Hwnd {
        self.0 as Hwnd
    }
}

extern "system" fn collect(hwnd: Hwnd, lparam: isize) -> i32 {
    // SAFETY: `lparam` is the `&mut Vec` passed by `gpui_windows` below, alive for the
    // whole (synchronous) EnumWindows call.
    let found = unsafe { &mut *(lparam as *mut Vec<WindowId>) };
    let mut pid = 0u32;
    let mut buf = [0u16; 64];
    // SAFETY: plain queries on a handle Windows just gave us; the buffer length is
    // passed correctly.
    let ours = unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
        pid == GetCurrentProcessId()
    };
    if ours {
        let n = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
        if n > 0 && String::from_utf16_lossy(&buf[..n as usize]) == GPUI_CLASS {
            found.push(WindowId(hwnd as isize));
        }
    }
    1 // keep enumerating
}

/// All GPUI windows of this process (Sotto has exactly one).
pub fn gpui_windows() -> Vec<WindowId> {
    let mut found: Vec<WindowId> = Vec::new();
    // SAFETY: the callback only uses `found` during this call.
    unsafe { EnumWindows(collect, &mut found as *mut Vec<WindowId> as isize) };
    found
}

pub fn exists(w: WindowId) -> bool {
    // SAFETY: IsWindow accepts any value and just answers yes/no.
    unsafe { IsWindow(w.hwnd()) != 0 }
}

pub fn hide(w: WindowId) {
    // SAFETY: our own window; SW_HIDE only changes its visibility.
    unsafe { ShowWindow(w.hwnd(), SW_HIDE) };
}

/// Show it again; if it was minimized before being closed, restore it too.
pub fn show(w: WindowId) {
    // SAFETY: our own window.
    unsafe {
        ShowWindow(w.hwnd(), SW_SHOW);
        if IsIconic(w.hwnd()) != 0 {
            ShowWindow(w.hwnd(), SW_RESTORE);
        }
        SetForegroundWindow(w.hwnd());
    }
}
