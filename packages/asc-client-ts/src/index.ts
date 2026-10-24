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
