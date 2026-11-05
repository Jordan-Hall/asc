// TSRX highlighting for the code view: the rules of the hub's chroma lexer
// (modules/highlight/tsrx) as a line tokenizer whose state carries from one line to the next, so the
// virtual list can colour any row after one pass over the rows above it. TSRX adds to TypeScript:
//   - the component body `@{ ... }`;
//   - control flow `@if`, `@else`, `@for`, `@empty`, `@switch`, `@case`, `@default`, `@try`,
//     `@pending` and `@catch`, each with an optional `(head)` and a `{block}`;
//   - elements and fragments, `{expression}` slots and text;
//   - a scoped `<style>` whose body is CSS.
// Every rule is anchored at the current position and either consumes text or changes the mode, and
// no rule scans further than the end of the line, so a line costs time linear in its length.

export type Kind =
  | "text"
  | "comment"
  | "keyword"
  | "control"
  | "constant"
  | "type"
  | "string"
  | "number"
  | "func"
  | "tag"
  | "attr"
  | "punct";

export interface Token {
  kind: Kind;
  text: string;
}

type Mode =
  | "code"
  | "braces"
  | "parens"
  | "control"
  | "template"
  | "comment"
  | "style"
  | "tag"
  | "attrs"
  | "children";

/** The mode stack at the start of a line; treat as immutable. */
export type State = readonly Mode[];

export const INITIAL: State = ["code"];

/** Deeper nesting is shown as text: the stack, and so each saved line state, stays small. */
const MAX_DEPTH = 128;
/** Past this many characters a line is shown as text, as VS Code does. */
export const MAX_LINE = 20_000;

