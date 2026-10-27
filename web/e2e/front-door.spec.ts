// The front door: the signed-out product page, the signed-in dashboard, the global header, the
// account menu and the command palette. ASC_E2E_SHOTS=<dir> also saves full-page screenshots in both
// themes and viewports.
import { expect, type Page, test } from "@playwright/test";
import { axeViolations, watch } from "./helpers";

const shots = process.env.ASC_E2E_SHOTS;
const REPOS = ["acme/payments", "asc/demo", "acme/web", "acme/ledger", "asc/engine"];

/** A signed-in browser that has opened a few repositories (the mock transport knows them all). */
async function signedIn(page: Page): Promise<void> {
  await page.addInitScript((repos) => {
    sessionStorage.setItem("asc-token", "demo-token");
    localStorage.setItem("asc-recent", JSON.stringify(repos));
  }, REPOS);
}

interface Vitals {
  cls: number;
  lcp: number;
}

/** Layout shift and largest contentful paint, collected by observers installed before the page. */
async function observe(page: Page): Promise<void> {
  await page.addInitScript(() => {
    const v = { cls: 0, lcp: 0 };
    (window as unknown as { vitals: Vitals }).vitals = v;
    new PerformanceObserver((list) => {
      for (const e of list.getEntries() as unknown as {
        value: number;
        hadRecentInput: boolean;
      }[]) {
        if (!e.hadRecentInput) v.cls += e.value;
      }
    }).observe({ type: "layout-shift", buffered: true });
    new PerformanceObserver((list) => {
      for (const e of list.getEntries()) v.lcp = e.startTime;
    }).observe({ type: "largest-contentful-paint", buffered: true });
  });
}

const vitals = (page: Page) =>
  page.evaluate(() => (window as unknown as { vitals: Vitals }).vitals);

for (const scheme of ["light", "dark"] as const) {
  test.describe(`front door, ${scheme}`, () => {
    test.use({ colorScheme: scheme });

    for (const kind of ["landing", "dashboard"] as const) {
      test(`${kind} is clean, accessible and stable`, async ({ page }, info) => {
        const problems = watch(page);
        if (kind === "dashboard") await signedIn(page);
        await page.goto("/ui/", { waitUntil: "networkidle" });
        await expect(page.locator("main h1")).toHaveCount(1);
        await expect(page.locator("[aria-busy='true']")).toHaveCount(0);
        if (kind === "dashboard") {
          await expect(page.getByRole("region", { name: "Needs you" })).toBeVisible();
        }
        await page.waitForTimeout(250);
        const overflow = await page.evaluate(
          () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
        );
        expect(overflow, "horizontal page scroll").toBeLessThanOrEqual(0);
        expect(await axeViolations(page), "axe violations").toEqual([]);
        if (shots) {
          await page.screenshot({
            path: `${shots}/${kind}-${info.project.name}-${scheme}.png`,
            fullPage: true,
          });
        }
        expect(problems).toEqual([]);
      });
    }
  });
}

// Timing is measured one page at a time per project: the parallel functional tests would otherwise
// compete for the CPU and inflate the paint times.
test.describe("performance on the mock transport", () => {
  test.describe.configure({ mode: "serial" });
  for (const kind of ["landing", "dashboard"] as const) {
    test(`${kind}: no layout shift and a fast largest paint`, async ({ page }) => {
      await observe(page);
      if (kind === "dashboard") await signedIn(page);
      // A page in the background is not painted, and so reports no paint at all.
      await page.bringToFront();
      await page.goto("/ui/", { waitUntil: "networkidle" });
      await expect.poll(async () => (await vitals(page)).lcp, { timeout: 5000 }).toBeGreaterThan(0);
      await page.waitForTimeout(300);
      const v = await vitals(page);
      expect(v.cls, "cumulative layout shift").toBeLessThan(0.01);
      expect(v.lcp, "largest contentful paint (ms)").toBeLessThan(1500);
    });
  }
});

