//! TLS settings as Command drivers (Mongo/Redis) apply them.

use crate::config::{SslConfig, SslMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandTls {
    Off,
    Unverified,
    VerifyCa,
    VerifyFull,
}

/// Spec §6.4: how a Command driver (Mongo/Redis) applies `SslConfig`. Those servers have no
/// "prefer": `Prefer` and `Require` both mean TLS without certificate checks.
pub fn command_tls(ssl: Option<&SslConfig>) -> CommandTls {
    match ssl.map(|s| s.mode) {
        None | Some(SslMode::Disable) => CommandTls::Off,
        Some(SslMode::Prefer | SslMode::Require) => CommandTls::Unverified,
        Some(SslMode::VerifyCa) => CommandTls::VerifyCa,
        Some(SslMode::VerifyFull) => CommandTls::VerifyFull,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_tls_maps_every_mode() {
        let ssl = |mode| SslConfig {
            version: 1,
            mode,
            ca_path: None,
            cert_path: None,
            key_path: None,
        };
        assert_eq!(command_tls(None), CommandTls::Off);
        for (mode, want) in [
            (SslMode::Disable, CommandTls::Off),
            (SslMode::Prefer, CommandTls::Unverified),
            (SslMode::Require, CommandTls::Unverified),
            (SslMode::VerifyCa, CommandTls::VerifyCa),
            (SslMode::VerifyFull, CommandTls::VerifyFull),
        ] {
            assert_eq!(command_tls(Some(&ssl(mode))), want, "{mode:?}");
        }
    }
}