const IDENT = /[A-Za-z_$][\w$]*/y;
const WS = /\s+/y;
const LINE_COMMENT = /\/\/.*/y;
const STYLE_OPEN = /<(style)((?:\s[^>]{0,200})?>)/y;
const CONTROL = /@(?:else\s+if|if|else|for|empty|switch|case|default|try|pending|catch)\b/y;
const SINGLE = /'(?:[^'\\]|\\.)*(?:'|$)/y;
const DOUBLE = /"(?:[^"\\]|\\.)*(?:"|$)/y;
const NUMBER = /0[xX][\da-fA-F_]+n?|0[bB][01_]+n?|\d[\d_]*(?:\.\d[\d_]*)?(?:[eE][+-]?\d+)?n?/y;
const OPERATOR = /=>|\.\.\.|[-+*/%&|^!~=<>?:]+/y;
const TAG_NAME = /[A-Za-z_][\w.:$-]*/y;
const ATTR_NAME = /[A-Za-z_][\w:.$-]*/y;
const ATTR_SINGLE = /'[^']*'/y;
const ATTR_DOUBLE = /"[^"]*"/y;
const CLOSE_TAG = /<\/([A-Za-z_][\w.:$-]*)?(\s*>)/y;
const CHILD_TEXT = /[^<{@}&]+/y;
const ENTITY = /&(?:[A-Za-z][A-Za-z0-9]{1,31}|#\d{1,7}|#x[\da-fA-F]{1,6});/y;
const TEMPLATE_TEXT = /[^`\\$]+/y;
const CSS_STYLE_END = /<\/style\s*>/g;
const CSS_TOKEN =
  /(\/\*.*?(?:\*\/|$))|([\w-]+)(?=\s*:)|('[^']*'?|"[^"]*"?)|(#[\da-fA-F]{3,8}\b|-?\d+(?:\.\d+)?(?:[a-z%]+)?)|([.#][\w-]+)|([{}();:,>+~])|(\s+|[^\s'"{}();:,>+~#.]+|.)/y;

const DECLARATIONS = new Set(["const", "let", "var", "function", "class", "interface", "type", "enum"]);
const CONSTANTS = new Set(["true", "false", "null", "undefined", "this", "NaN", "Infinity"]);
const TYPES = new Set([
  "string",
  "number",
  "boolean",
  "any",
  "unknown",
  "never",
  "object",
  "symbol",
  "bigint",
]);
const KEYWORDS = new Set([
  "abstract",
  "as",
  "async",
  "await",
  "break",
  "case",
  "catch",
  "continue",
  "debugger",
  "declare",
  "default",
  "delete",
  "do",
  "else",
  "export",
  "extends",
  "finally",
  "for",
  "from",
  "get",
  "if",
  "implements",
  "import",
  "in",
  "instanceof",
  "is",
  "keyof",
  "namespace",
  "new",
  "of",
  "private",
  "protected",
  "public",
  "readonly",
  "return",
  "satisfies",
  "set",
  "static",
  "super",
  "switch",
  "throw",
  "try",
  "typeof",
  "void",
  "while",
  "with",
  "yield",
  "infer",
  "override",
  "accessor",
  "using",
]);
const NAMES_FUNCTION = new Set(["function"]);
const NAMES_TYPE = new Set(["class", "interface", "type", "enum", "namespace"]);

/** A "<" starts markup only where an expression can: not after a word, `)` or `]`. */
const tagMayStart = (line: string, i: number): boolean => i === 0 || !/[\w$)\]]/.test(line[i - 1] ?? "");

function at(re: RegExp, line: string, i: number): RegExpExecArray | null {
  re.lastIndex = i;
  return re.exec(line);
}

class Lexer {
  readonly out: Token[] = [];
  readonly stack: Mode[];
  i = 0;
  constructor(
    readonly line: string,
    state: State,
  ) {
    this.stack = [...state];
  }

  emit(kind: Kind, text: string): void {
    if (text === "") return;
    const last = this.out.at(-1);
    if (last?.kind === kind) last.text += text;
    else this.out.push({ kind, text });
    this.i += text.length;
  }

  get mode(): Mode {
    return this.stack.at(-1) ?? "code";
  }

  /** Push `m`, or stay put (the opener shows as text) when the stack is full. */
  push(m: Mode): boolean {
    if (this.stack.length >= MAX_DEPTH) return false;
    this.stack.push(m);
    return true;
  }

  pop(): void {
    if (this.stack.length > 1) this.stack.pop();
  }

  replace(m: Mode): void {
    this.stack[this.stack.length - 1] = m;
  }
}

/** Markup starting at `lx.i` (a style, a fragment or an element), true when one was taken. */
function markup(lx: Lexer, guarded: boolean): boolean {
  const { line, i } = lx;
  if (line[i] !== "<" || (guarded && !tagMayStart(line, i))) return false;
  const open = at(STYLE_OPEN, line, i);
  if (open !== null && lx.push("style")) {
    lx.emit("punct", "<");
    lx.emit("tag", open[1] ?? "");
    lx.emit("punct", open[2] ?? "");
    return true;
  }
  if (line[i + 1] === ">" && lx.push("children")) {
    lx.emit("punct", "<>");
    return true;
  }
  if (/[A-Za-z_]/.test(line[i + 1] ?? "") && lx.push("tag")) {
    lx.emit("punct", "<");
    return true;
  }
  return false;
}

function code(lx: Lexer): void {
  const { line, i } = lx;
  const c = line[i] ?? "";
  const mode = lx.mode;
  let m: RegExpExecArray | null;
  if ((m = at(WS, line, i))) return lx.emit("text", m[0]);
  if (mode === "braces" && c === "}") {
    lx.emit("punct", c);
    return lx.pop();
  }
  if (mode === "parens" && c === ")") {
    lx.emit("punct", c);
    return lx.pop();
  }
  if ((m = at(LINE_COMMENT, line, i))) return lx.emit("comment", m[0]);
  if (line.startsWith("/*", i)) {
    const end = line.indexOf("*/", i + 2);
    if (end >= 0) return lx.emit("comment", line.slice(i, end + 2));
    lx.push("comment");
    return lx.emit("comment", line.slice(i));
  }
  if (line.startsWith("@{", i) && lx.push("braces")) return lx.emit("control", "@{");
  if ((m = at(CONTROL, line, i)) && lx.push("control")) return lx.emit("control", m[0]);
  if (markup(lx, true)) return;
  if (c === "`" && lx.push("template")) return lx.emit("string", c);
  if ((m = at(SINGLE, line, i)) || (m = at(DOUBLE, line, i))) return lx.emit("string", m[0]);
  if (/\d/.test(c) && (m = at(NUMBER, line, i))) return lx.emit("number", m[0]);
  if ((m = at(IDENT, line, i))) {
    const w = m[0];
    if (DECLARATIONS.has(w) || NAMES_TYPE.has(w)) {
      lx.emit("keyword", w);
      const ws = at(WS, line, lx.i);
      const name = ws ? at(IDENT, line, lx.i + ws[0].length) : null;
      if (ws && name && (NAMES_FUNCTION.has(w) || NAMES_TYPE.has(w))) {
        lx.emit("text", ws[0]);
        lx.emit(NAMES_FUNCTION.has(w) ? "func" : "type", name[0]);
      }
      return;
    }
    if (CONSTANTS.has(w)) return lx.emit("constant", w);
    if (TYPES.has(w)) return lx.emit("type", w);
    if (KEYWORDS.has(w)) return lx.emit("keyword", w);
    return lx.emit("text", w);
  }
  if ((m = at(OPERATOR, line, i))) return lx.emit("punct", m[0]);
  if (c === "{" && lx.push("braces")) return lx.emit("punct", c);
  if (c === "(" && lx.push("parens")) return lx.emit("punct", c);
  return lx.emit(/[()[\];,.}]/.test(c) ? "punct" : "text", c);
}

function control(lx: Lexer): void {
  const { line, i } = lx;
  const ws = at(WS, line, i);
  if (ws) return lx.emit("text", ws[0]);
  if (line[i] === "(" && lx.push("parens")) return lx.emit("punct", "(");
  if (line[i] === "{") {
    lx.replace("braces");
    return lx.emit("punct", "{");
  }
  lx.pop();
}

