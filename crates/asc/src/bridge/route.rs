//! Routing of bridge calls (SPEC §24.2, IP-D2). Deny by default: a method is served only when it
//! is written `<service>/<Method>`, names a method of the built-in descriptor set, belongs to a
//! service on the allowlist below and takes a single request message. Everything else is
//! `UNIMPLEMENTED` or `PERMISSION_DENIED` before any byte reaches the daemon or a server.
//!
//! Requests and answers cross the bridge as opaque protobuf bytes, so the codec here copies
//! them and the bridge never needs the message types of the services it relays.
//!
//! TODO(M09): the SPEC asks for "the same `LocalBackend` routing as `asc mcp`". `asc mcp` is still
//! a stub, so this module routes by service name with the same three targets (daemon IPC, asc
//! server, in-process); replace `resolve` by the M09 backend when it lands.

use std::sync::OnceLock;

use bytes::{Buf, BufMut, Bytes};
use prost_reflect::{DescriptorPool, MethodDescriptor};
use tonic::Status;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};

/// The services whose calls go to the daemon over the IPC pipe.
const DAEMON_SERVICES: &[&str] = &["asc.v1.Workspace"];
/// The services whose calls go to the asc server of the workspace's remote.
const SERVER_SERVICES: &[&str] = &[
    "asc.v1.Repo",
    "asc.v1.Changes",
    "asc.v1.Projects",
    "asc.v1.Cloud",
    "asc.v1.CodeIntel",
    "asc.v1.Inbox",
    "asc.v1.Review",
    "asc.v1.Ci",
];
/// Daemon methods a plugin must never call: they stop or reconfigure the daemon for every client.
const DENIED: &[&str] = &["asc.v1.Workspace/Shutdown"];

/// The methods the bridge itself serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BridgeMethod {
    Hello,
    Run,
    Token,
    StoreToken,
    Events,
}

#[derive(Clone, Debug)]
pub enum Target {
    Bridge(BridgeMethod),
    Daemon(MethodDescriptor),
    Server(MethodDescriptor),
}

fn pool() -> &'static DescriptorPool {
    static POOL: OnceLock<DescriptorPool> = OnceLock::new();
    POOL.get_or_init(|| {
        DescriptorPool::decode(asc_rpc::DESCRIPTOR_SET)
            .expect("the built-in descriptor set decodes")
    })
}

/// Where `method` (`asc.v1.Workspace/Status`) goes, or why it goes nowhere.
pub fn resolve(method: &str) -> Result<Target, Status> {
    let unknown = || Status::unimplemented("unknown method");
    if method.len() > 128
        || !method
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | '_'))
    {
        return Err(unknown());
    }
    let (service, name) = method.split_once('/').ok_or_else(unknown)?;
    if service == "asc.v1.Bridge" {
        return match name {
            "Hello" => Ok(Target::Bridge(BridgeMethod::Hello)),
            "Run" => Ok(Target::Bridge(BridgeMethod::Run)),
            "Token" => Ok(Target::Bridge(BridgeMethod::Token)),
            "StoreToken" => Ok(Target::Bridge(BridgeMethod::StoreToken)),
            "Events" => Ok(Target::Bridge(BridgeMethod::Events)),
            _ => Err(unknown()),
        };
    }
    let daemon = DAEMON_SERVICES.contains(&service);
    if !daemon && !SERVER_SERVICES.contains(&service) {
        return Err(Status::permission_denied(
            "service is not served by the bridge",
        ));
    }
    if DENIED.contains(&method) {
        return Err(Status::permission_denied(
            "method is not served by the bridge",
        ));
    }
    let desc = pool()
        .get_service_by_name(service)
        .and_then(|s| s.methods().find(|m| m.name() == name))
        .ok_or_else(unknown)?;
    if desc.is_client_streaming() {
        return Err(Status::unimplemented(
            "client-streaming methods are not served by the bridge",
        ));
    }
    Ok(if daemon {
        Target::Daemon(desc)
    } else {
        Target::Server(desc)
    })
}

/// The HTTP/2 path of `method`.
pub fn path_of(method: &MethodDescriptor) -> String {
    format!("/{}/{}", method.parent_service().full_name(), method.name())
}

/// A codec that carries message bytes unchanged.
#[derive(Clone, Copy, Default)]
pub struct RawCodec;

pub struct RawEncoder;
pub struct RawDecoder;

impl Encoder for RawEncoder {
    type Item = Bytes;
    type Error = Status;
    fn encode(&mut self, item: Bytes, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        dst.put(item);
        Ok(())
    }
}

impl Decoder for RawDecoder {
    type Item = Bytes;
    type Error = Status;
    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Bytes>, Status> {
        Ok(Some(src.copy_to_bytes(src.remaining())))
    }
}

impl Codec for RawCodec {
    type Encode = Bytes;
    type Decode = Bytes;
    type Encoder = RawEncoder;
    type Decoder = RawDecoder;
    fn encoder(&mut self) -> RawEncoder {
        RawEncoder
    }
    fn decoder(&mut self) -> RawDecoder {
        RawDecoder
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(m: &str) -> tonic::Code {
        resolve(m).unwrap_err().code()
    }

    #[test]
    fn bridge_methods_resolve() {
        assert!(matches!(
            resolve("asc.v1.Bridge/Hello"),
            Ok(Target::Bridge(BridgeMethod::Hello))
        ));
        assert_eq!(code("asc.v1.Bridge/Nope"), tonic::Code::Unimplemented);
    }

    #[test]
    fn workspace_goes_to_the_daemon_and_repo_to_the_server() {
        assert!(matches!(
            resolve("asc.v1.Workspace/Status"),
            Ok(Target::Daemon(_))
        ));
        assert!(matches!(resolve("asc.v1.Repo/Log"), Ok(Target::Server(_))));
        assert!(matches!(
            resolve("asc.v1.Changes/Versions"),
            Ok(Target::Server(_))
        ));
    }

    #[test]
    fn inbox_review_and_ci_go_to_the_server() {
        for m in [
            "asc.v1.Inbox/List",
            "asc.v1.Inbox/Count",
            "asc.v1.Inbox/SetState",
            "asc.v1.Review/ListThreads",
            "asc.v1.Review/AddComment",
            "asc.v1.Review/ResolveThread",
            "asc.v1.Review/RequestReview",
            "asc.v1.Ci/ListRuns",
            "asc.v1.Ci/JobLog",
        ] {
            assert!(matches!(resolve(m), Ok(Target::Server(_))), "{m}");
        }
        assert_eq!(code("asc.v1.Inbox/Nope"), tonic::Code::Unimplemented);
    }

    #[test]
    fn everything_else_is_denied() {
        assert_eq!(
            code("asc.v1.Workspace/Shutdown"),
            tonic::Code::PermissionDenied
        );
        assert_eq!(
            code("grpc.reflection.v1.ServerReflection/X"),
            tonic::Code::PermissionDenied
        );
        assert_eq!(code("/asc.v1.Repo/Log"), tonic::Code::PermissionDenied);
        assert_eq!(code("asc.v1.Repo/Nope"), tonic::Code::Unimplemented);
        assert_eq!(code("asc.v1.Repo"), tonic::Code::Unimplemented);
        assert_eq!(code("asc.v1.Repo/Log\n"), tonic::Code::Unimplemented);
        assert_eq!(code(""), tonic::Code::Unimplemented);
    }

    #[test]
    fn client_streams_are_not_served() {
        assert_eq!(code("asc.v1.Repo/PutObjects"), tonic::Code::Unimplemented);
    }
}
