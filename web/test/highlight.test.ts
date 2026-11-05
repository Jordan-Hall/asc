// TSRX highlighting in the code view: the kinds the line tokenizer gives each form, the state that
// carries across lines, the line cache, and a time budget on large and hostile input.
import { describe, expect, test } from "bun:test";
import { Highlighter, highlighterFor, highlighterOf } from "../src/lib/highlight";
import { INITIAL, MAX_LINE, type State, type Token, tokenizeLine } from "../src/lib/highlight/tsrx";

function lex(src: string): Token[][] {
  let state: State = INITIAL;
  return src.split("\n").map((line) => {
    const r = tokenizeLine(line, state);
    state = r.state;
    return r.tokens;
  });
}

/** The kind of the first token on `line` (1-based) whose text is `text`. */
function kindOf(lines: Token[][], line: number, text: string): string {
  const t = lines[line - 1]?.find((x) => x.text === text);
  if (t === undefined) {
    throw new Error(`no ${JSON.stringify(text)} on line ${line}: ${JSON.stringify(lines[line - 1])}`);
  }
  return t.kind;
}

/** Milliseconds to tokenize `src`. */
function ms(src: string): number {
  const t0 = performance.now();
  lex(src);
  return performance.now() - t0;
}

const SAMPLE = `import type { Task } from './types';

function TaskList({ tasks, compact }: Props) @{
  /* one row
     per task */
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
    .task-row { padding: 4px; color: "red"; }
  </style>
}

export const limit = 1_000 < 2000;
const greeting = \`hello \${name}\`;`;

describe("tsrx tokens", () => {
  const lines = lex(SAMPLE);

  test("the text of every line is kept", () => {
    const src = SAMPLE.split("\n");
    for (const [i, toks] of lines.entries()) expect(toks.map((t) => t.text).join("")).toBe(src[i] ?? "");
  });

  test("component body, control flow and TypeScript", () => {
    expect(kindOf(lines, 1, "import")).toBe("keyword");
    expect(kindOf(lines, 1, "'./types'")).toBe("string");
    expect(kindOf(lines, 3, "function")).toBe("keyword");
    expect(kindOf(lines, 3, "TaskList")).toBe("func");
    expect(kindOf(lines, 3, "@{")).toBe("control");
    expect(kindOf(lines, 6, "const")).toBe("keyword");
    expect(kindOf(lines, 6, "string")).toBe("type");
    expect(kindOf(lines, 7, "@for")).toBe("control");
    expect(kindOf(lines, 8, "'nobody'")).toBe("string");
    expect(kindOf(lines, 11, "@if")).toBe("control");
    expect(kindOf(lines, 13, "@else")).toBe("control");
    expect(kindOf(lines, 17, "@empty")).toBe("control");
  });

  test("a block comment spans lines", () => {
    expect(lines[3]?.[1]?.kind).toBe("comment");
    expect(lines[4]?.map((t) => t.kind)).toEqual(["comment"]);
  });

  test("tags, attributes, slots, entities and text", () => {
    expect(kindOf(lines, 9, "div")).toBe("tag");
    expect(kindOf(lines, 9, "class")).toBe("attr");
    expect(kindOf(lines, 9, '"task-row"')).toBe("string");
    expect(kindOf(lines, 9, "data-id")).toBe("attr");
    expect(kindOf(lines, 10, "span")).toBe("tag");
    expect(kindOf(lines, 12, "/>")).toBe("punct");
    expect(kindOf(lines, 14, "'detail'")).toBe("string");
    expect(kindOf(lines, 14, "Owner: ")).toBe("text");
    expect(kindOf(lines, 14, "&amp;")).toBe("constant");
    expect(kindOf(lines, 16, "div")).toBe("tag");
    expect(kindOf(lines, 18, "No tasks")).toBe("text");
    expect(kindOf(lines, 20, "<>")).toBe("punct");
    expect(kindOf(lines, 21, "button")).toBe("tag");
  });

  test("a <style> body is CSS", () => {
    expect(kindOf(lines, 23, "style")).toBe("tag");
    expect(kindOf(lines, 24, ".task-row")).toBe("type");
    expect(kindOf(lines, 24, "padding")).toBe("attr");
    expect(kindOf(lines, 24, "4px")).toBe("number");
    expect(kindOf(lines, 24, '"red"')).toBe("string");
    expect(kindOf(lines, 25, "style")).toBe("tag");
  });

  test("generics and comparisons are not tags; the file ends back in code", () => {
    for (const t of lines[5] ?? []) expect(t.kind).not.toBe("tag");
    for (const t of lines[27] ?? []) expect(t.kind).not.toBe("tag");
    expect(kindOf(lines, 28, "1_000")).toBe("number");
    expect(kindOf(lines, 29, "`hello ")).toBe("string");
    expect(kindOf(lines, 29, "${")).toBe("punct");
    expect(kindOf(lines, 29, "`")).toBe("string");
    let state: State = INITIAL;
    for (const line of SAMPLE.split("\n")) state = tokenizeLine(line, state).state;
    expect(state).toEqual(["code"]);
  });

  test("the whole control-flow set", () => {
    const toks = lex(
      "@{ @switch (k) { @case (1) { <a/> } @default { <b/> } } @try { <c/> } @pending { <d/> } @catch (e) { <e/> } }",
    )[0];
    for (const kw of ["@switch", "@case", "@default", "@try", "@pending", "@catch"]) {
      expect(toks?.find((t) => t.text === kw)?.kind).toBe("control");
    }
  });
});

