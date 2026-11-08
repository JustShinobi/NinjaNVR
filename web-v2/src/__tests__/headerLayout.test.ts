import { describe, expect, test } from "bun:test";
import { en } from "../i18n/en";
import { pt } from "../i18n/pt";

async function readSrcFile(relativePathFromTests: string): Promise<string> {
  const fileUrl = new URL(relativePathFromTests, import.meta.url);
  const bunGlobal = (globalThis as unknown as { Bun: { file(url: URL): { text(): Promise<string> } } }).Bun;
  return bunGlobal.file(fileUrl).text();
}

/** Body of the first `@media (max-width: Npx)` block, or "" when absent. */
function mediaBlock(css: string, maxWidth: number): string {
  const start = css.indexOf(`@media (max-width: ${maxWidth}px) {`);
  if (start < 0) return "";
  let depth = 0;
  for (let i = css.indexOf("{", start); i < css.length; i++) {
    if (css[i] === "{") depth += 1;
    else if (css[i] === "}") {
      depth -= 1;
      if (depth === 0) return css.slice(start, i + 1);
    }
  }
  return "";
}

describe("desktop header collapses instead of scrolling sideways", () => {
  test("every tab has a short name in both locales, no longer than the full one", () => {
    for (const dict of [en, pt]) {
      const keys = Object.keys(dict.nav) as Array<keyof typeof dict.nav>;
      expect(Object.keys(dict.navShort).sort()).toEqual([...keys].sort());
      for (const k of keys) {
        expect(dict.navShort[k].trim().length).toBeGreaterThan(0);
        expect(dict.navShort[k].length).toBeLessThanOrEqual(dict.nav[k].length);
      }
    }
  });

  test("collapse steps hide secondary header content in order", async () => {
    const css = await readSrcFile("../styles.css");
    expect(css).toMatch(/\.nvr-nav-short\s*\{\s*display:\s*none;/);
    expect(mediaBlock(css, 1860)).toMatch(/\.nvr-nav \.nvr-kbd/);
    expect(mediaBlock(css, 1860)).toMatch(/\.nvr-header-btn \.nvr-header-label/);
    expect(mediaBlock(css, 1500)).toMatch(/\.nvr-brand-version/);
    expect(mediaBlock(css, 1500)).toMatch(/\.nvr-telemetry-ext/);
    expect(mediaBlock(css, 1400)).toMatch(/\.nvr-telemetry-perf/);
    const shortStep = mediaBlock(css, 1180);
    expect(shortStep).toMatch(/\.nvr-nav-short\s*\{\s*display:\s*inline;/);
    // The full tab name stays in the accessibility tree: visually hidden, never display:none.
    const label = shortStep.match(/\.nvr-nav-label\s*\{([^}]*)\}/)?.[1] || "";
    expect(label).toMatch(/clip-path:\s*inset\(50%\)/);
    expect(label).not.toMatch(/display:\s*none/);
  });

  test("tab buttons render both names and icon-only buttons keep an accessible name", async () => {
    const app = await readSrcFile("../App.tsx");
    expect((app.match(/className="nvr-nav-label"/g) || []).length).toBe(7);
    expect((app.match(/className="nvr-nav-short" aria-hidden="true"/g) || []).length).toBe(7);
    expect(app).toMatch(/aria-label="Cycle Console Theme \(Dark \/ Light \/ System\)"/);
    expect(app).toMatch(/aria-label="Log out of NinjaNVR Console"/);
    expect((app.match(/className="nvr-telemetry-perf"/g) || []).length).toBe(2);
  });

  test("hiding the brand title is limited to the desktop range", async () => {
    const css = await readSrcFile("../styles.css");
    expect(css).toMatch(/@media \(min-width: 768px\) and \(max-width: 900px\) \{\s*\.nvr-header \.nvr-brand-title/);
    expect(css).not.toMatch(/@media \(max-width: 900px\) \{\s*\.nvr-header \.nvr-brand-title/);
  });
});
