// The house design-token contract, applied to site/. Lane E item 2, house-standards
// acceptance 7: every required role defined in both themes, no unregistered role name,
// no bare `var()` that a third-party stylesheet could collide with.
//
// The validator itself is VENDORED and not editable here — tools/vendored-config-drift.test.ts
// hashes it against the canonical copy. This file is the part that is ours: pointing it at
// this site and asserting the verdict.
//
// It lives in tools/ rather than site/ so the existing `bun test tools/` CI step picks it up
// with no workflow edit. The validator lives under site/ instead, because it imports postcss
// and Bun resolves that by walking up from the IMPORTED file — there is no root package.json
// here, so a copy at tools/design-tokens/ would find no node_modules at all.
import { describe, expect, it } from "bun:test";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";

import { validateTokens, type CssFile, type TokensConfig } from "../site/tools/design-tokens/index.ts";

const SITE = resolve(import.meta.dir, "..", "site");

const config = JSON.parse(readFileSync(join(SITE, "tokens.config.json"), "utf8")) as TokensConfig;
const files: CssFile[] = config.css.map((path) => ({
  path,
  css: readFileSync(join(SITE, path), "utf8"),
}));

describe("site/ conforms to the house design-token contract", () => {
  // Asserted before the verdict, and this order is the point. `validateTokens` over zero
  // files returns zero findings and reads as a pass — the same empty-fold that lets a
  // typechecker with no input exit 0. A config whose glob stopped matching would otherwise
  // report this site as conformant forever.
  it("actually read the stylesheet", () => {
    expect(files).toHaveLength(1);
    expect(files[0]?.css.length ?? 0).toBeGreaterThan(1000);
  });

  it("reports no findings", () => {
    const findings = validateTokens(config, files);
    const detail = findings.map((f) => `[${f.code}] ${f.message}`).join("\n");
    expect(findings, detail).toHaveLength(0);
  });

  // A negative control. The two assertions above pass equally well against a validator that
  // was vendored as an empty file, or one whose import silently resolved to something inert.
  // This proves the code under test can still fail — the house rule that a guard nobody has
  // watched fail is a guard nobody has tested.
  it("still rejects a stylesheet that is missing a role", () => {
    const findings = validateTokens(config, [
      { path: "fixture.css", css: ":root { --gws-bg: #000 }\n[data-theme='light'] { --gws-bg: #fff }" },
    ]);
    expect(findings.some((f) => f.code === "missing-role")).toBe(true);
  });
});