describe("highlighter", () => {
  test("only .tsrx files get one", () => {
    expect(highlighterFor("src/app.tsrx")).toBeInstanceOf(Highlighter);
    expect(highlighterFor("src/app.ts")).toBeUndefined();
    expect(highlighterFor("README.md")).toBeUndefined();
  });

  test("one per lines array, and rows far down carry the state of the rows above", () => {
    const lines = ["@{", "  /* open", "  still comment */ <div>", "  text", "  </div>", "}"];
    const hl = highlighterOf(lines, "x.tsrx");
    expect(highlighterOf(lines, "x.tsrx")).toBe(hl);
    expect(highlighterOf([...lines], "x.tsrx")).not.toBe(hl);
    expect(hl?.tokens(lines, 3).map((t) => t.kind)).toEqual(["text"]);
    expect(hl?.tokens(lines, 2)[0]?.kind).toBe("comment");
    // The array grows in place while the file streams in.
    lines.push("const x = 1;");
    expect(hl?.tokens(lines, 6)[0]?.kind).toBe("keyword");
  });

  test("an over-long line is plain text and keeps the state", () => {
    const long = `<div>${"x".repeat(MAX_LINE)}`;
    const r = tokenizeLine(long, ["children"]);
    expect(r.tokens).toEqual([{ kind: "text", text: long }]);
    expect(r.state).toEqual(["children"]);
  });
});

describe("time budget", () => {
  test("a large well-formed file", () => {
    const row = '  <div class="r" data-k={k}><span>{v}</span> text &lt; more</div>';
    expect(ms(`function Big() @{\n${Array.from({ length: 20_000 }, () => row).join("\n")}\n}`)).toBeLessThan(
      1500,
    );
  });

  test("hostile lines", () => {
    for (const src of [
      "<".repeat(MAX_LINE),
      "<a ".repeat(6000),
      "@if (".repeat(4000),
      "{".repeat(MAX_LINE),
      `<style${" x".repeat(9000)}`,
      `'${"\\'".repeat(9000)}`,
      `/*${"*".repeat(MAX_LINE - 2)}`,
      `<div a="${"x".repeat(MAX_LINE - 10)}`,
      Array.from({ length: 20_000 }, () => "<p>").join("\n"),
      Array.from({ length: 20_000 }, () => "{").join("\n"),
    ]) {
      expect(ms(src)).toBeLessThan(1000);
    }
  });
});
