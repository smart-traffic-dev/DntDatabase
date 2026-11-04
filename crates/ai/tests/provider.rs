//! `HttpFimClient` against a scripted local HTTP server.

mod support;

use dnt_ai::{AiError, Endpoint, FimClient, FimRequest, HttpFimClient};
use secrecy::SecretString;
use std::time::Duration;
use support::{FakeServer, Reply};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn client(base: &str, key: Option<&str>, timeout: Duration) -> HttpFimClient {
    let key = key.map(SecretString::from);
    HttpFimClient::new(Endpoint::parse(base, key, timeout).unwrap()).unwrap()
}

fn request() -> FimRequest {
    FimRequest::new(
        "coder",
        "SELECT id FROM users WHERE ".to_string(),
        "\nORDER BY id;".to_string(),
    )
}

async fn complete_with(reply: Reply) -> Result<dnt_ai::FimReply, AiError> {
    let server = FakeServer::start(reply).await;
    client(&server.base(), None, Duration::from_secs(5))
        .complete(&request())
        .await
}

#[tokio::test]
async fn sends_fim_body() {
    let server = FakeServer::start(Reply::json(200, &fixture("completion_basic.json"))).await;
    client(&server.base(), None, Duration::from_secs(5))
        .complete(&request())
        .await
        .unwrap();
    let rec = server.recorded();
    assert_eq!(rec.len(), 1);
    assert_eq!(rec[0].method, "POST");
    assert_eq!(rec[0].path, "/v1/completions");
    let body = rec[0].json();
    assert_eq!(body["model"], "coder");
    assert_eq!(body["prompt"], "SELECT id FROM users WHERE ");
    assert_eq!(body["suffix"], "\nORDER BY id;");
    assert_eq!(body["max_tokens"], 128);
    assert!((body["temperature"].as_f64().unwrap() - 0.2).abs() < 1e-6);
    assert_eq!(body["stop"], serde_json::json!(["\n\n"]));
    assert_eq!(body["stream"], false);
    assert!(rec[0]
        .header("user-agent")
        .unwrap()
        .starts_with("DntDatabase/"));
}

#[tokio::test]
async fn bearer_header_when_key() {
    let server = FakeServer::start(Reply::json(200, &fixture("completion_basic.json"))).await;
    client(&server.base(), Some("sk-test-123"), Duration::from_secs(5))
        .complete(&request())
        .await
        .unwrap();
    assert_eq!(
        server.recorded()[0].header("authorization"),
        Some("Bearer sk-test-123")
    );
}

#[tokio::test]
async fn no_key_no_auth_header() {
    for key in [None, Some(""), Some("  ")] {
        let server = FakeServer::start(Reply::json(200, &fixture("completion_basic.json"))).await;
        client(&server.base(), key, Duration::from_secs(5))
            .complete(&request())
            .await
            .unwrap();
        assert_eq!(
            server.recorded()[0].header("authorization"),
            None,
            "{key:?}"
        );
    }
}

#[tokio::test]
async fn parses_text_and_finish_reason() {
    let r = complete_with(Reply::json(200, &fixture("completion_basic.json")))
        .await
        .unwrap();
    assert_eq!(r.text, "active = 1");
    assert_eq!(r.finish_reason.as_deref(), Some("stop"));

    let r = complete_with(Reply::json(200, &fixture("completion_no_usage.json")))
        .await
        .unwrap();
    assert_eq!(r.text, "o.status = 'paid'");
    assert_eq!(r.finish_reason, None);

    let r = complete_with(Reply::json(200, &fixture("completion_finish_length.json")))
        .await
        .unwrap();
    assert_eq!(r.finish_reason.as_deref(), Some("length"));
}

#[tokio::test]
async fn empty_choices_is_invalid_response() {
    let e = complete_with(Reply::json(200, &fixture("completion_empty_choices.json")))
        .await
        .unwrap_err();
    assert!(matches!(e, AiError::InvalidResponse { .. }), "{e:?}");
}

#[tokio::test]
async fn non_json_is_invalid_response() {
    let e = complete_with(Reply::json(200, "<html>proxy login</html>"))
        .await
        .unwrap_err();
    assert!(matches!(e, AiError::InvalidResponse { .. }), "{e:?}");
}

