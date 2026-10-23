//! U89: the RPC client talks https only. Plain http, loopback included, needs the explicit
//! opt-in (`asc.allowLoopbackHttp` / `ASC_ALLOW_LOOPBACK_HTTP=1`). This binary never sets the
//! environment variable, and it flips the config flag in one test, so the order is fixed.

use asc_core::transport_policy::set_config_allow_loopback_http;
use asc_rpc::auth::BearerToken;
use asc_rpc::client::{ClientError, RpcClient};

fn connect(url: &str) -> Result<RpcClient, ClientError> {
    RpcClient::connect(url, "owner/repo", None::<BearerToken>)
}

#[tokio::test]
async fn plain_http_needs_the_opt_in_and_loopback() {
    if std::env::var_os("ASC_ALLOW_LOOPBACK_HTTP").is_some() {
        return;
    }
    set_config_allow_loopback_http(false);
    for url in [
        "http://127.0.0.1:1",
        "http://localhost:1",
        "http://[::1]:1",
        "http://example.com",
    ] {
        assert!(connect(url).is_err(), "{url}");
        let https = connect("https://primary.example").unwrap();
        assert!(https.with_object_tiers(&[url]).is_err(), "tier {url}");
    }
    assert!(connect("https://primary.example").is_ok());
    // The refusal names the origin and keeps the credentials out.
    let err = connect("http://user:hunter2@example.com:8080/o/r?t=abc")
        .err()
        .unwrap()
        .to_string();
    assert!(err.contains("http://example.com:8080"), "{err}");
    for secret in ["hunter2", "user", "abc"] {
        assert!(!err.contains(secret), "{err}");
    }

    set_config_allow_loopback_http(true);
    assert!(connect("http://127.0.0.1:1").is_ok());
    assert!(connect("http://localhost:1").is_ok());
    assert!(connect("http://[::1]:1").is_ok());
    // The opt-in is for the local machine only.
    assert!(connect("http://example.com").is_err());
    assert!(connect("http://127.example.com").is_err());
    let https = connect("https://primary.example").unwrap();
    assert!(https.with_object_tiers(&["http://127.0.0.1:2"]).is_ok());
    assert!(https.with_object_tiers(&["http://example.com"]).is_err());
}
