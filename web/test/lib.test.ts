import { describe, expect, test } from "bun:test";
import { AGENTS_KEY, readAgents, writeAgents } from "../src/lib/agents";
import { createAuthStore, TOKEN_KEY } from "../src/lib/auth";
import { BINDINGS, ChordMachine, typingTarget } from "../src/lib/keys";
import {
  applyTheme,
  parsePref,
  readPref,
  resolveTheme,
  THEME_KEY,
  writePref,
} from "../src/lib/theme";
import { windowRange } from "../src/lib/virtual";

function memStorage() {
  const m = new Map<string, string>();
  return {
    m,
    s: {
      getItem: (k: string) => m.get(k) ?? null,
      setItem: (k: string, v: string) => void m.set(k, v),
      removeItem: (k: string) => void m.delete(k),
    },
  };
}

describe("windowRange", () => {
  test("renders only the visible rows plus overscan", () => {
    expect(windowRange(0, 360, 36, 1_000_000, 4)).toEqual({ start: 0, end: 15 });
    const r = windowRange(36 * 500_000, 360, 36, 1_000_000, 4);
    expect(r.start).toBe(499_996);
    expect(r.end - r.start).toBeLessThan(30);
  });
  test("clamps at both ends and for empty lists", () => {
    expect(windowRange(1e12, 360, 36, 100, 4).end).toBe(100);
    expect(windowRange(-50, 360, 36, 100, 4).start).toBe(0);
    expect(windowRange(0, 360, 36, 0)).toEqual({ start: 0, end: 0 });
  });
  test("a few rows however large the list", () => {
    const r = windowRange(123_456, 1000, 20, 5_000_000);
    expect(r.end - r.start).toBeLessThan(120);
  });
});

describe("ChordMachine", () => {
  test("g then c", () => {
    const m = new ChordMachine(BINDINGS);
    expect(m.feed("g", 0)).toEqual({ action: undefined, pending: true });
    expect(m.feed("c", 100)).toEqual({ action: "go-code", pending: false });
  });
  test("a chord expires", () => {
    const m = new ChordMachine(BINDINGS);
    m.feed("g", 0);
    expect(m.feed("c", 5000).action).toBeUndefined();
  });
  test("single keys and a restart after a wrong second key", () => {
    const m = new ChordMachine(BINDINGS);
    expect(m.feed("t", 0).action).toBe("find-file");
    m.feed("g", 10);
    expect(m.feed("g", 20).pending).toBe(true);
    expect(m.feed("p", 30).action).toBe("go-changes");
    expect(m.feed("?", 40).action).toBe("help");
  });
  test("typing targets are ignored", () => {
    expect(typingTarget({ tagName: "INPUT" } as unknown as EventTarget)).toBe(true);
    expect(
      typingTarget({ tagName: "DIV", isContentEditable: true } as unknown as EventTarget),
    ).toBe(true);
    expect(typingTarget({ tagName: "A" } as unknown as EventTarget)).toBe(false);
  });
});

describe("theme", () => {
  test("resolution and parsing", () => {
    expect(resolveTheme("asc-auto", true)).toBe("dark");
    expect(resolveTheme("asc-auto", false)).toBe("light");
    expect(resolveTheme("asc-light", true)).toBe("light");
    expect(resolveTheme("asc-dark", false)).toBe("dark");
    expect(parsePref("bogus")).toBe("asc-auto");
    expect(parsePref("asc-dark")).toBe("asc-dark");
  });
  test("values stored by earlier versions still parse", () => {
    expect(parsePref("light")).toBe("asc-light");
    expect(parsePref("dark")).toBe("asc-dark");
    expect(parsePref("system")).toBe("asc-auto");
  });
  test("storage failures are survivable", () => {
    const fail = () => {
      throw new Error("blocked");
    };
    const throwing = { getItem: fail, setItem: fail, removeItem: fail };
    expect(readPref(throwing)).toBe("asc-auto");
    expect(() => writePref(throwing, "asc-dark")).not.toThrow();
  });
  test("round trip uses the key theme-init.js reads", async () => {
    const { m, s } = memStorage();
    writePref(s, "asc-dark");
    expect(m.get(THEME_KEY)).toBe("asc-dark");
    const init = await Bun.file(new URL("../public/theme-init.js", import.meta.url)).text();
    expect(init).toContain(`"${THEME_KEY}"`);
    for (const name of ["asc-light", "asc-dark", "asc-auto"]) expect(init).toContain(`"${name}"`);
    const classes = new Set<string>();
    const root = {
      classList: {
        toggle: (c: string, on: boolean) => void (on ? classes.add(c) : classes.delete(c)),
      },
      dataset: {} as Record<string, string>,
      style: {} as Record<string, string>,
    } as unknown as HTMLElement;
    applyTheme(root, "dark", "asc-dark");
    expect(classes.has("dark")).toBe(true);
    expect(root.dataset.theme).toBe("asc-dark");
  });
  test("asc-auto repeats the dark tokens exactly", async () => {
    const css = await Bun.file(new URL("../src/styles/app.css", import.meta.url)).text();
    const block = (re: RegExp) => [...(css.match(re)?.[1] ?? "").matchAll(/--[\w-]+:[^;]+;/g)].map((x) => x[0].replace(/\s+/g, " "));
    const dark = block(/\[data-theme="asc-dark"\],\s*\.dark \{([^}]*)\}/);
    const auto = block(/ASC-DARK-AUTO-BEGIN \*\/[\s\S]*?\[data-theme="asc-auto"\] \{([^}]*)\}/);
    expect(dark.length).toBeGreaterThan(20);
    expect(auto).toEqual(dark);
    // The light block defines every token the dark block overrides.
    const light = block(/:root,\s*\[data-theme="asc-light"\] \{([^}]*)\}/).map((d) => d.split(":")[0]);
    for (const d of dark) expect(light).toContain(d.split(":")[0]);
  });
});

describe("agent features switch", () => {
  test("off by default, round trips, survives blocked storage", () => {
    const { m, s } = memStorage();
    expect(readAgents(s)).toBe(false);
    writeAgents(s, true);
    expect(m.get(AGENTS_KEY)).toBe("1");
    expect(readAgents(s)).toBe(true);
    writeAgents(s, false);
    expect(m.has(AGENTS_KEY)).toBe(false);
    const fail = () => {
      throw new Error("blocked");
    };
    expect(readAgents({ getItem: fail })).toBe(false);
    expect(() => writeAgents({ setItem: fail, removeItem: fail }, true)).not.toThrow();
  });
});

describe("auth store", () => {
  test("memory only unless asked to remember", () => {
    const { m, s } = memStorage();
    const a = createAuthStore(s);
    a.signIn("  tok  ", false);
    expect(a.token()).toBe("tok");
    expect(m.has(TOKEN_KEY)).toBe(false);
    expect(a.headers()).toEqual({ authorization: "Bearer tok" });
    a.signIn("tok", true);
    expect(m.get(TOKEN_KEY)).toBe("tok");
    expect(createAuthStore(s).token()).toBe("tok");
    a.signOut();
    expect(m.has(TOKEN_KEY)).toBe(false);
    expect(a.headers()).toEqual({});
  });
  test("notifies subscribers", () => {
    const a = createAuthStore(undefined);
    let n = 0;
    a.subscribe(() => n++);
    a.signIn("x", false);
    a.signOut();
    expect(n).toBe(2);
  });
});
