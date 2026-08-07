//! End-to-end smoke test for the encrypted secrets backend with the real
//! machine-ID-derived key. Skipped silently if the host doesn't expose a
//! readable machine ID (e.g. some CI sandboxes).

use mailypoppins::secrets::{decrypt_blob, encrypt_blob, EncryptedFileBackend, SecretsBackend};
use std::fs;
use tempfile::tempdir;

#[test]
fn encrypted_backend_round_trip_with_real_machine_key() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("secrets.enc");

    // Open with the real machine key. If this errors, machine-uid is
    // unreadable on this host -- skip rather than fail.
    let backend = match EncryptedFileBackend::open(path.clone()) {
        Ok(b) => b,
        Err(_) => return,
    };

    backend.set("smtp-password-foo", "hunter2").unwrap();
    backend.set("imap-password-foo", "hunter3").unwrap();

    // File should exist with restrictive permissions.
    assert!(path.exists(), "secrets file not created");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secrets file should be chmod 0600");
    }

    // Reopen and verify both entries decrypt.
    let reopened = EncryptedFileBackend::open(path.clone()).unwrap();
    assert_eq!(reopened.get("smtp-password-foo").unwrap(), "hunter2");
    assert_eq!(reopened.get("imap-password-foo").unwrap(), "hunter3");

    // Delete one and verify the other survives.
    reopened.delete("smtp-password-foo").unwrap();
    assert!(reopened.get("smtp-password-foo").is_err());
    assert_eq!(reopened.get("imap-password-foo").unwrap(), "hunter3");
}

#[test]
fn encrypt_blob_round_trip_smoke() {
    // Used by oauth2.rs token cache.
    let pt = b"{\"access_token\":\"foo\",\"refresh_token\":\"bar\",\"expires_at\":42}";
    let ct = match encrypt_blob(pt) {
        Ok(c) => c,
        Err(_) => return, // machine-uid unavailable in this env
    };
    let back = decrypt_blob(&ct).unwrap();
    assert_eq!(back.as_slice(), pt);
}
