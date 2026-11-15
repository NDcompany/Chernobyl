//! Phase 8: backup + restore of the companion's durable state.
//!
//! The durable state is a single append-only journal (`companion.journal`).
//! Backup copies it to a target path and writes a `<name>.sha256` sidecar;
//! restore verifies that sidecar before copying back, so a truncated or
//! corrupted backup is refused rather than silently promoted to live state.
//! The sidecar is an unkeyed checksum stored beside the backup: it detects
//! corruption, not deliberate edits. Whoever can rewrite the backup can
//! rewrite the sidecar, so keep backups in a folder only you can write
//! (security review 2026-10-25). Restore never un-revokes a device: every
//! revocation in the journal it replaces is re-applied to the restored one.
//! Key rotation is treated the same way (security pass 2026-11-15): for a
//! device present in both, the LIVE keys win, so a restore can never bring
//! back a pre-rotation key (the one in a QR that may have been photographed).
//!
//! Honest limitation (documented, not hidden): PSKs inside the journal are
//! DPAPI-bound to this machine + user. A restore onto a DIFFERENT machine will
//! not decrypt pairing keys — devices must re-pair there. Backup/restore is a
//! same-machine durability aid, not a portable secrets export.

use crate::crypto;
use crate::storage::{self, Storage};
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
/// `<data_dir>/companion.journal`. Refuses on any checksum mismatch, and while
/// a companion holds the journal. Returns the hex SHA-256 and the ids of the
/// devices whose revocation was re-applied.
pub fn restore(src: &Path, data_dir: &Path) -> Result<(String, Vec<String>), String> {
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
    // A running companion keeps appending to the journal it opened and
    // compacts its own state back over the restore (security review
    // 2026-10-25).
    let _lock = storage::lock_data_dir(data_dir)?;
    // Revocations and key rotations are monotonic: a device revoked since the
    // backup was taken stays revoked, and keeps its live (rotated) keys, or the
    // restore would re-admit a lost phone / revive a leaked pre-rotation key.
    let live: Vec<storage::DeviceRecord> = Storage::open_read_only(data_dir)?
        .devices
        .into_values()
        .collect();
    // Staged beside the journal and swapped in with one rename, so a failure
    // part-way (a revocation not yet re-applied) leaves the live journal as is.
    let stage = data_dir.join("restore.tmp");
    let _ = fs::remove_dir_all(&stage);
    let staged = stage_restore(&stage, &bytes, &live).and_then(|reapplied| {
        fs::rename(stage.join(JOURNAL_NAME), data_dir.join(JOURNAL_NAME))
            .map_err(|e| format!("write journal: {e}"))?;
        Ok(reapplied)
    });
    let _ = fs::remove_dir_all(&stage);
    Ok((digest, staged?))
}

