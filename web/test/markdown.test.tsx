import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";
import Markdown, { classifyHref, parseBlocks } from "../src/components/markdown";

describe("classifyHref", () => {
  const blocked = [
    "javascript:alert(1)",
    "JavaScript:alert(1)",
    "JAVASCRIPT:alert(1)",
    "  javascript:alert(1)",
    "java\nscript:alert(1)",
    "java\tscript:alert(1)",
    "java\r\nscript:alert(1)",
    "\u0001javascript:alert(1)",
    "\u0000javascript:alert(1)",
    "jav​ase​cript:alert(1)",
    " javascript:alert(1)",
    "data:text/html,<script>alert(1)</script>",
    "DATA:text/html;base64,PHNjcmlwdD4=",
    "vbscript:msgbox(1)",
    "VbScript:msgbox(1)",
    "file:///etc/passwd",
    "blob:https://x/uuid",
    "ftp://host/file",
    "//evil.example/x",
    "\\\\evil.example\\x",
    "/\\evil.example",
    "",
    "   ",
  ];
  for (const h of blocked) {
    test(`blocks ${JSON.stringify(h)}`, () => {
      expect(classifyHref(h).kind).toBe("blocked");
    });
  }

  test("allows http, https and mailto in any case", () => {
    expect(classifyHref("https://example.org/a?b=1").kind).toBe("external");
    expect(classifyHref("HTTP://example.org").kind).toBe("external");
    expect(classifyHref("mailto:a@b.c").kind).toBe("external");
    expect(classifyHref("ht\ntps://example.org/").kind).toBe("external");
  });

  test("anchors and relative paths stay inside the app", () => {
    expect(classifyHref("#install")).toEqual({ kind: "anchor", href: "#install" });
    expect(classifyHref("docs/guide.md")).toEqual({ kind: "relative", href: "docs/guide.md" });
    expect(classifyHref("./docs/guide.md").kind).toBe("relative");
    expect(classifyHref("../x").kind).toBe("relative");
  });
});

describe("Markdown rendering", () => {
  const hostile = [
    "[x](javascript:alert(1))",
    "[x](JaVaScRiPt:alert(1))",
    "[x](java\nscript:alert(1))",
    "[x](data:text/html,hi)",
    "[x](vbscript:run)",
    "![x](javascript:alert(1))",
    "![x](https://tracker.example/pixel.png)",
    "<script>alert(1)</script>",
    "<img src=x onerror=alert(1)>",
  ];
  for (const md of hostile) {
    test(`no executable output for ${JSON.stringify(md)}`, () => {
      const html = renderToStaticMarkup(<Markdown source={md} />);
      expect(html).not.toMatch(/href="\s*(javascript|data|vbscript):/i);
      expect(html).not.toContain("<script");
      expect(html).not.toContain("<img");
      expect(html).not.toMatch(/<[a-z][^>]*\ssrc=/i);
      expect(html).not.toContain('onerror="');
    });
  }

  test("a blocked link keeps its text, an allowed one is an anchor with rel", () => {
    const html = renderToStaticMarkup(
      <Markdown source={"[bad](javascript:x) [ok](https://example.org)"} />,
    );
    expect(html).toContain("<span>bad</span>");
    expect(html).toContain('href="https://example.org/"');
    expect(html).toContain('rel="noreferrer noopener"');
    expect(html).toContain('target="_blank"');
  });

  test("inline HTML is text, not markup", () => {
    const html = renderToStaticMarkup(<Markdown source={"<b>hi</b>"} />);
    expect(html).toContain("&lt;b&gt;hi&lt;/b&gt;");
  });

  test("parses headings, lists, fences", () => {
    const b = parseBlocks("# T\n\n- a\n- b\n\n```ts\nx\n```\n");
    expect(b.map((x) => x.t)).toEqual(["h", "ul", "code"]);
  });
});
