//! Minimal Windows FFI — system DLLs only (kernel32/psapi/ws2_32), no crates.
//! Console ctrl handling for graceful shutdown; process memory for health;
//! one winsock call std doesn't expose (listener backlog).

#![allow(non_snake_case)]

use std::ffi::c_void;
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
    fn FreeConsole() -> i32;
}

/// Detach this process from its console window (Phase 8 windowless host). The
/// HKCU Run autostart used to flash a console window at login; calling this
/// after boot closes that window while the daemon keeps serving on its sockets.
/// Diagnostics remain reachable via the loopback `/health` + `/metrics` routes,
/// so detaching the console costs no observability. No-op if already detached.
pub fn detach_console() {
    unsafe {
        FreeConsole();
    }
}

#[link(name = "psapi")]
unsafe extern "system" {
    fn GetProcessMemoryInfo(
        process: usize,
        counters: *mut PROCESS_MEMORY_COUNTERS,
        size: u32,
    ) -> i32;
}

#[link(name = "ws2_32")]
unsafe extern "system" {
    fn WSAStartup(ver: u16, data: *mut u8) -> i32;
    fn WSAGetLastError() -> i32;
    fn socket(af: i32, ty: i32, proto: i32) -> u64;
    fn bind(s: u64, addr: *const u8, len: i32) -> i32;
    fn listen(s: u64, backlog: i32) -> i32;
    fn closesocket(s: u64) -> i32;
}

const AF_INET: i32 = 2;
const SOCK_STREAM: i32 = 1;
const IPPROTO_TCP: i32 = 6;
const INVALID_SOCKET: u64 = u64::MAX;
const SOMAXCONN: i32 = 0x7fffffff;