/// Writes the backup into `stage` and re-applies the live devices' revocation
/// and keys to it; returns the ids whose revocation the backup had lost.
fn stage_restore(
    stage: &Path,
    bytes: &[u8],
    live: &[storage::DeviceRecord],
) -> Result<Vec<String>, String> {
    fs::create_dir_all(stage).map_err(|e| format!("create staging dir: {e}"))?;
    fs::write(stage.join(JOURNAL_NAME), bytes).map_err(|e| format!("write journal: {e}"))?;
    let mut reapplied = Vec::new();
    if live.is_empty() {
        return Ok(reapplied);
    }
    // a full open drops a torn tail first, so the records land on a clean
    // line; no pruning here, retention is the next boot's job
    let mut store = Storage::open(stage, usize::MAX, usize::MAX)?;
    for l in live {
        let id = &l.device_id;
        let Some(backed_up) = store.devices.get(id).cloned() else {
            continue;
        };
        let mut d = backed_up.clone();
        d.revoked |= l.revoked;
        d.psk_protected = l.psk_protected.clone();
        d.psk_next_protected = l.psk_next_protected.clone();
        if d == backed_up {
            continue;
        }
        store.put_device(&d)?;
        if d.revoked && !backed_up.revoked {
            let _ = store.append_audit(
                "revoke",
                &format!("device {id} revocation re-applied after restore"),
            );
            reapplied.push(id.clone());
        }
        if d.psk_protected != backed_up.psk_protected
            || d.psk_next_protected != backed_up.psk_next_protected
        {
            let _ = store.append_audit(
                "restore",
                &format!("device {id} kept its live keys after restore"),
            );
        }
    }
    Ok(reapplied)
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
        let (d2, reapplied) = restore(&bak, &store).unwrap();
        assert_eq!(d1, d2, "checksum must match on a clean round-trip");
        assert!(reapplied.is_empty());
        let restored = fs::read(store.join(JOURNAL_NAME)).unwrap();
        assert!(String::from_utf8_lossy(&restored).contains("hostId"));
        assert_eq!(
            restored,
            fs::read(&bak).unwrap(),
            "nothing to re-apply: exact bytes"
        );
        let _ = fs::remove_dir_all(&data);
        let _ = fs::remove_dir_all(&store);
    }

    /// Security review 2026-10-25: restoring a backup taken before a phone was
    /// revoked must not re-admit that phone.
    #[test]
    fn restore_keeps_revocations_made_after_the_backup() {
        use crate::storage::DeviceRecord;
        let data = temp_dir("revoked");
        let device = |id: &str, revoked| DeviceRecord {
            device_id: id.into(),
            name: "phone".into(),
            psk_protected: "P".into(),
            psk_next_protected: None,
            paired_at: 1,
            revoked,
        };
        {
            let mut s = Storage::open(&data, 500, 500).unwrap();
            s.put_device(&device("lost", false)).unwrap();
            s.put_device(&device("kept", false)).unwrap();
        }
        let bak = data.join("snap.journal");
        backup(&data, &bak).unwrap();
        {
            let mut s = Storage::open(&data, 500, 500).unwrap();
            s.put_device(&device("lost", true)).unwrap();
        }

        let (_, reapplied) = restore(&bak, &data).unwrap();
        assert_eq!(reapplied, vec!["lost".to_string()]);
        let s = Storage::open_read_only(&data).unwrap();
        assert!(s.devices["lost"].revoked, "a restore must never un-revoke");
        assert!(
            !s.devices["kept"].revoked,
            "other devices come back as backed up"
        );
        assert!(
            s.audit
                .iter()
                .any(|a| a.text.contains("re-applied after restore"))
        );
        assert!(!data.join("restore.tmp").exists(), "staging dir removed");
        let _ = fs::remove_dir_all(&data);
    }

    /// Security pass 2026-11-15 (C2): a restore must not bring back a
    /// pre-rotation device key (the one a photographed QR still carries).
    #[test]
    fn restore_keeps_the_live_keys_of_a_rotated_device() {
        use crate::storage::DeviceRecord;
        let data = temp_dir("rotated");
        let device = |id: &str, psk: &str, next: Option<&str>| DeviceRecord {
            device_id: id.into(),
            name: "phone".into(),
            psk_protected: psk.into(),
            psk_next_protected: next.map(str::to_string),
            paired_at: 1,
            revoked: false,
        };
        {
            let mut s = Storage::open(&data, 500, 500).unwrap();
            // the state at backup time: the QR key, a rotation not yet acked
            s.put_device(&device("phone", "QR-KEY", Some("ROTATED-KEY")))
                .unwrap();
            s.put_device(&device("old-only", "OLD-ONLY-KEY", None))
                .unwrap();
        }
        let bak = data.join("snap.journal");
        backup(&data, &bak).unwrap();
        {
            let mut s = Storage::open(&data, 500, 500).unwrap();
            // rotation confirmed since: the rotated key is the only key now
            s.put_device(&device("phone", "ROTATED-KEY", None)).unwrap();
            s.put_device(&device("new-only", "NEW-ONLY-KEY", None))
                .unwrap();
        }
        let (_, reapplied) = restore(&bak, &data).unwrap();
        assert!(reapplied.is_empty());
        let s = Storage::open_read_only(&data).unwrap();
        let p = &s.devices["phone"];
        assert_eq!(p.psk_protected, "ROTATED-KEY", "QR key must not return");
        assert_eq!(p.psk_next_protected, None);
        // a device only the backup knows comes back as backed up
        assert_eq!(s.devices["old-only"].psk_protected, "OLD-ONLY-KEY");
        assert!(
            s.audit
                .iter()
                .any(|a| a.text.contains("kept its live keys"))
        );
        let _ = fs::remove_dir_all(&data);
    }

    #[cfg(windows)]
    #[test]
    fn restore_is_refused_while_a_companion_holds_the_journal() {
        let data = temp_dir("locked");
        let journal = data.join(JOURNAL_NAME);
        fs::write(
            &journal,
            "{\"kind\":\"kv\",\"k\":\"hostId\",\"v\":\"old\"}\n",
        )
        .unwrap();
        let bak = data.join("snap.journal");
        backup(&data, &bak).unwrap();
        let live = "{\"kind\":\"kv\",\"k\":\"hostId\",\"v\":\"live\"}\n";
        fs::write(&journal, live).unwrap();

        let held = storage::lock_data_dir(&data).unwrap();
        let err = restore(&bak, &data).unwrap_err();
        assert!(err.contains("already running"), "got: {err}");
        assert_eq!(
            fs::read_to_string(&journal).unwrap(),
            live,
            "journal untouched"
        );
        drop(held);
        restore(&bak, &data).unwrap();
        assert!(fs::read_to_string(&journal).unwrap().contains("old"));
        let _ = fs::remove_dir_all(&data);
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
