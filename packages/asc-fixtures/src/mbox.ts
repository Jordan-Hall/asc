// Builds an mbox of patches for `asc am`, so a long linear history costs one asc process instead
// of one per commit. The patches are produced from a model of the tree, with fixed author
// dates, so the resulting commits are identical on every build.
import { EPOCH, IDENTITY } from "./asc";

export type FileEdit =
  | { readonly path: string; readonly content: string }
  | { readonly path: string; readonly remove: true }
  | { readonly path: string; readonly renameFrom: string };

export interface PatchCommit {
  readonly subject: string;
  /** Seconds after the fixture epoch. */
  readonly at: number;
  readonly edits: readonly FileEdit[];
}

function rfc2822(seconds: number): string {
  return new Date(seconds * 1000).toUTCString().replace("GMT", "+0000");
}

function lines(text: string): string[] {
  if (text === "") return [];
  const out = text.split("\n");
  if (out.at(-1) === "") out.pop();
  return out;
}

function range(start: number, count: number): string {
  return count === 1 ? `${start}` : `${start},${count}`;
}

/** A whole-file replacement hunk; `git apply` needs no context for one. */
function hunk(oldText: string, newText: string): string {
  const a = lines(oldText);
  const b = lines(newText);
  const head = `@@ -${a.length === 0 ? "0,0" : range(1, a.length)} +${b.length === 0 ? "0,0" : range(1, b.length)} @@\n`;
  return head + a.map((l) => `-${l}\n`).join("") + b.map((l) => `+${l}\n`).join("");
}

/**
 * The patch series for `commits`, applied on top of `tree` (path to content), which is updated
 * as the series advances so a caller can chain several series.
 */
export function buildMbox(commits: readonly PatchCommit[], tree: Map<string, string>): string {
  let out = "";
  commits.forEach((c, i) => {
    let diff = "";
    let stat = 0;
    for (const e of c.edits) {
      stat += 1;
      if ("remove" in e) {
        const old = tree.get(e.path);
        if (old === undefined) throw new Error(`mbox: ${e.path} is not in the tree`);
        diff += `diff --git a/${e.path} b/${e.path}\ndeleted file mode 100644\n--- a/${e.path}\n+++ /dev/null\n${hunk(old, "")}`;
        tree.delete(e.path);
      } else if ("renameFrom" in e) {
        const old = tree.get(e.renameFrom);
        if (old === undefined) throw new Error(`mbox: ${e.renameFrom} is not in the tree`);
        diff += `diff --git a/${e.renameFrom} b/${e.path}\nsimilarity index 100%\nrename from ${e.renameFrom}\nrename to ${e.path}\n`;
        tree.delete(e.renameFrom);
        tree.set(e.path, old);
      } else {
        const old = tree.get(e.path);
        if (old === undefined) {
          diff += `diff --git a/${e.path} b/${e.path}\nnew file mode 100644\n--- /dev/null\n+++ b/${e.path}\n${hunk("", e.content)}`;
        } else {
          diff += `diff --git a/${e.path} b/${e.path}\n--- a/${e.path}\n+++ b/${e.path}\n${hunk(old, e.content)}`;
        }
        tree.set(e.path, e.content);
      }
    }
    const from = `From ${(i + 1).toString(16).padStart(40, "0")} Mon Sep 17 00:00:00 2001\n`;
    out +=
      `${from}From: ${IDENTITY.name} <${IDENTITY.email}>\nDate: ${rfc2822(EPOCH + c.at)}\n` +
      `Subject: [PATCH] ${c.subject}\n\n---\n ${stat} file${stat === 1 ? "" : "s"} changed\n\n${diff}-- \n2.0.0\n\n`;
  });
  return out;
}