/// `bind` + `listen(SOMAXCONN)` with a real backlog. std's TcpListener::bind
/// exposes no backlog knob and effectively queues only ~128 handshakes: the
/// burst test measured 19 refused connects out of 200 simultaneous. A
/// reconnect storm (app + relay flaps) is exactly that shape. The socket is
/// built raw and adopted via `from_raw_socket`, so accept/shutdown behavior
/// downstream is unchanged. IPv4 only — callers fall back to std bind.
pub fn bind_listener_backlog(ipv4: &str, port: u16) -> Result<std::net::TcpListener, String> {
    // winsock init is process-wide and refcounted; any std net use does this
    // too, a second WSAStartup is harmless
    let mut wsa = [0u8; 512];
    unsafe {
        if WSAStartup(0x0202, wsa.as_mut_ptr()) != 0 {
            return Err("WSAStartup failed".into());
        }
    }
    let octets = ipv4
        .parse::<std::net::Ipv4Addr>()
        .map_err(|_| format!("backlog listener needs an IPv4 bind address, got '{ipv4}'"))?
        .octets();
    unsafe {
        let s = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
        if s == INVALID_SOCKET {
            return Err(format!("socket() failed: {}", WSAGetLastError()));
        }
        let mut addr = [0u8; 16]; // sockaddr_in: family(le), port(be), addr, zero[8]
        addr[0..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
        addr[2..4].copy_from_slice(&port.to_be_bytes());
        addr[4..8].copy_from_slice(&octets);
        if bind(s, addr.as_ptr(), addr.len() as i32) != 0 {
            let e = WSAGetLastError();
            closesocket(s);
            return Err(format!("bind() failed: {e}"));
        }
        if listen(s, SOMAXCONN) != 0 {
            let e = WSAGetLastError();
            closesocket(s);
            return Err(format!("listen() failed: {e}"));
        }
        use std::os::windows::io::FromRawSocket;
        Ok(std::net::TcpListener::from_raw_socket(
            s as std::os::windows::io::RawSocket,
        ))
    }
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

// --- Console input mode (Phase 8 terminal TUI) ---
// A keybind TUI needs one keystroke per read (no line buffering, no echo), and
// arrow keys as VT escape sequences. These helpers flip the console input mode
// and restore it on drop so the shell prompt is never left in raw mode.

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetStdHandle(n: u32) -> usize;
    fn GetConsoleMode(handle: usize, mode: *mut u32) -> i32;
    fn SetConsoleMode(handle: usize, mode: u32) -> i32;
}

const STD_INPUT_HANDLE: u32 = 0xFFFFFFF6; // (DWORD)-10
const INVALID_HANDLE_VALUE: usize = usize::MAX; // (HANDLE)-1
const ENABLE_LINE_INPUT: u32 = 0x0002;
const ENABLE_ECHO_INPUT: u32 = 0x0004;
const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;

/// Restores the saved console input mode when dropped.
pub struct ConsoleInputMode {
    handle: usize,
    saved: u32,
}

impl Drop for ConsoleInputMode {
    fn drop(&mut self) {
        unsafe {
            SetConsoleMode(self.handle, self.saved);
        }
    }
}

/// Puts the console input into raw, VT-translated mode so a TUI reads one
/// keystroke per read and arrow keys arrive as `\x1b[A`…`\x1b[D`. Returns a
/// guard that restores the previous mode on drop. Fails (without changing
/// anything) when stdin is not a console — e.g. piped/redirected — so callers
/// can fall back to line input instead of hanging.
pub fn enable_raw_console_input() -> Result<ConsoleInputMode, String> {
    unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        if handle == 0 || handle == INVALID_HANDLE_VALUE {
            return Err("stdin is not a console".into());
        }
        let mut saved = 0u32;
        if GetConsoleMode(handle, &mut saved) == 0 {
            return Err(format!(
                "GetConsoleMode failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let raw =
            (saved & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT)) | ENABLE_VIRTUAL_TERMINAL_INPUT;
        if SetConsoleMode(handle, raw) == 0 {
            return Err(format!(
                "SetConsoleMode failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(ConsoleInputMode { handle, saved })
    }
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

#[link(name = "bcrypt")]
unsafe extern "system" {
    fn BCryptGenRandom(alg: *mut u8, buf: *mut u8, len: u32, flags: u32) -> i32;
}
const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;

/// Cryptographically secure random bytes via the OS CSPRNG (BCryptGenRandom,
/// system-preferred RNG). Fails closed on error — never returns predictable
/// bytes. Used for PSKs, nonces, and pairing challenges (ADR-011).
pub fn csprng_fill(buf: &mut [u8]) -> Result<(), String> {
    if buf.is_empty() {
        return Ok(());
    }
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(format!("BCryptGenRandom failed: NTSTATUS {status:#x}"))
    }
}

/// Convenience: exactly `n` random bytes.
pub fn csprng_bytes(n: usize) -> Result<Vec<u8>, String> {
    let mut b = vec![0u8; n];
    csprng_fill(&mut b)?;
    Ok(b)
}

// --- DPAPI (at-rest hardening for stored PSKs, ADR-011) ---
// CryptProtectData/CryptUnprotectData bind secrets to the current Windows user
// so a stolen journal file cannot be replayed on another account/machine.

#[repr(C)]
struct DataBlob {
    cb_data: u32,
    pb_data: *mut u8,
}

const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;

#[link(name = "crypt32")]
unsafe extern "system" {
    fn CryptProtectData(
        data_in: *const DataBlob,
        descr: *const u16,
        entropy: *const DataBlob,
        reserved: *mut c_void,
        prompt: *mut c_void,
        flags: u32,
        data_out: *mut DataBlob,
    ) -> i32;
    fn CryptUnprotectData(
        data_in: *const DataBlob,
        descr: *mut *mut u16,
        entropy: *const DataBlob,
        reserved: *mut c_void,
        prompt: *mut c_void,
        flags: u32,
        data_out: *mut DataBlob,
    ) -> i32;
}

unsafe extern "system" {
    fn LocalFree(mem: *mut c_void) -> *mut c_void;
}

fn last_win_err() -> String {
    std::io::Error::last_os_error().to_string()
}

/// Encrypt `plaintext` to a blob only decryptable by the current user on this
/// machine. No UI prompt (headless daemon). Used to protect PSKs at rest.
pub fn dpapi_protect(plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let in_blob = DataBlob {
        cb_data: plaintext.len() as u32,
        pb_data: plaintext.as_ptr() as *mut u8,
    };
    let mut out = DataBlob {
        cb_data: 0,
        pb_data: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptProtectData(
            &in_blob,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    if ok == 0 {
        return Err(format!("CryptProtectData failed: {}", last_win_err()));
    }
    let res = unsafe { std::slice::from_raw_parts(out.pb_data, out.cb_data as usize) }.to_vec();
    unsafe { LocalFree(out.pb_data as *mut c_void) };
    Ok(res)
}

/// Decrypt a blob produced by [`dpapi_protect`]. Fails closed if the blob was
/// written by another user/machine or was tampered with.
pub fn dpapi_unprotect(cipher: &[u8]) -> Result<Vec<u8>, String> {
    let in_blob = DataBlob {
        cb_data: cipher.len() as u32,
        pb_data: cipher.as_ptr() as *mut u8,
    };
    let mut out = DataBlob {
        cb_data: 0,
        pb_data: std::ptr::null_mut(),
    };
    let mut descr: *mut u16 = std::ptr::null_mut();
    let ok = unsafe {
        CryptUnprotectData(
            &in_blob,
            &mut descr,
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    if ok == 0 {
        return Err(format!("CryptUnprotectData failed: {}", last_win_err()));
    }
    let res = unsafe { std::slice::from_raw_parts(out.pb_data, out.cb_data as usize) }.to_vec();
    unsafe { LocalFree(out.pb_data as *mut c_void) };
    if !descr.is_null() {
        unsafe { LocalFree(descr as *mut c_void) };
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backlog_listener_binds_ephemeral_port_and_refuses_non_ipv4() {
        let l = bind_listener_backlog("127.0.0.1", 0).unwrap();
        assert!(l.local_addr().unwrap().port() > 0);
        // IPv6 is the std-bind fallback path, not an error state for the server
        assert!(bind_listener_backlog("::1", 0).is_err());
    }

    #[test]
    fn csprng_fills_and_is_nondeterministic() {
        let a = csprng_bytes(32).unwrap();
        let b = csprng_bytes(32).unwrap();
        assert_eq!(a.len(), 32);
        // two independent 32-byte draws colliding would mean a broken RNG
        assert_ne!(a, b, "CSPRNG returned identical blocks");
        // not all-zero (a real RNG essentially never returns an all-zero block)
        assert!(a.iter().any(|&x| x != 0));
        // empty request is a no-op, not an error
        assert!(csprng_fill(&mut []).is_ok());
    }

    #[test]
    fn dpapi_round_trip_protects_and_restores() {
        let secret = b"the quick brown fox PSK 0123456789";
        let cipher = dpapi_protect(secret).unwrap();
        // ciphertext must differ from plaintext (actually encrypted)
        assert_ne!(cipher.as_slice(), secret.as_slice());
        // ...and round-trips back to the exact original
        assert_eq!(dpapi_unprotect(&cipher).unwrap(), secret);
        // tampered ciphertext must fail closed (no silent wrong plaintext)
        let mut bad = cipher.clone();
        let n = bad.len();
        bad[n - 1] ^= 0xff;
        assert!(dpapi_unprotect(&bad).is_err());
    }
}
