// The generated client carries DepsService (proto/asc/v1/deps.proto), and the relay does not forward any
// of it: the hub has its own Dependencies page and reaches the service through its server, so nothing in
// the editor webviews can call it until a screen is wired to it and its RPCs are moved out of `never`.
import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { ALLOWLIST_PATH, protoRpcs } from "../scripts/gen-allowlist";
import { DepsService } from "../src/gen/asc/v1/deps_pb";

const METHODS = [
  "Scan",
  "ListFindings",
  "ListUpdates",
  "ProposeUpdate",
  "SetConfig",
  "GetConfig",
  "Dismiss",
];

describe("DepsService", () => {
  test("the generated service has the seven RPCs", () => {
    expect(DepsService.typeName).toBe("asc.v1.DepsService");
    expect(DepsService.methods.map((m) => m.name).sort()).toEqual([...METHODS].sort());
  });

  test("every RPC is known to gen-allowlist and none is forwarded by the relay", () => {
    const al = JSON.parse(readFileSync(ALLOWLIST_PATH, "utf8")) as {
      rpc: { read: string[]; write: string[]; never: string[] };
    };
    const rpcs = protoRpcs();
    for (const m of METHODS) {
      const name = `asc.v1.DepsService/${m}`;
      expect(rpcs).toContain(name);
      expect(al.rpc.never).toContain(name);
      expect(al.rpc.read).not.toContain(name);
      expect(al.rpc.write).not.toContain(name);
    }
  });
});
