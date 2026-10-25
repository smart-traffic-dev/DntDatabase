//! File identity for plans (R19, R21): name, size and SHA-256, hashed in 1 MiB reads.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use specta::Type;

const CHUNK: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct FileFacts {
    /// File name without its directory.
    pub name: String,
    /// Bytes hashed (the file size at the time of hashing).
    #[specta(type = f64)]
    pub size: u64,
    /// Lower-case hex SHA-256 of the content.
    pub sha256: String,
}

/// Hashes the file in 1 MiB reads; memory stays at one chunk whatever the file size.
pub fn file_facts(path: &Path) -> io::Result<FileFacts> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut size = 0u64;
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(FileFacts {
        name,
        size,
        sha256: hex::encode(hasher.finalize()),
    })
}
