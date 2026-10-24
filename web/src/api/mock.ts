// A BridgeTransport that answers from handlers. Requests are decoded and replies encoded with the
// real generated schemas, so the screens and the api modules run their production encode/decode
// path in tests and in the `?mock` development mode.
import {
  create,
  type DescMessage,
  type DescMethodServerStreaming,
  type DescMethodUnary,
  fromBinary,
  type MessageInitShape,
  type MessageShape,
  toBinary,
} from "@bufbuild/protobuf";
import {
  BridgeError,
  type BridgeState,
  type BridgeTransport,
  type CallOptions,
} from "@asc/asc-client";

function path(m: { parent: { typeName: string }; name: string }): string {
  return `${m.parent.typeName}/${m.name}`;
}

const NOT_FOUND = 5;
const UNIMPLEMENTED = 12;

export class MockTransport implements BridgeTransport {
  state: BridgeState = "up";
  /** Method paths in call order. */
  readonly calls: string[] = [];
  private readonly unaryHandlers = new Map<string, (req: Uint8Array) => Uint8Array>();
  private readonly streamHandlers = new Map<string, (req: Uint8Array) => Iterable<Uint8Array>>();

  constructor(private readonly latencyMs = 0) {}

  onUnary<I extends DescMessage, O extends DescMessage>(
    method: DescMethodUnary<I, O>,
    fn: (req: MessageShape<I>) => MessageInitShape<O>,
  ): this {
    this.unaryHandlers.set(path(method), (raw) =>
      toBinary(method.output, create(method.output, fn(fromBinary(method.input, raw)))),
    );
    return this;
  }

  onStream<I extends DescMessage, O extends DescMessage>(
    method: DescMethodServerStreaming<I, O>,
    fn: (req: MessageShape<I>) => Iterable<MessageInitShape<O>>,
  ): this {
    this.streamHandlers.set(path(method), (raw) => {
      const out: Uint8Array[] = [];
      for (const init of fn(fromBinary(method.input, raw))) {
        out.push(toBinary(method.output, create(method.output, init)));
      }
      return out;
    });
    return this;
  }

  private async wait(signal: AbortSignal | undefined): Promise<void> {
    if (signal?.aborted) throw new BridgeError(1, "cancelled");
    if (this.latencyMs > 0) await new Promise((r) => setTimeout(r, this.latencyMs));
    if (signal?.aborted) throw new BridgeError(1, "cancelled");
  }

  async unary(call: CallOptions): Promise<Uint8Array> {
    this.calls.push(call.method);
    await this.wait(call.signal);
    const h = this.unaryHandlers.get(call.method);
    if (!h) throw new BridgeError(UNIMPLEMENTED, `mock has no handler for ${call.method}`);
    return h(call.request);
  }

  async *stream(call: CallOptions): AsyncGenerator<Uint8Array> {
    this.calls.push(call.method);
    await this.wait(call.signal);
    const h = this.streamHandlers.get(call.method);
    if (!h) throw new BridgeError(UNIMPLEMENTED, `mock has no handler for ${call.method}`);
    for (const frame of h(call.request)) {
      await this.wait(call.signal);
      yield frame;
    }
  }

  close(): void {
    this.state = "closed";
  }
}

export function notFound(what: string): BridgeError {
  return new BridgeError(NOT_FOUND, `${what} not found`);
}
