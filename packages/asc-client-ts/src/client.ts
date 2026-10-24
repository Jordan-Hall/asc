// Typed calls over a BridgeTransport: encode with the generated schemas, decode the replies.
import {
  type DescMethodServerStreaming,
  type DescMethodUnary,
  type DescMessage,
  fromBinary,
  type MessageInitShape,
  type MessageShape,
  create,
  toBinary,
} from "@bufbuild/protobuf";
import type { BridgeTransport } from "./transport/bridge-node";

export interface CallParams {
  root?: string;
  signal?: AbortSignal;
}

function path(m: { parent: { typeName: string }; name: string }): string {
  return `${m.parent.typeName}/${m.name}`;
}

function params(p: CallParams | undefined): { root?: string; signal?: AbortSignal } {
  return {
    ...(p?.root === undefined ? {} : { root: p.root }),
    ...(p?.signal === undefined ? {} : { signal: p.signal }),
  };
}

export async function callUnary<I extends DescMessage, O extends DescMessage>(
  t: BridgeTransport,
  method: DescMethodUnary<I, O>,
  init: MessageInitShape<I>,
  p?: CallParams,
): Promise<MessageShape<O>> {
  const request = toBinary(method.input, create(method.input, init));
  const reply = await t.unary({ method: path(method), request, ...params(p) });
  return fromBinary(method.output, reply);
}

export async function* callStream<I extends DescMessage, O extends DescMessage>(
  t: BridgeTransport,
  method: DescMethodServerStreaming<I, O>,
  init: MessageInitShape<I>,
  p?: CallParams,
): AsyncGenerator<MessageShape<O>> {
  const request = toBinary(method.input, create(method.input, init));
  for await (const reply of t.stream({ method: path(method), request, ...params(p) })) {
    yield fromBinary(method.output, reply);
  }
}