test.describe("product page", () => {
  test("section links scroll to their section", async ({ page, isMobile }) => {
    test.skip(isMobile, "the section links are in the menu on small screens");
    await page.goto("/ui/", { waitUntil: "networkidle" });
    await page.getByRole("banner").getByRole("link", { name: "Review", exact: true }).click();
    await expect(page).toHaveURL(/#review$/);
    await expect(page.locator("#review")).toBeInViewport();
  });

  test("the footer links go to real pages", async ({ page }) => {
    await page.goto("/ui/", { waitUntil: "networkidle" });
    const footer = page.locator("footer");
    for (const [name, url, heading] of [
      ["Docs", /\/ui\/docs$/, "Documentation"],
      ["Install", /\/ui\/install$/, "Install asc"],
      ["Licenses", /\/ui\/licenses$/, "Licenses"],
      ["Privacy", /\/ui\/privacy$/, "Privacy"],
    ] as const) {
      await footer.getByRole("link", { name, exact: true }).click();
      await expect(page).toHaveURL(url);
      await expect(page.getByRole("heading", { level: 1, name: heading })).toBeVisible();
      await page.goBack({ waitUntil: "networkidle" });
    }
  });

  test("the clone command is shown", async ({ page, isMobile }) => {
    await page.goto("/ui/", { waitUntil: "networkidle" });
    const clone = isMobile
      ? page.getByRole("region", { name: "Clone command" })
      : page.getByText("asc clone https://hub.example.com/acme/payments").first();
    await expect(clone).toBeVisible();
    await expect(clone).toContainText("asc clone https://hub.example.com/acme/payments");
  });
});

test.describe("signed in", () => {
  test.beforeEach(async ({ page }) => {
    await signedIn(page);
  });

  test("the dashboard shows what needs you, your stacks and today", async ({ page }) => {
    await page.goto("/ui/", { waitUntil: "networkidle" });
    await expect(page.getByRole("heading", { level: 1 })).toContainText("Good");
    await expect(page.getByRole("region", { name: "Needs you" })).toContainText(
      "Wire retry policy into the client",
    );
    await expect(page.getByRole("region", { name: "Your stacks" })).toContainText(
      "retry-with-backoff",
    );
    await expect(page.getByRole("region", { name: "Today" })).toContainText("pushed");
  });

  test("a repository name is opened from All repositories and remembered", async ({ page }) => {
    await page.goto("/ui/repos", { waitUntil: "networkidle" });
    await page.getByLabel("Repository name").fill("not a name");
    await page.getByRole("button", { name: "Open", exact: true }).click();
    await expect(page.getByText("Use the form owner/repo.")).toBeVisible();
    await page.getByLabel("Repository name").fill("acme/ledger");
    await page.getByRole("button", { name: "Open", exact: true }).click();
    await expect(page).toHaveURL(/\/ui\/acme\/ledger$/);
    const recent = await page.evaluate(() =>
      JSON.parse(localStorage.getItem("asc-recent") ?? "[]"),
    );
    expect(recent[0]).toBe("acme/ledger");
  });

  test("Ctrl+K opens the palette, filters, and Enter opens a repository", async ({
    page,
    isMobile,
  }) => {
    test.skip(isMobile, "no hardware keyboard");
    await page.goto("/ui/", { waitUntil: "networkidle" });
    await page.keyboard.press("Control+k");
    const dialog = page.getByRole("dialog", { name: "Command palette" });
    await expect(dialog).toBeVisible();
    await expect(page.getByRole("combobox")).toBeFocused();
    await page.keyboard.type("ledg");
    await expect(page.getByRole("option").first()).toContainText("acme/ledger");
    await page.keyboard.press("Enter");
    await expect(page).toHaveURL(/\/ui\/acme\/ledger$/);
    await expect(dialog).toHaveCount(0);
  });

  test("the palette offers an owner/repo that was never opened, and Escape closes it", async ({
    page,
    isMobile,
  }) => {
    test.skip(isMobile, "no hardware keyboard");
    await page.goto("/ui/", { waitUntil: "networkidle" });
    await page.keyboard.press("Control+k");
    await expect(page.getByRole("combobox")).toBeFocused();
    await page.keyboard.type("asc/web");
    await expect(page.getByRole("option").first()).toContainText("Open asc/web");
    await page.keyboard.press("Escape");
    await expect(page.getByRole("dialog")).toHaveCount(0);
  });

  test("the menu opens the palette on a small screen", async ({ page, isMobile }) => {
    test.skip(!isMobile, "desktop has the search box");
    await page.goto("/ui/", { waitUntil: "networkidle" });
    await page.getByRole("button", { name: "Open menu" }).click();
    await page.getByRole("button", { name: "Search or run a command" }).click();
    await expect(page.getByRole("dialog", { name: "Command palette" })).toBeVisible();
  });

  test("the account menu: arrow keys, theme, Escape and sign out", async ({ page, isMobile }) => {
    test.skip(isMobile, "small screens have a profile link and the menu drawer instead");
    await page.goto("/ui/", { waitUntil: "networkidle" });
    const button = page.getByRole("button", { name: "Account menu" });
    await button.focus();
    await page.keyboard.press("Enter");
    await expect(page.getByRole("menu", { name: "Account menu" })).toBeVisible();
    await expect(page.getByRole("menuitem", { name: /Dashboard/ })).toBeFocused();
    await page.keyboard.press("ArrowDown");
    await expect(page.getByRole("menuitem", { name: /Inbox/ })).toBeFocused();
    await page.keyboard.press("End");
    await expect(page.getByRole("menuitem", { name: "Sign out" })).toBeFocused();
    await page.keyboard.press("Escape");
    await expect(page.getByRole("menu")).toHaveCount(0);
    await expect(button).toBeFocused();

    await button.click();
    await page.getByRole("menuitemradio", { name: "Dark" }).click();
    await expect(page.locator("html")).toHaveClass(/dark/);
    await button.click();
    await expect(page.getByRole("menuitemradio", { name: "Dark" })).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await page.getByRole("menuitemradio", { name: "Match system" }).click();

    await button.click();
    await page.getByRole("menuitem", { name: "Sign out" }).click();
    await expect(page.getByRole("heading", { name: /Review code at the speed/ })).toBeVisible();
    expect(await page.evaluate(() => sessionStorage.getItem("asc-token"))).toBeNull();
  });

  test("the create menu opens the palette for a repository", async ({ page, isMobile }) => {
    test.skip(isMobile, "the header has no Create menu on small screens");
    await page.goto("/ui/", { waitUntil: "networkidle" });
    await page.getByRole("button", { name: "Create" }).click();
    await page.getByRole("menuitem", { name: /Open a repository/ }).click();
    await expect(page.getByRole("dialog", { name: "Command palette" })).toBeVisible();
  });

  test("the inbox badge counts the changes that are your turn", async ({ page }) => {
    await page.goto("/ui/", { waitUntil: "networkidle" });
    // The count is the number of "Needs you" rows.
    await expect(
      page.getByRole("heading", { level: 1 }).locator("xpath=following::*[1]"),
    ).toContainText("5 things need you");
    await expect(page.getByRole("link", { name: /Inbox/ }).first()).toContainText("5");
  });

  test("a repository you open is added to the dashboard", async ({ page }) => {
    await page.goto("/ui/asc/engine", { waitUntil: "networkidle" });
    const recent = await page.evaluate(() =>
      JSON.parse(localStorage.getItem("asc-recent") ?? "[]"),
    );
    expect(recent[0]).toBe("asc/engine");
  });

  test("/ focuses the search box", async ({ page, isMobile }) => {
    test.skip(isMobile, "no hardware keyboard");
    await page.goto("/ui/", { waitUntil: "networkidle" });
    await page.keyboard.press("/");
    await expect(page.getByRole("searchbox", { name: "Search" })).toBeFocused();
  });
});
