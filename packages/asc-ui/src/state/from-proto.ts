import { changeIdToString, toHex } from "@asc/asc-client";
import type { WsStacksResponse } from "@asc/asc-client/workspace";
import type { ReviewInfo, Stack, StackChange, Stacks } from "./model";

const decoder = new TextDecoder("utf-8", { fatal: false });

/** Convert the wire message to the model. Pure; the proto types stop here. */
export function stacksFromProto(res: WsStacksResponse): Stacks {
  return {
    version: res.version,
    trunks: res.trunks.map((t) => ({ name: t.name, tip: toHex(t.tip), remote: t.remote })),
    stacks: res.stacks.map(
      (s): Stack => ({
        trunk: s.trunk,
        base: toHex(s.base),
        behind: s.behind,
        changes: s.changes.map((c): StackChange => {
          const review: ReviewInfo | undefined = c.review
            ? {
                host: c.review.host,
                number: Number(c.review.number),
                url: c.review.url,
                state: c.review.state,
                draft: c.review.draft,
                approvals: c.review.approvals,
                checks: c.review.checks,
              }
            : undefined;
          const parent = c.parentChange.length > 0 ? changeIdToString(c.parentChange) : undefined;
          return {
            id: changeIdToString(c.change),
            ...(parent === undefined ? {} : { parent }),
            subject: decoder.decode(c.subject),
            branches: c.branches,
            state: c.state,
            needsRestack: c.needsRestack,
            frozen: c.frozen,
            empty: c.empty,
            current: c.current,
            ...(review === undefined ? {} : { review }),
          };
        }),
      }),
    ),
  };
}
