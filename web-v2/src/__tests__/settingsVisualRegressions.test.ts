import { describe, expect, test } from "bun:test";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { ToggleSwitch } from "../views/settings/ConsoleAndStreamsPanels";
import { SheetSwitch } from "../components/CameraDetailsSheet";

async function readSrcFile(relativePathFromTests: string): Promise<string> {
  const fileUrl = new URL(relativePathFromTests, import.meta.url);
  const bunGlobal = (globalThis as unknown as { Bun: { file(url: URL): { text(): Promise<string> } } }).Bun;
  return bunGlobal.file(fileUrl).text();
}

interface TopLevelRule {
  selector: string;
  line: number;
  props: Record<string, string>;
}

/** Collects rules that are not nested in any at-rule (`@media`, `@supports`, …). */
function topLevelRules(css: string): TopLevelRule[] {
  const src = css.replace(/\/\*[\s\S]*?\*\//g, (m) => m.replace(/[^\n]/g, " "));
  const rules: TopLevelRule[] = [];
  const stack: string[] = [];
  const token = /([^{}]*)\{|\}/g;
  let m: RegExpExecArray | null;
  while ((m = token.exec(src))) {
    if (m[0] === "}") {
      stack.pop();
      continue;
    }
    const prelude = m[1].trim();
    const nestedInAtRule = stack.some((s) => s.startsWith("@"));
    stack.push(prelude);
    if (prelude.startsWith("@") || nestedInAtRule) continue;
    const end = src.indexOf("}", token.lastIndex);
    const body = src.slice(token.lastIndex, end);
    const props: Record<string, string> = {};
    for (const decl of body.split(";")) {
      const idx = decl.indexOf(":");
      if (idx > 0) props[decl.slice(0, idx).trim()] = decl.slice(idx + 1).trim();
    }
    const line = src.slice(0, m.index).split("\n").length;
    for (const sel of prelude.split(",")) rules.push({ selector: sel.trim(), line, props });
  }
  return rules;
}

const BOX_PROPS = ["width", "height", "padding", "border-radius"] as const;

describe("settings visual regressions", () => {
  test("no top-level selector is redefined later with a different box size", async () => {
    // A second `.nvr-switch` block (52x32, added for the camera details sheet) silently
    // overrode the 38x20 settings switch, leaving its 14px thumb stuck to the top edge.
    const css = await readSrcFile("../styles.css");
    const bySelector = new Map<string, TopLevelRule[]>();
    for (const r of topLevelRules(css)) {
      bySelector.set(r.selector, [...(bySelector.get(r.selector) || []), r]);
    }
    const conflicts: string[] = [];
    for (const [selector, list] of bySelector) {
      for (const later of list.slice(1)) {
        for (const prop of BOX_PROPS) {
          const first = list[0].props[prop];
          const next = later.props[prop];
          if (first !== undefined && next !== undefined && first !== next) {
            conflicts.push(`${selector} ${prop}: line ${list[0].line} "${first}" vs line ${later.line} "${next}"`);
          }
        }
      }
    }
    expect(conflicts).toEqual([]);
  });

  test("settings and sheet switches use separate class families", async () => {
    const settings = renderToStaticMarkup(
      React.createElement(ToggleSwitch, { checked: true, onChange: () => {}, label: "x" })
    );
    expect(settings).toContain('class="nvr-switch is-on"');
    expect(settings).toContain("nvr-switch-thumb");

    const sheet = renderToStaticMarkup(
      React.createElement(SheetSwitch, { checked: true, onChange: () => {}, label: "x" })
    );
    expect(sheet).toContain('class="nvr-sheet-switch on"');
    expect(sheet).toContain("nvr-sheet-switch-knob");
    expect(sheet).not.toMatch(/class="nvr-switch[\s"]/);

    const css = await readSrcFile("../styles.css");
    const switchRule = topLevelRules(css).find((r) => r.selector === ".nvr-switch");
    const thumbRule = topLevelRules(css).find((r) => r.selector === ".nvr-switch-thumb");
    // The thumb must be vertically centred in the track: top inset == (track - thumb - borders) / 2.
    const px = (v: string | undefined) => Number.parseFloat(v || "NaN");
    const track = px(switchRule?.props.height);
    const border = px(switchRule?.props.border?.split(" ")[0]);
    const thumb = px(thumbRule?.props.height);
    const top = px(thumbRule?.props.top);
    expect(top).toBeCloseTo((track - 2 * border - thumb) / 2, 5);
  });

  test("settings form grids let columns shrink so inputs stay inside their cards", async () => {
    for (const file of [
      "../views/settings/ConsoleAndStreamsPanels.tsx",
      "../views/settings/RecordingAndSecurityPanels.tsx",
      "../views/settings/VisionAndAiPanels.tsx",
    ]) {
      const src = await readSrcFile(file);
      // A bare `1fr` track is `minmax(auto, 1fr)`: it never shrinks below the input's intrinsic
      // width, which pushed ONVIF fields past the card edge at 1280–1600 px.
      const bare = src.match(/gridTemplateColumns: "(?:\d*\.?\d+fr\s*)+"/g) || [];
      expect({ file, bare }).toEqual({ file, bare: [] });
    }
  });
});
