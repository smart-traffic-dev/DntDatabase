//! Where the AI client sends requests: a validated OpenAI-compatible base URL, the
//! optional API key and the request timeout.

use crate::AiError;
use reqwest::Url;
use secrecy::{ExposeSecret, SecretString};
use std::fmt;
use std::time::Duration;

/// Whole-request timeout when the settings give none.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Endpoint {
    /// http(s), with a host, no user info, query or fragment, no trailing `/`.
    base: Url,
    pub api_key: Option<SecretString>,
    /// Whole request, connect included.
    pub timeout: Duration,
}

impl Endpoint {
    /// Validates `base_url` (e.g. `https://api.example.com/v1`). An empty or blank key
    /// is `None`: no `Authorization` header is sent.
    pub fn parse(
        base_url: &str,
        api_key: Option<SecretString>,
        timeout: Duration,
    ) -> Result<Self, AiError> {
        let config = |message: &str| AiError::Config {
            message: message.to_string(),
        };
        let scheme_msg = "The base URL must start with http:// or https://.";
        let mut base = Url::parse(base_url.trim()).map_err(|_| config(scheme_msg))?;
        if !matches!(base.scheme(), "http" | "https") {
            return Err(config(scheme_msg));
        }
        if base.host_str().is_none_or(str::is_empty) {
            return Err(config("The base URL must name a host."));
        }
        if base.query().is_some() || base.fragment().is_some() {
            return Err(config("The base URL must not contain a query or fragment."));
        }
        if !base.username().is_empty() || base.password().is_some() {
            return Err(config(
                "The base URL must not contain a user name or password. Use the API key field.",
            ));
        }
        let path = base.path().trim_end_matches('/').to_string();
        base.set_path(&path);
        let api_key = api_key.filter(|k| !k.expose_secret().trim().is_empty());
        Ok(Endpoint {
            base,
            api_key,
            timeout,
        })
    }

    /// `{base}/{path}`: `url("completions")` of `https://h/v1` is `https://h/v1/completions`.
    pub fn url(&self, path: &str) -> Url {
        let mut url = self.base.clone();
        let joined = format!("{}/{}", self.base.path().trim_end_matches('/'), path);
        url.set_path(&joined);
        url
    }

    /// `scheme://host[:port]` (port only when not the scheme's default): what the
    /// per-connection consent is tied to.
    pub fn origin(&self) -> String {
        self.base.origin().ascii_serialization()
    }

    /// Host of the base URL, for messages.
    pub fn host(&self) -> &str {
        self.base.host_str().unwrap_or_default()
    }
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint")
            .field("base", &self.base.as_str())
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("timeout", &self.timeout)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(url: &str) -> Result<Endpoint, AiError> {
        Endpoint::parse(url, None, DEFAULT_TIMEOUT)
    }

    fn config_message(r: Result<Endpoint, AiError>) -> String {
        match r {
            Err(AiError::Config { message }) => message,
            other => panic!("expected a Config error, got {other:?}"),
        }
    }

    #[test]
    fn parse_strips_trailing_slash() {
        let e = ep(" https://api.example.com/v1/ ").unwrap();
        assert_eq!(
            e.url("completions").as_str(),
            "https://api.example.com/v1/completions"
        );
        assert_eq!(
            e.url("models").as_str(),
            "https://api.example.com/v1/models"
        );
        let root = ep("http://localhost:8000").unwrap();
        assert_eq!(
            root.url("completions").as_str(),
            "http://localhost:8000/completions"
        );
    }

    #[test]
    fn parse_rejects_bad_scheme() {
        for bad in ["ftp://x", "api.example.com", "", "file:///c:/x"] {
            assert_eq!(
                config_message(ep(bad)),
                "The base URL must start with http:// or https://.",
                "{bad:?}"
            );
        }
    }

    #[test]
    fn parse_rejects_query() {
        for bad in ["https://a.b/v1?x=1", "https://a.b/v1#top"] {
            assert_eq!(
                config_message(ep(bad)),
                "The base URL must not contain a query or fragment.",
                "{bad:?}"
            );
        }
    }

    #[test]
    fn parse_rejects_user_info() {
        assert!(config_message(ep("https://u:p@a.b/v1")).contains("user name or password"));
    }

    #[test]
    fn parse_empty_key_is_none() {
        for key in ["", "   "] {
            let e = Endpoint::parse(
                "https://a.b/v1",
                Some(SecretString::from(key)),
                DEFAULT_TIMEOUT,
            )
            .unwrap();
            assert!(e.api_key.is_none(), "{key:?}");
        }
    }

    #[test]
    fn origin_has_port_only_when_set() {
        assert_eq!(
            ep("http://localhost:8000/v1").unwrap().origin(),
            "http://localhost:8000"
        );
        assert_eq!(ep("https://a.b/v1").unwrap().origin(), "https://a.b");
        assert_eq!(ep("https://a.b:443/v1").unwrap().origin(), "https://a.b");
    }

    #[test]
    fn endpoint_debug_hides_key() {
        let e = Endpoint::parse(
            "https://a.b/v1",
            Some(SecretString::from("sk-very-secret")),
            DEFAULT_TIMEOUT,
        )
        .unwrap();
        let dbg = format!("{e:?}");
        assert!(!dbg.contains("sk-very-secret"), "{dbg}");
        assert!(dbg.contains("***"), "{dbg}");
    }
}
