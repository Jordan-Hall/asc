// One mapping from an RPC failure to a page. NOT_FOUND and PERMISSION_DENIED look the same on
// purpose: the UI must not reveal whether a repository exists to someone who cannot read it.
import { BridgeError } from "@asc/asc-client";
import type { ReactNode } from "react";
import { Link } from "../lib/router";

const NOT_FOUND = 5;
const PERMISSION_DENIED = 7;
const UNAUTHENTICATED = 16;

export type ErrorKind = "not-found" | "sign-in" | "unavailable" | "other";

export function classify(error: unknown): ErrorKind {
  if (!(error instanceof BridgeError)) return "other";
  if (error.code === NOT_FOUND || error.code === PERMISSION_DENIED) return "not-found";
  if (error.code === UNAUTHENTICATED) return "sign-in";
  if (error.code === 14) return "unavailable";
  return "other";
}

const COPY: Record<ErrorKind, { title: string; body: string }> = {
  "not-found": { title: "Not found", body: "This page does not exist, or you cannot see it." },
  "sign-in": { title: "Sign in required", body: "Sign in with an asc token to see this." },
  unavailable: {
    title: "Server unreachable",
    body: "The server did not answer. Check your connection and try again.",
  },
  other: { title: "Something went wrong", body: "The request failed." },
};

export function ErrorState(props: { error: unknown; onRetry?: () => void }): ReactNode {
  const kind = classify(props.error);
  const copy = COPY[kind];
  return (
    <div role="alert" className="mx-auto max-w-xl p-8 text-center">
      <h1 className="mb-2 text-xl font-semibold text-fg-strong">{copy.title}</h1>
      <p className="mb-4 text-fg-muted">{copy.body}</p>
      {kind === "sign-in" ? (
        <Link
          to="/signin"
          className="inline-flex h-9 items-center rounded-md bg-brand px-4 text-on-brand no-underline"
        >
          Sign in
        </Link>
      ) : null}
      {kind === "unavailable" && props.onRetry ? (
        <button
          type="button"
          onClick={props.onRetry}
          className="h-9 rounded-md bg-brand px-4 text-on-brand"
        >
          Retry
        </button>
      ) : null}
    </div>
  );
}
