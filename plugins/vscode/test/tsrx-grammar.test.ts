// The TSRX TextMate grammar (syntaxes/tsrx.tmLanguage.json) run through vscode-textmate with the
// Oniguruma engine VS Code uses. The embedded TypeScript and CSS grammars are small stand-ins, so
// the checks are about the scopes TSRX itself adds: component bodies, control flow, tags,
// attributes, `{expr}` slots, fragments and `<style>` bodies, plus a time budget on hostile input.
import { beforeAll, describe, expect, test } from "bun:test";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import * as onig from "vscode-oniguruma";
import { INITIAL, type IGrammar, type IRawGrammar, Registry, parseRawGrammar } from "vscode-textmate";

const ROOT = join(import.meta.dir, "..");

const STUB_TS = {
  scopeName: "source.ts",
  patterns: [
    { name: "keyword.control.ts", match: "\\b(?:const|let|function|return|import|export|type|from|new)\\b" },
    { name: "string.quoted.single.ts", match: "'[^']*'" },
    { name: "constant.numeric.ts", match: "\\b\\d[\\d_]*\\b" },
  ],
};
const STUB_CSS = {
  scopeName: "source.css",
  patterns: [{ name: "support.type.property-name.css", match: "[a-z-]+(?=\\s*:)" }],
};

interface Tok {
  text: string;
  scopes: string[];
}

let grammar: IGrammar;

beforeAll(async () => {
  const wasm = await readFile(join(ROOT, "node_modules/vscode-oniguruma/release/onig.wasm"));
  await onig.loadWASM(wasm.buffer.slice(wasm.byteOffset, wasm.byteOffset + wasm.byteLength));
  const tsrx = await readFile(join(ROOT, "syntaxes/tsrx.tmLanguage.json"), "utf8");
  const registry = new Registry({
    onigLib: Promise.resolve({
      createOnigScanner: (p: string[]) => new onig.OnigScanner(p),
      createOnigString: (s: string) => new onig.OnigString(s),
    }),
    loadGrammar: async (scope: string): Promise<IRawGrammar | null> => {
      if (scope === "source.tsrx") return parseRawGrammar(tsrx, "tsrx.tmLanguage.json");
      if (scope === "source.ts") return parseRawGrammar(JSON.stringify(STUB_TS), "ts.json");
      if (scope === "source.css") return parseRawGrammar(JSON.stringify(STUB_CSS), "css.json");
      return null;
    },
  });
  const g = await registry.loadGrammar("source.tsrx");
  if (g === null) throw new Error("source.tsrx did not load");
  grammar = g;
});

function tokenize(src: string): Tok[][] {
  let state = INITIAL;
  return src.split("\n").map((line) => {
    const r = grammar.tokenizeLine(line, state);
    state = r.ruleStack;
    return r.tokens.map((t) => ({ text: line.slice(t.startIndex, t.endIndex), scopes: t.scopes }));
  });
}

/** The scopes of the first token on `line` (1-based) whose text is `text`. */
function scopesOf(lines: Tok[][], line: number, text: string): string[] {
  const t = lines[line - 1]?.find((x) => x.text === text);
  if (t === undefined) {
    const seen = lines[line - 1]?.map((x) => JSON.stringify(x.text)).join(" ");
    throw new Error(`no token ${JSON.stringify(text)} on line ${line}: ${seen}`);
  }
  return t.scopes;
}

/** Milliseconds to tokenize `src`. */
function budget(src: string): number {
  const t0 = performance.now();
  tokenize(src);
  return performance.now() - t0;
}

const SAMPLE = `import type { Task } from './types';

function TaskList({ tasks, compact }: Props) @{
  // one row per task
  const counts = new Map<string, number>();
  @for (const task of tasks) {
    const owner = task.owner ?? 'nobody';
    <div class="task-row" data-id={task.id}>
      <span>{task.title}</span>
      @if (compact) {
        <img src="/dot.png" alt="" />
      } @else {
        <p class='detail'>Owner: {owner} &amp; team</p>
      }
    </div>
  } @empty {
    <p>No tasks</p>
  }
  <>
    <button onClick={() => counts.clear()}>Reset</button>
  </>
  <style>
    .task-row { padding: 4px; }
  </style>
}

export const limit = 1_000 < 2000;`;

