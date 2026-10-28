import { expect, test } from "bun:test";
import { checkJetbrains, loadBaseline, unenforced, type Metrics } from "./jetbrains";

const base = loadBaseline();
const measured: Metrics = {
  activationMs: null,
  inertActivationMs: null,
  jvmHeapDeltaMiB: null,
  bridgeRssMiB: 12,
  jarBytes: 3_000_000,
};

test("without a probe report only the metrics this machine measures are judged, and the rest are named", () => {
  expect(checkJetbrains(measured, base)).toEqual([]);
  expect(unenforced(measured)).toEqual(["activationMs", "inertActivationMs", "jvmHeapDeltaMiB"]);
});

test("a measured metric over its budget fails, probe or not", () => {
  const f = checkJetbrains({ ...measured, jarBytes: 5_000_000, activationMs: 31 }, base);
  expect(f.map((x) => `${x.metric}:${x.kind}`).sort()).toEqual(["activationMs:budget", "jarBytes:budget"]);
});

test("a missing jar or bridge measurement is a failure, never a pass", () => {
  const f = checkJetbrains({ ...measured, bridgeRssMiB: null, jarBytes: null }, base);
  expect(f.map((x) => x.kind)).toEqual(["missing", "missing"]);
});
