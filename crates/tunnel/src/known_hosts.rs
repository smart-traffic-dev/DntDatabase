//! App-owned `known_hosts` file (`%APPDATA%\DntDatabase\known_hosts`).
//! One line per trusted server: `[host]:port <algorithm> <base64 key>`, like OpenSSH.

use dnt_core::error::DbError;
use russh::keys::{HashAlg, PublicKey};
use std::io::{ErrorKind, Write};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyStatus {
    Known,
    Unknown { fingerprint: String },
    Mismatch { expected: String, actual: String },
}

#[derive(Debug, Clone)]
pub struct KnownHosts {
    path: PathBuf,
}

/// `SHA256:<base64, no padding>` of the public key wire blob, as `ssh-keygen -lf` prints it.
pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// `algorithm base64` without the comment.
pub(crate) fn openssh_line(key: &PublicKey) -> Result<String, DbError> {
    PublicKey::new(key.key_data().clone(), "")
        .to_openssh()
        .map_err(|e| DbError::Internal {
            message: format!("cannot encode SSH public key: {e}"),
        })
}

fn host_pattern(host: &str, port: u16) -> String {
    format!("[{}]:{port}", host.to_ascii_lowercase())
}

/// Host field and key of a well-formed entry line; `None` for comments, blanks and garbage.
fn parse_line(line: &str) -> Option<(&str, PublicKey)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (host, key) = line.split_once(char::is_whitespace)?;
    let key = PublicKey::from_openssh(key.trim()).ok()?;
    Some((host, key))
}

fn config_err(message: impl Into<String>) -> DbError {
    DbError::Config {
        field: Some("known_hosts".into()),
        message: message.into(),
    }
}

fn io_err(what: &str, e: std::io::Error) -> DbError {
    DbError::Internal {
        message: format!("cannot {what} known_hosts: {e}"),
    }
}

impl KnownHosts {
    pub fn new(path: PathBuf) -> Self {
        KnownHosts { path }
    }