describe("tsrx grammar", () => {
  let lines: Tok[][];
  beforeAll(() => {
    lines = tokenize(SAMPLE);
  });

  test("the component body and control flow are keywords", () => {
    expect(scopesOf(lines, 3, "@{")).toContain("keyword.control.component.tsrx");
    for (const [ln, kw] of [
      [6, "@for"],
      [10, "@if"],
      [12, "@else"],
      [16, "@empty"],
    ] as const) {
      expect(scopesOf(lines, ln, kw)).toContain("keyword.control.flow.tsrx");
    }
    expect(scopesOf(lines, 6, "(")).toContain("meta.control.head.tsrx");
  });

  test("tags, attributes, strings and slots", () => {
    expect(scopesOf(lines, 8, "div")).toContain("entity.name.tag.tsrx");
    expect(scopesOf(lines, 8, "class")).toContain("entity.other.attribute-name.tsrx");
    expect(scopesOf(lines, 8, '"task-row"')).toContain("string.quoted.double.tsrx");
    expect(scopesOf(lines, 8, "data-id")).toContain("entity.other.attribute-name.tsrx");
    expect(scopesOf(lines, 9, "{")).toContain("punctuation.section.embedded.begin.tsrx");
    expect(scopesOf(lines, 13, "'detail'")).toContain("string.quoted.single.tsrx");
    expect(scopesOf(lines, 13, "&amp;")).toContain("constant.character.entity.tsrx");
    expect(scopesOf(lines, 13, " team")).toContain("meta.tag.tsrx");
    expect(scopesOf(lines, 15, "div")).toContain("entity.name.tag.tsrx");
  });

  test("embedded TypeScript inside a control body and a slot", () => {
    expect(scopesOf(lines, 7, "const")).toContain("keyword.control.ts");
    expect(scopesOf(lines, 7, "const")).toContain("meta.control.body.tsrx");
    expect(scopesOf(lines, 7, "'nobody'")).toContain("string.quoted.single.ts");
  });

  test("a self-closing tag and a fragment close", () => {
    expect(scopesOf(lines, 11, "/>")).toContain("punctuation.definition.tag.end.tsrx");
    // Line 12 sits inside the outer <div> only: the <img> closed itself.
    const after = lines[11]?.[0]?.scopes ?? [];
    expect(after.filter((s) => s === "meta.tag.tsrx")).toHaveLength(1);
    expect(scopesOf(lines, 19, "<")).toContain("meta.tag.fragment.tsrx");
    expect(scopesOf(lines, 20, "button")).toContain("meta.tag.fragment.tsrx");
  });

  test("a <style> body is CSS", () => {
    const prop = scopesOf(lines, 23, "padding");
    expect(prop).toContain("source.css");
    expect(prop).toContain("support.type.property-name.css");
  });

  test("generics and comparisons are not tags", () => {
    for (const t of lines[4] ?? []) expect(t.scopes).not.toContain("entity.name.tag.tsrx");
    for (const t of lines[26] ?? []) {
      expect(t.scopes).not.toContain("meta.tag.tsrx");
      expect(t.scopes).not.toContain("meta.block.component.tsrx");
    }
    expect(scopesOf(lines, 4, "// one row per task")).toContain("comment.line.double-slash.tsrx");
  });

  test("every keyword of the control-flow set", () => {
    const src =
      "@{ @switch (k) { @case (1) { <a/> } @default { <b/> } } @try { <c/> } @pending { <d/> } @catch (e) { <e/> } }";
    const toks = tokenize(src)[0] ?? [];
    for (const kw of ["@switch", "@case", "@default", "@try", "@pending", "@catch"]) {
      const t = toks.find((x) => x.text === kw);
      expect(t?.scopes).toContain("keyword.control.flow.tsrx");
    }
    expect(toks.at(-1)?.scopes).toContain("keyword.control.component.tsrx");
  });
});

describe("tsrx grammar on large and hostile input", () => {
  test("a large well-formed file", () => {
    const row = '  <div class="r" data-k={k}><span>{v}</span> text &lt; more</div>';
    const src = `function Big() @{\n${Array.from({ length: 5000 }, () => row).join("\n")}\n}`;
    expect(budget(src)).toBeLessThan(1000);
  });

  test("pathological lines stay within budget", () => {
    for (const src of [
      "<".repeat(50_000),
      "<a ".repeat(20_000),
      "@if (".repeat(10_000),
      "{".repeat(20_000),
      `<style${" x".repeat(20_000)}`,
      "@{ ".repeat(10_000) + "}".repeat(10_000),
      `<div a="${"x".repeat(100_000)}`,
      Array.from({ length: 5000 }, () => "<p>").join("\n"),
    ]) {
      expect(budget(src)).toBeLessThan(5000);
    }
  });
});
