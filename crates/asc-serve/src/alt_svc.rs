//! `Alt-Svc` for HTTP/3 (U89).
//!
//! asc prefers HTTP/3 where it is available. This server speaks h1 and h2 only, so it does not
//! serve QUIC itself; an h3-capable proxy or CDN in front of it does (Traefik and Caddy both
//! can). `http3_port` names the UDP port that proxy serves h3 on, and every response then carries
//! `Alt-Svc: h3=":<port>"; ma=86400` so clients can switch. Unset, no header is sent: this
//! server never advertises a transport nobody serves.

use axum::http::{HeaderValue, Response, header::ALT_SVC};
use tower::util::MapResponseLayer;

/// How long a client may remember the alternative service, in seconds.
const MAX_AGE_SECS: u32 = 86_400;

/// The `Alt-Svc` value for h3 on `port`.
pub fn header_value(port: u16) -> HeaderValue {
    HeaderValue::from_str(&format!("h3=\":{port}\"; ma={MAX_AGE_SECS}"))
        .expect("an ASCII value with a number is a valid header")
}

/// A layer that adds `Alt-Svc` to every response when `port` is set, and does nothing otherwise.
/// A response that already carries the header keeps its own.
pub fn layer<B>(
    port: Option<u16>,
) -> MapResponseLayer<impl Fn(Response<B>) -> Response<B> + Clone> {
    let value = port.map(header_value);
    MapResponseLayer::new(move |mut resp: Response<B>| {
        if let Some(v) = &value
            && !resp.headers().contains_key(ALT_SVC)
        {
            resp.headers_mut().insert(ALT_SVC, v.clone());
        }
        resp
    })
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::{Layer, ServiceExt, service_fn};

    use super::*;

    async fn send(port: Option<u16>, existing: Option<&'static str>) -> Response<Body> {
        let inner = service_fn(move |_req: Request<Body>| async move {
            let mut resp = Response::new(Body::empty());
            if let Some(v) = existing {
                resp.headers_mut()
                    .insert(ALT_SVC, HeaderValue::from_static(v));
            }
            Ok::<_, std::convert::Infallible>(resp)
        });
        layer(port)
            .layer(inner)
            .oneshot(Request::new(Body::empty()))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn advertises_h3_only_when_a_port_is_configured() {
        let resp = send(Some(443), None).await;
        assert_eq!(resp.headers()[ALT_SVC], "h3=\":443\"; ma=86400");
        assert!(send(None, None).await.headers().get(ALT_SVC).is_none());
        // A response's own header wins.
        let resp = send(Some(443), Some("clear")).await;
        assert_eq!(resp.headers()[ALT_SVC], "clear");
    }
}