#[tokio::test]
async fn maps_401_403() {
    for status in [401, 403] {
        let e = complete_with(Reply::json(status, r#"{"error":{"message":"no"}}"#))
            .await
            .unwrap_err();
        assert_eq!(e, AiError::Unauthorized { status });
    }
}

#[tokio::test]
async fn maps_429_retry_after_seconds() {
    let e = complete_with(Reply::Respond {
        status: 429,
        headers: vec![("Retry-After".into(), "17".into())],
        body: String::new(),
    })
    .await
    .unwrap_err();
    assert_eq!(
        e,
        AiError::RateLimited {
            retry_after: Some(Duration::from_secs(17))
        }
    );
}

#[tokio::test]
async fn maps_429_retry_after_date_to_none() {
    let e = complete_with(Reply::Respond {
        status: 429,
        headers: vec![("Retry-After".into(), "Wed, 21 Oct 2026 07:28:00 GMT".into())],
        body: String::new(),
    })
    .await
    .unwrap_err();
    assert_eq!(e, AiError::RateLimited { retry_after: None });
}

#[tokio::test]
async fn maps_suffix_400_to_fim_unsupported() {
    let e = complete_with(Reply::json(400, &fixture("error_openai_style.json")))
        .await
        .unwrap_err();
    assert_eq!(
        e,
        AiError::FimUnsupported {
            message: "Unrecognized request argument supplied: suffix".into()
        }
    );
}

#[tokio::test]
async fn maps_other_400_to_bad_request() {
    let e = complete_with(Reply::json(
        404,
        r#"{"error":{"message":"The model `nope` does not exist"}}"#,
    ))
    .await
    .unwrap_err();
    assert_eq!(
        e,
        AiError::BadRequest {
            status: 404,
            message: "The model `nope` does not exist".into()
        }
    );
}

#[tokio::test]
async fn maps_503_to_server() {
    let e = complete_with(Reply::json(503, "overloaded"))
        .await
        .unwrap_err();
    assert_eq!(e, AiError::Server { status: 503 });
}

#[tokio::test]
async fn redirect_is_not_followed() {
    let other = FakeServer::start(Reply::json(200, &fixture("completion_basic.json"))).await;
    let server = FakeServer::start(Reply::Respond {
        status: 302,
        headers: vec![(
            "Location".into(),
            format!("http://localhost:{}/v1/completions", other.addr.port()),
        )],
        body: String::new(),
    })
    .await;
    let e = client(&server.base(), Some("sk-secret"), Duration::from_secs(5))
        .complete(&request())
        .await
        .unwrap_err();
    assert_eq!(
        e,
        AiError::Redirected {
            host: "localhost".into()
        }
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(other.recorded().is_empty(), "the redirect was followed");
}

#[tokio::test]
async fn timeout_maps_to_timeout() {
    let server = FakeServer::start(Reply::Hang).await;
    let e = client(&server.base(), None, Duration::from_millis(300))
        .complete(&request())
        .await
        .unwrap_err();
    assert_eq!(e, AiError::Timeout);
}

#[tokio::test]
async fn drop_cancels_the_request() {
    let server = FakeServer::start(Reply::Hang).await;
    let c = client(&server.base(), None, Duration::from_secs(30));
    let req = request();
    let pending = tokio::time::timeout(Duration::from_millis(200), c.complete(&req)).await;
    assert!(pending.is_err(), "the request should still be pending");
    tokio::time::timeout(Duration::from_secs(1), server.closed.notified())
        .await
        .expect("dropping the future closes the connection");
}

#[tokio::test]
async fn connection_refused_is_network() {
    // Bind then drop: the port is (very likely) closed.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let e = client(
        &format!("http://127.0.0.1:{port}/v1"),
        Some("sk-secret"),
        Duration::from_secs(5),
    )
    .complete(&request())
    .await
    .unwrap_err();
    match e {
        AiError::Network { message } => assert!(!message.contains("sk-secret"), "{message}"),
        other => panic!("expected Network, got {other:?}"),
    }
}

#[tokio::test]
async fn error_message_is_truncated() {
    let long = format!(r#"{{"error":{{"message":"{}"}}}}"#, "x".repeat(10_000));
    let e = complete_with(Reply::json(422, &long)).await.unwrap_err();
    match e {
        AiError::BadRequest { status, message } => {
            assert_eq!(status, 422);
            assert!(message.chars().count() <= 300, "{}", message.len());
        }
        other => panic!("expected BadRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn lists_models_sorted_unique() {
    let server = FakeServer::start(Reply::json(200, &fixture("models.json"))).await;
    let models = client(&server.base(), None, Duration::from_secs(5))
        .list_models()
        .await
        .unwrap();
    assert_eq!(models, ["deepseek-coder", "qwen-coder"]);
    let rec = server.recorded();
    assert_eq!(
        (rec[0].method.as_str(), rec[0].path.as_str()),
        ("GET", "/v1/models")
    );
}

/// Sends the Settings → Test probe to a real endpoint. Run by hand:
/// `DNT_AI_BASE_URL=… DNT_AI_MODEL=… [DNT_AI_KEY=…] cargo test -p dnt-ai --test provider -- --ignored`
#[tokio::test]
#[ignore = "calls the endpoint in DNT_AI_BASE_URL"]
async fn real_endpoint_smoke() {
    let base = std::env::var("DNT_AI_BASE_URL").expect("DNT_AI_BASE_URL");
    let model = std::env::var("DNT_AI_MODEL").expect("DNT_AI_MODEL");
    let key = std::env::var("DNT_AI_KEY").ok();
    let c = client(&base, key.as_deref(), Duration::from_secs(20));
    let req = FimRequest::new(
        model,
        dnt_ai::post::PROBE_PROMPT.to_string(),
        dnt_ai::post::PROBE_SUFFIX.to_string(),
    );
    match c.complete(&req).await {
        Ok(reply) => println!(
            "ok in {:?}: {} chars, finish {:?}, verdict {:?}",
            reply.elapsed,
            reply.text.chars().count(),
            reply.finish_reason,
            dnt_ai::post::judge_probe(&reply.text)
        ),
        Err(e) => println!("error: {e}"),
    }
}
