//! Phase 8: backup + restore of the companion's durable state.
//!
//! The durable state is a single append-only journal (`companion.journal`).
//! Backup copies it to a target path and writes a `<name>.sha256` sidecar;
//! restore verifies that sidecar before copying back, so a truncated or
//! tampered backup is refused rather than silently promoted to live state.
//!
//! Honest limitation (documented, not hidden): PSKs inside the journal are
//! DPAPI-bound to this machine + user. A restore onto a DIFFERENT machine will
//! not decrypt pairing keys — devices must re-pair there. Backup/restore is a
//! same-machine durability aid, not a portable secrets export.

use crate::crypto;
use std::fs;
use std::path::Path;

pub const JOURNAL_NAME: &str = "companion.journal";

/// Copy `<data_dir>/companion.journal` to `dest`, writing a `.sha256` sidecar.
/// Returns the hex SHA-256 of the backed-up bytes.
pub fn backup(data_dir: &Path, dest: &Path) -> Result<String, String> {
    let src = data_dir.join(JOURNAL_NAME);
    let bytes = fs::read(&src).map_err(|e| format!("read journal: {e}"))?;
    let digest = hex(&crypto::sha256(&bytes));
    fs::write(dest, &bytes).map_err(|e| format!("write backup: {e}"))?;
    let name = dest.file_name().unwrap_or_default().to_string_lossy();
    let manifest = format!("{digest}  {name}\n");
    let sidecar = sidecar_path(dest);
    fs::write(&sidecar, manifest).map_err(|e| format!("write manifest: {e}"))?;
    Ok(digest)
}

/// Verify `src` against its `.sha256` sidecar, then copy it to
/// `<data_dir>/companion.journal`. Refuses on any checksum mismatch.
pub fn restore(src: &Path, data_dir: &Path) -> Result<String, String> {
    let bytes = fs::read(src).map_err(|e| format!("read backup: {e}"))?;
    let digest = hex(&crypto::sha256(&bytes));
    let sidecar = sidecar_path(src);
    let manifest = fs::read_to_string(&sidecar).map_err(|e| format!("read manifest: {e}"))?;
    let expected = manifest.split_whitespace().next().unwrap_or("");
    if expected.is_empty() {
        return Err("manifest is empty".into());
    }
    if expected != digest {
        return Err(format!(
            "checksum mismatch: manifest {expected}, file {digest} — refusing to restore"
        ));
    }
    fs::create_dir_all(data_dir).map_err(|e| format!("create data dir: {e}"))?;
    let dest = data_dir.join(JOURNAL_NAME);
    fs::write(&dest, &bytes).map_err(|e| format!("write journal: {e}"))?;
    Ok(digest)
}

fn sidecar_path(p: &Path) -> std::path::PathBuf {
    let mut os = p.as_os_str().to_os_string();
    os.push(".sha256");
    std::path::PathBuf::from(os)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chernobyl-backup-{tag}-{}-{}",
            std::process::id(),
            crate::protocol::now_ms()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trip_restores_original_bytes() {
        let data = temp_dir("rt");
        let store = temp_dir("rt-store");
        // a journal with a couple of records
        fs::write(
            data.join(JOURNAL_NAME),
            "{\"kind\":\"kv\",\"k\":\"hostId\",\"v\":\"abc\"}\n{\"kind\":\"meta\",\"schema\":1}\n",
        )
        .unwrap();
        let bak = data.join("snap.journal");
        let d1 = backup(&data, &bak).unwrap();
        // clobber the live journal, then restore
        fs::write(data.join(JOURNAL_NAME), b"garbage").unwrap();
        let d2 = restore(&bak, &store).unwrap();
        assert_eq!(d1, d2, "checksum must match on a clean round-trip");
        let restored = fs::read(store.join(JOURNAL_NAME)).unwrap();
        assert!(String::from_utf8_lossy(&restored).contains("hostId"));
        let _ = fs::remove_dir_all(&data);
        let _ = fs::remove_dir_all(&store);
    }

    #[test]
    fn tampered_backup_is_refused() {
        let data = temp_dir("tamper");
        let store = temp_dir("tamper-store");
        fs::write(
            data.join(JOURNAL_NAME),
            "{\"kind\":\"kv\",\"k\":\"hostId\",\"v\":\"abc\"}\n",
        )
        .unwrap();
        let bak = data.join("snap.journal");
        backup(&data, &bak).unwrap();
        // flip a byte in the backup payload
        let mut bytes = fs::read(&bak).unwrap();
        bytes[0] ^= 0x01;
        fs::write(&bak, &bytes).unwrap();
        let err = restore(&bak, &store).unwrap_err();
        assert!(err.contains("checksum mismatch"), "got: {err}");
        // nothing was promoted to live state
        assert!(!store.join(JOURNAL_NAME).exists());
        let _ = fs::remove_dir_all(&data);
        let _ = fs::remove_dir_all(&store);
    }
}
