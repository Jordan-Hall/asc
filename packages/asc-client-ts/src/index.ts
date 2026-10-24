// @asc/asc-client: the bridge transport, typed calls, id helpers and the bridge messages. The other
// generated modules are subpath imports (`@asc/asc-client/workspace` ...) so a consumer bundles only
// the services it calls.
export * as bridge from "./gen/asc/v1/bridge_pb";
export { type CallParams, callStream, callUnary } from "./client";
export {
  type BridgeNode,
  type BridgeNodeOptions,
  type BridgeState,
  type BridgeTransport,
  BridgeError,
  bridgeNode,
  type CallOptions,
  Code,
  type ResumableOptions,
  resumable,
} from "./transport/bridge-node";
export { encodeFrame, FrameError, FrameReader, MAX_FRAME } from "./transport/frame";
export {
  changeIdFromString,
  changeIdToString,
  fromHex,
  isChangeId,
  shortChangeId,
  toHex,
} from "./ids";
export { type GrpcWebOptions, checkBaseUrl, grpcWeb } from "./transport/grpc-web";
export {
  type ForgejoReply,
  type ForgejoRequest,
  type HostPort,
  type HostReply,
  type HostRequest,
  type HostTransport,
  host,
} from "./transport/host";
// The relay (node-only, it checks paths) is the subpath `@asc/asc-client/relay`.