function template(lx: Lexer): void {
  const { line, i } = lx;
  const c = line[i];
  if (c === "`") {
    lx.emit("string", c);
    return lx.pop();
  }
  if (c === "\\") return lx.emit("string", line.slice(i, i + 2));
  if (line.startsWith("${", i) && lx.push("braces")) return lx.emit("punct", "${");
  const m = at(TEMPLATE_TEXT, line, i);
  lx.emit("string", m ? m[0] : (c ?? ""));
}

function comment(lx: Lexer): void {
  const end = lx.line.indexOf("*/", lx.i);
  if (end < 0) return lx.emit("comment", lx.line.slice(lx.i));
  lx.emit("comment", lx.line.slice(lx.i, end + 2));
  lx.pop();
}

function css(lx: Lexer, body: string): void {
  let j = 0;
  while (j < body.length) {
    const m = at(CSS_TOKEN, body, j);
    if (m === null) break;
    const kind: Kind =
      m[1] !== undefined
        ? "comment"
        : m[2] !== undefined
          ? "attr"
          : m[3] !== undefined
            ? "string"
            : m[4] !== undefined
              ? "number"
              : m[5] !== undefined
                ? "type"
                : m[6] !== undefined
                  ? "punct"
                  : "text";
    lx.emit(kind, m[0]);
    j += m[0].length;
  }
}

function style(lx: Lexer): void {
  const { line, i } = lx;
  CSS_STYLE_END.lastIndex = i;
  const end = CSS_STYLE_END.exec(line);
  if (end === null) return css(lx, line.slice(i));
  css(lx, line.slice(i, end.index));
  lx.emit("punct", "</");
  lx.emit("tag", "style");
  lx.emit("punct", end[0].slice("</style".length));
  lx.pop();
}

function tag(lx: Lexer): void {
  const m = at(TAG_NAME, lx.line, lx.i);
  if (m === null) return lx.pop();
  lx.replace("attrs");
  lx.emit("tag", m[0]);
}

function attrs(lx: Lexer): void {
  const { line, i } = lx;
  const c = line[i] ?? "";
  let m: RegExpExecArray | null;
  if ((m = at(WS, line, i))) return lx.emit("text", m[0]);
  if (line.startsWith("/>", i)) {
    lx.emit("punct", "/>");
    return lx.pop();
  }
  if (c === ">") {
    lx.replace("children");
    return lx.emit("punct", c);
  }
  if (c === "=") return lx.emit("punct", c);
  if ((m = at(ATTR_SINGLE, line, i)) || (m = at(ATTR_DOUBLE, line, i))) {
    return lx.emit("string", m[0]);
  }
  if (c === "{" && lx.push("braces")) return lx.emit("punct", c);
  if ((m = at(ATTR_NAME, line, i))) return lx.emit("attr", m[0]);
  lx.emit("text", c);
}

function children(lx: Lexer): void {
  const { line, i } = lx;
  let m: RegExpExecArray | null;
  if ((m = at(CLOSE_TAG, line, i))) {
    lx.emit("punct", "</");
    lx.emit("tag", m[1] ?? "");
    lx.emit("punct", m[2] ?? "");
    return lx.pop();
  }
  if (markup(lx, false)) return;
  if (line[i] === "{" && lx.push("braces")) return lx.emit("punct", "{");
  if ((m = at(CONTROL, line, i)) && lx.push("control")) return lx.emit("control", m[0]);
  if ((m = at(ENTITY, line, i))) return lx.emit("constant", m[0]);
  if ((m = at(CHILD_TEXT, line, i))) return lx.emit("text", m[0]);
  lx.emit("text", line[i] ?? "");
}

const STEP: Record<Mode, (lx: Lexer) => void> = {
  code,
  braces: code,
  parens: code,
  control,
  template,
  comment,
  style,
  tag,
  attrs,
  children,
};

/** The tokens of one line, and the state the next line starts in. */
export function tokenizeLine(line: string, state: State): { tokens: Token[]; state: State } {
  if (line.length > MAX_LINE) return { tokens: [{ kind: "text", text: line }], state };
  const lx = new Lexer(line, state);
  // Each step consumes text or changes the mode; a mode change that consumes nothing (control,
  // tag) always moves to a mode whose step consumes, so this bound is never reached.
  let guard = line.length * 4 + 8;
  while (lx.i < line.length && guard-- > 0) {
    const before = lx.i;
    const depth = lx.stack.length;
    const top = lx.mode;
    STEP[top](lx);
    if (lx.i === before && lx.stack.length === depth && lx.mode === top) lx.emit("text", line[lx.i] ?? "");
  }
  if (lx.i < line.length) lx.emit("text", line.slice(lx.i));
  // `@if` at the end of a line keeps waiting for its head on the next one; a tag name does not
  // continue past a line break, so an unnamed "<" falls back to the surrounding mode.
  if (lx.mode === "tag") lx.pop();
  return { tokens: lx.out, state: lx.stack };
}
