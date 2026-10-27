//! Temporary scaffolds for the build-out services (docs/BUILDOUT.md, "Scaffolds").
//!
//! Unit 0 of the build-out lands the whole wire contract at once. Every new service gets a [`Scaffold`]
//! implementation here that answers each method `UNIMPLEMENTED`, so the workspace compiles while the
//! units that own the services write them. A unit that implements a service deletes its group from the
//! `scaffold!` call below and its lines from the scaffold list in `docs/BUILDOUT.md`; the two must agree
//! (`tests/scaffold_registry.rs`). Nothing registers a [`Scaffold`] on a listener: a method that is not
//! served is already `UNIMPLEMENTED` from the router, and this type exists to keep the trait impls
//! checked by the compiler until the real ones replace it. When the list is empty this module is deleted.

use async_trait::async_trait;
use futures::stream::Empty;
use tonic::metadata::MetadataValue;
use tonic::{Request, Response, Status};

use crate::pb;

/// The metadata key that names the method a scaffold refused, so a client can tell "this server does
/// not carry the feature yet" from any other `UNIMPLEMENTED`.
pub const UNIMPLEMENTED_HEADER: &str = "x-asc-unimplemented";

/// The status a scaffolded method answers: `UNIMPLEMENTED`, the method named in the message and in
/// [`UNIMPLEMENTED_HEADER`]. `rpc` is `asc.v1.Service/Method`.
pub fn not_available(rpc: &str) -> Status {
    let mut status = Status::unimplemented(format!("{rpc} is not available on this server"));
    if let Ok(v) = MetadataValue::try_from(rpc) {
        status.metadata_mut().insert(UNIMPLEMENTED_HEADER, v);
    }
    status
}

/// A response stream that is never produced: a scaffolded streaming method fails before it starts.
pub type ScaffoldStream<T> = Empty<Result<T, Status>>;

/// Answers every method of every scaffolded service `UNIMPLEMENTED`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Scaffold;

macro_rules! scaffold {
    (
        $(
            $module:ident :: $service:ident = $full:literal {
                unary { $( $uname:literal $ufn:ident($ureq:ident) -> $uresp:ident; )* }
                stream { $( $sname:literal $sfn:ident($sreq:ident) -> $sresp:ident as $sty:ident; )* }
            }
        )+
    ) => {
        $(
            #[async_trait]
            impl crate::services::$module::$service for Scaffold {
                $( type $sty = ScaffoldStream<pb::$sresp>; )*
                $(
                    async fn $ufn(
                        &self,
                        _request: Request<pb::$ureq>,
                    ) -> Result<Response<pb::$uresp>, Status> {
                        Err(not_available(concat!($full, "/", $uname)))
                    }
                )*
                $(
                    async fn $sfn(
                        &self,
                        _request: Request<pb::$sreq>,
                    ) -> Result<Response<Self::$sty>, Status> {
                        Err(not_available(concat!($full, "/", $sname)))
                    }
                )*
            }
        )+

        /// Every method this module scaffolds, as `asc.v1.Service/Method`.
        pub const SCAFFOLDED_RPCS: &[&str] = &[
            $(
                $( concat!($full, "/", $uname), )*
                $( concat!($full, "/", $sname), )*
            )+
        ];
    };
}

scaffold! {
    merge_queue_server::MergeQueue = "asc.v1.MergeQueue" {
        unary {
            "Enqueue" enqueue(EnqueueRequest) -> EnqueueResponse;
            "Dequeue" dequeue(DequeueRequest) -> DequeueResponse;
            "List" list(ListQueueRequest) -> ListQueueResponse;
            "Get" get(GetQueueEntryRequest) -> GetQueueEntryResponse;
            "Revalidate" revalidate(RevalidateRequest) -> RevalidateResponse;
            "SetPaused" set_paused(SetQueuePausedRequest) -> SetQueuePausedResponse;
        }
        stream {
            "Watch" watch(WatchQueueRequest) -> QueueEvent as WatchStream;
        }
    }
    cross_search_server::CrossSearch = "asc.v1.CrossSearch" {
        unary {
            "LookupSymbol" lookup_symbol(LookupSymbolRequest) -> LookupSymbolResponse;
            "SymbolPanel" symbol_panel(SymbolPanelRequest) -> SymbolPanelResponse;
        }
        stream {
            "Search" search(CrossSearchRequest) -> CrossSearchResponse as SearchStream;
        }
    }
}
