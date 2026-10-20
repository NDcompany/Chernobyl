//! Minimal Windows FFI — system DLLs only (kernel32/psapi), no crates.
//! Console ctrl handling for graceful shutdown; process memory for health.

#![allow(non_snake_case)]

use std::sync::atomic::{AtomicBool, Ordering};

pub static SHUTDOWN: AtomicBool = AtomicBool::new(false);

const CTRL_C_EVENT: u32 = 0;
const CTRL_BREAK_EVENT: u32 = 1;
const CTRL_CLOSE_EVENT: u32 = 2;
const CTRL_LOGOFF_EVENT: u32 = 5;
const CTRL_SHUTDOWN_EVENT: u32 = 6;

type HandlerRoutine = extern "system" fn(u32) -> i32;

unsafe extern "system" {
    fn SetConsoleCtrlHandler(handler: Option<HandlerRoutine>, add: i32) -> i32;
    fn GetCurrentProcess() -> usize;
}

#[link(name = "psapi")]
unsafe extern "system" {
    fn GetProcessMemoryInfo(
        process: usize,
        counters: *mut PROCESS_MEMORY_COUNTERS,
        size: u32,
    ) -> i32;
}

#[repr(C)]
#[derive(Default)]
struct PROCESS_MEMORY_COUNTERS {
    cb: u32,
    PageFaultCount: u32,
    PeakWorkingSetSize: usize,
    WorkingSetSize: usize,
    QuotaPeakPagedPoolUsage: usize,
    QuotaPagedPoolUsage: usize,
    QuotaPeakNonPagedPoolUsage: usize,
    QuotaNonPagedPoolUsage: usize,
    PagefileUsage: usize,
    PeakPagefileUsage: usize,
}

extern "system" fn on_ctrl_event(kind: u32) -> i32 {
    if matches!(
        kind,
        CTRL_C_EVENT
            | CTRL_BREAK_EVENT
            | CTRL_CLOSE_EVENT
            | CTRL_LOGOFF_EVENT
            | CTRL_SHUTDOWN_EVENT
    ) {
        SHUTDOWN.store(true, Ordering::SeqCst);
        1 // handled: give us time to flush and exit cleanly
    } else {
        0
    }
}

/// Registers the console ctrl handler. Idempotent.
pub fn install_shutdown_handler() {
    unsafe {
        SetConsoleCtrlHandler(Some(on_ctrl_event), 1);
    }
}

pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

/// Resident set size in bytes (working set).
pub fn rss_bytes() -> u64 {
    unsafe {
        let mut counters = PROCESS_MEMORY_COUNTERS {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        if GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ) != 0
        {
            counters.WorkingSetSize as u64
        } else {
            0
        }
    }
}
