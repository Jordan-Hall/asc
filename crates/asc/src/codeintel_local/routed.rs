//! Offline routing (SPEC §23.2.5): the server answers when it can; when it cannot be reached,
//! the local index answers with `coverage` saying how much of the revision it holds.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use asc_rpc::server::wire;
use async_trait::async_trait;
use tonic::{Code, Status};

use crate::daemon::codeintel_overlay::ServerAnswer;

/// Whether `s` says the server could not be reached (as opposed to refusing the request).
pub fn is_offline(s: &Status) -> bool {
    matches!(s.code(), Code::Unavailable | Code::DeadlineExceeded)
}

/// A server with the local index behind it.
pub struct Routed {
    server: Arc<dyn ServerAnswer>,
    local: Arc<dyn ServerAnswer>,
    offline: AtomicBool,
}

impl Routed {
    pub fn new(server: Arc<dyn ServerAnswer>, local: Arc<dyn ServerAnswer>) -> Self {
        Self {
            server,
            local,
            offline: AtomicBool::new(false),
        }
    }

    /// The last server call found the server unreachable.
    pub fn offline(&self) -> bool {
        self.offline.load(Ordering::Relaxed)
    }

    fn note(&self, r: &Result<(), &Status>) -> bool {
        let off = r.is_err_and(is_offline);
        self.offline.store(off, Ordering::Relaxed);
        off
    }
}

#[async_trait]
impl ServerAnswer for Routed {
    async fn search(&self, req: wire::SearchRequest) -> Result<Vec<wire::SearchResponse>, Status> {
        let r = self.server.search(req.clone()).await;
        if self.note(&r.as_ref().map(|_| ())) {
            return self.local.search(req).await;
        }
        r
    }

    async fn symbols(&self, req: wire::SymbolsRequest) -> Result<wire::SymbolsResponse, Status> {
        let r = self.server.symbols(req.clone()).await;
        if self.note(&r.as_ref().map(|_| ())) {
            return self.local.symbols(req).await;
        }
        r
    }

    async fn semantic(&self, req: wire::SemanticRequest) -> Result<wire::SemanticResponse, Status> {
        let r = self.server.semantic(req.clone()).await;
        if self.note(&r.as_ref().map(|_| ())) {
            return self.local.semantic(req).await;
        }
        r
    }

    async fn index_status(
        &self,
        req: wire::IndexStatusRequest,
    ) -> Result<wire::IndexStatusResponse, Status> {
        let r = self.server.index_status(req.clone()).await;
        if self.note(&r.as_ref().map(|_| ())) {
            return self.local.index_status(req).await;
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Down;
    struct Refuses;
    struct Marks(&'static str);

    macro_rules! answer {
        ($t:ty, $err:expr) => {
            #[async_trait]
            impl ServerAnswer for $t {
                async fn search(
                    &self,
                    _: wire::SearchRequest,
                ) -> Result<Vec<wire::SearchResponse>, Status> {
                    Err($err)
                }
                async fn symbols(
                    &self,
                    _: wire::SymbolsRequest,
                ) -> Result<wire::SymbolsResponse, Status> {
                    Err($err)
                }
                async fn semantic(
                    &self,
                    _: wire::SemanticRequest,
                ) -> Result<wire::SemanticResponse, Status> {
                    Err($err)
                }
                async fn index_status(
                    &self,
                    _: wire::IndexStatusRequest,
                ) -> Result<wire::IndexStatusResponse, Status> {
                    Err($err)
                }
            }
        };
    }
    answer!(Down, Status::unavailable("offline"));
    answer!(Refuses, Status::permission_denied("no"));

    #[async_trait]
    impl ServerAnswer for Marks {
        async fn search(
            &self,
            _: wire::SearchRequest,
        ) -> Result<Vec<wire::SearchResponse>, Status> {
            Ok(vec![wire::SearchResponse {
                next_page_token: self.0.into(),
                ..wire::SearchResponse::default()
            }])
        }
        async fn symbols(&self, _: wire::SymbolsRequest) -> Result<wire::SymbolsResponse, Status> {
            Ok(wire::SymbolsResponse::default())
        }
        async fn semantic(
            &self,
            _: wire::SemanticRequest,
        ) -> Result<wire::SemanticResponse, Status> {
            Ok(wire::SemanticResponse::default())
        }
        async fn index_status(
            &self,
            _: wire::IndexStatusRequest,
        ) -> Result<wire::IndexStatusResponse, Status> {
            Ok(wire::IndexStatusResponse::default())
        }
    }

    #[tokio::test]
    async fn an_unreachable_server_falls_back_to_the_local_index() {
        let r = Routed::new(Arc::new(Down), Arc::new(Marks("local")));
        let out = r.search(wire::SearchRequest::default()).await.unwrap();
        assert_eq!(out[0].next_page_token, "local");
        assert!(r.offline());
    }

    #[tokio::test]
    async fn a_refusal_is_not_offline_and_a_live_server_wins() {
        let r = Routed::new(Arc::new(Refuses), Arc::new(Marks("local")));
        let e = r.search(wire::SearchRequest::default()).await.unwrap_err();
        assert_eq!(e.code(), Code::PermissionDenied);
        assert!(!r.offline());
        let r = Routed::new(Arc::new(Marks("server")), Arc::new(Marks("local")));
        let out = r.search(wire::SearchRequest::default()).await.unwrap();
        assert_eq!(out[0].next_page_token, "server");
    }
}
