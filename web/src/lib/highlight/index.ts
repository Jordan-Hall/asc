// Syntax colours for the code view. A highlighter keeps the state at the start of every line it
// has reached, so the virtual list can colour row i after one pass over rows 0..i-1, and a file
// that streams in (lines only appended) never re-reads a line. Only TSRX has a tokenizer so far;
// other files keep plain lines.
import { INITIAL, type State, type Token, tokenizeLine } from "./tsrx";

export type { Kind, Token } from "./tsrx";

export class Highlighter {
  private readonly states: State[] = [INITIAL];

  /** The tokens of line `i` (0-based) of `lines`, which only ever grows at the end. */
  tokens(lines: readonly string[], i: number): Token[] {
    while (this.states.length <= i && this.states.length <= lines.length) {
      const n = this.states.length - 1;
      this.states.push(tokenizeLine(lines[n] ?? "", this.states[n] ?? INITIAL).state);
    }
    return tokenizeLine(lines[i] ?? "", this.states[i] ?? INITIAL).tokens;
  }
}

/** A highlighter for the file at `path`, or undefined when its language has none. */
export function highlighterFor(path: string): Highlighter | undefined {
  return /\.tsrx$/i.test(path) ? new Highlighter() : undefined;
}

// One highlighter per lines array (a file view's array grows in place and a new read makes a new
// one), so the states of the rows already shown survive re-renders and scrolling.
const byLines = new WeakMap<readonly string[], Highlighter | undefined>();

/** The highlighter for these lines of the file at `path`, made on first use. */
export function highlighterOf(lines: readonly string[], path: string): Highlighter | undefined {
  if (!byLines.has(lines)) byLines.set(lines, highlighterFor(path));
  return byLines.get(lines);
}
