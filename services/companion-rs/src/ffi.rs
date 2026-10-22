//! Minimal Windows FFI — system DLLs only (kernel32/psapi/ws2_32), no crates.
//! Console ctrl handling for graceful shutdown; process memory for health;
//! one winsock call std doesn't expose (listener backlog).

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
}