    fn read(&self) -> Result<String, DbError> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(io_err("read", e)),
        }
    }

    pub fn check(&self, host: &str, port: u16, key: &PublicKey) -> Result<HostKeyStatus, DbError> {
        let pattern = host_pattern(host, port);
        let text = self.read()?;
        let mut expected = None;
        for (h, known) in text.lines().filter_map(parse_line) {
            if !h.eq_ignore_ascii_case(&pattern) {
                continue;
            }
            if known.key_data() == key.key_data() {
                return Ok(HostKeyStatus::Known);
            }
            expected.get_or_insert_with(|| fingerprint(&known));
        }
        let actual = fingerprint(key);
        Ok(match expected {
            Some(expected) => HostKeyStatus::Mismatch { expected, actual },
            None => HostKeyStatus::Unknown {
                fingerprint: actual,
            },
        })
    }

    /// Trusts `key_openssh` for `[host]:port`, replacing any key previously trusted for it.
    /// `fingerprint` must be the one the user confirmed; it is checked against the key.
    pub fn trust(
        &self,
        host: &str,
        port: u16,
        fingerprint: &str,
        key_openssh: &str,
    ) -> Result<(), DbError> {
        let key = PublicKey::from_openssh(key_openssh.trim())
            .map_err(|_| config_err("not an OpenSSH public key"))?;
        let actual = self::fingerprint(&key);
        if actual != fingerprint {
            return Err(config_err(format!(
                "host key fingerprint {actual} does not match the confirmed {fingerprint}"
            )));
        }
        let pattern = host_pattern(host, port);
        let mut out = String::new();
        for line in self.read()?.lines() {
            let same_host = parse_line(line).is_some_and(|(h, _)| h.eq_ignore_ascii_case(&pattern));
            if !same_host {
                out.push_str(line);
                out.push('\n');
            }
        }
        out.push_str(&format!("{pattern} {}\n", openssh_line(&key)?));
        self.write_atomic(&out)
    }

    fn write_atomic(&self, text: &str) -> Result<(), DbError> {
        let dir = match self.path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
            _ => PathBuf::from("."),
        };
        std::fs::create_dir_all(&dir).map_err(|e| io_err("create the directory of", e))?;
        let mut tmp = tempfile::NamedTempFile::new_in(&dir).map_err(|e| io_err("write", e))?;
        tmp.write_all(text.as_bytes())
            .and_then(|_| tmp.as_file().sync_all())
            .map_err(|e| io_err("write", e))?;
        tmp.persist(&self.path)
            .map_err(|e| io_err("replace", e.error))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMwX008TclzRzuzUBAIDTWbAZw6okF3Trf2MQclqRRio";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJheNArdFhHfCyiO0uUA49b+sUhgOfEffwQMiF7vGmUF";
    // C:\laragon\bin\git\usr\bin\ssh-keygen.exe -lf testdata/ssh/id_ed25519.pub
    const FP_A: &str = "SHA256:PoVA8MJE/3odGD9flBg93RTPDG+b2kboI4MfUWL0kqc";
    // C:\laragon\bin\git\usr\bin\ssh-keygen.exe -lf testdata/ssh/id_ed25519_enc.pub
    const FP_B: &str = "SHA256:mxhav4lgGoATV9Pc+ehb/E71ZXvdXmJ7sn87eWB4WIk";

    fn key(s: &str) -> PublicKey {
        PublicKey::from_openssh(s).unwrap()
    }

    fn store() -> (tempfile::TempDir, KnownHosts) {
        let dir = tempfile::tempdir().unwrap();
        // Parent directory does not exist yet: `trust` must create it.
        let kh = KnownHosts::new(dir.path().join("sub").join("known_hosts"));
        (dir, kh)
    }

    #[test]
    fn fingerprint_matches_ssh_keygen() {
        assert_eq!(fingerprint(&key(KEY_A)), FP_A);
        assert_eq!(fingerprint(&key(KEY_B)), FP_B);
    }

    #[test]
    fn missing_file_is_unknown() {
        let (_d, kh) = store();
        assert_eq!(
            kh.check("db.example", 22, &key(KEY_A)).unwrap(),
            HostKeyStatus::Unknown {
                fingerprint: FP_A.into()
            }
        );
    }

    #[test]
    fn trust_then_known_mismatch_and_other_port() {
        let (_d, kh) = store();
        kh.trust("db.example", 2222, FP_A, &format!("{KEY_A} some comment"))
            .unwrap();
        assert_eq!(
            kh.check("db.example", 2222, &key(KEY_A)).unwrap(),
            HostKeyStatus::Known
        );
        assert_eq!(
            kh.check("DB.Example", 2222, &key(KEY_A)).unwrap(),
            HostKeyStatus::Known
        );
        assert_eq!(
            kh.check("db.example", 2222, &key(KEY_B)).unwrap(),
            HostKeyStatus::Mismatch {
                expected: FP_A.into(),
                actual: FP_B.into()
            }
        );
        assert_eq!(
            kh.check("db.example", 22, &key(KEY_A)).unwrap(),
            HostKeyStatus::Unknown {
                fingerprint: FP_A.into()
            }
        );
        let text = std::fs::read_to_string(&kh.path).unwrap();
        assert_eq!(text, format!("[db.example]:2222 {KEY_A}\n"));
    }

    #[test]
    fn trust_replaces_line_for_same_host_and_keeps_others() {
        let (_d, kh) = store();
        kh.trust("a", 22, FP_A, KEY_A).unwrap();
        kh.trust("b", 22, FP_A, KEY_A).unwrap();
        kh.trust("a", 22, FP_B, KEY_B).unwrap();
        assert_eq!(
            kh.check("a", 22, &key(KEY_B)).unwrap(),
            HostKeyStatus::Known
        );
        assert!(matches!(
            kh.check("a", 22, &key(KEY_A)).unwrap(),
            HostKeyStatus::Mismatch { .. }
        ));
        assert_eq!(
            kh.check("b", 22, &key(KEY_A)).unwrap(),
            HostKeyStatus::Known
        );
        let text = std::fs::read_to_string(&kh.path).unwrap();
        assert_eq!(text, format!("[b]:22 {KEY_A}\n[a]:22 {KEY_B}\n"));
    }

    #[test]
    fn trust_rejects_fingerprint_that_does_not_match_key() {
        let (_d, kh) = store();
        let err = kh.trust("a", 22, FP_B, KEY_A).unwrap_err();
        assert!(matches!(err, DbError::Config { .. }), "{err:?}");
        let err = kh.trust("a", 22, FP_A, "not a key").unwrap_err();
        assert!(matches!(err, DbError::Config { .. }), "{err:?}");
        assert!(!kh.path.exists());
    }

    #[test]
    fn ignores_comments_blank_and_garbage_lines() {
        let (_d, kh) = store();
        std::fs::create_dir_all(kh.path.parent().unwrap()).unwrap();
        std::fs::write(
            &kh.path,
            format!("# comment\n\ngarbage\n[a]:22 ssh-ed25519 !!!\n[a]:22 {KEY_A}\n"),
        )
        .unwrap();
        assert_eq!(
            kh.check("a", 22, &key(KEY_A)).unwrap(),
            HostKeyStatus::Known
        );
        kh.trust("c", 22, FP_B, KEY_B).unwrap();
        let text = std::fs::read_to_string(&kh.path).unwrap();
        assert!(text.starts_with("# comment\n\ngarbage\n"), "{text}");
        assert!(text.ends_with(&format!("[c]:22 {KEY_B}\n")), "{text}");
    }
}
