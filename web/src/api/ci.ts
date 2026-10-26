// CI and Actions runs over asc.v1.Ci (the `ci.v1` feature). Reads only: reporting a run is for
// runners and forge adapters with a service credential.
import {
  type BridgeTransport,
  callUnary,
  type CallParams,
  changeIdToString,
} from "@asc/asc-client";
import { Ci, CiStatus } from "@asc/asc-client/ci";

export type { CiStatus };

export interface CiJobView {
  id: number;
  name: string;
  status: CiStatus;
  url: string;
  logSize: number;
}

export interface CiRunView {
  id: number;
  name: string;
  /** Empty for a run not tied to a change. */
  change: string;
  version: number;
  status: CiStatus;
  /** Seconds since the epoch. */
  created: number;
  url: string;
  jobs: readonly CiJobView[];
}

export interface CiRunsView {
  runs: readonly CiRunView[];
  next: string;
}

export async function listRuns(
  t: BridgeTransport,
  repo: string,
  opts: { change?: Uint8Array; status?: CiStatus; pageToken?: string } = {},
  p?: CallParams,
): Promise<CiRunsView> {
  const res = await callUnary(
    t,
    Ci.method.listRuns,
    {
      repo: { name: repo },
      change: opts.change ?? new Uint8Array(0),
      status: opts.status ?? CiStatus.UNSPECIFIED,
      pageToken: opts.pageToken ?? "",
    },
    p,
  );
  return {
    next: res.nextPageToken,
    runs: res.runs.map((r) => ({
      id: Number(r.id),
      name: r.name,
      change: r.change.length > 0 ? changeIdToString(r.change) : "",
      version: r.version,
      status: r.status,
      created: Number(r.created),
      url: r.url,
      jobs: r.jobs.map((j) => ({
        id: Number(j.id),
        name: j.name,
        status: j.status,
        url: j.url,
        logSize: Number(j.logSize),
      })),
    })),
  };
}
