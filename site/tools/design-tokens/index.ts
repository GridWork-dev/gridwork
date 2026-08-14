/**
 * The design-token validator — `outputs/specs/house-standards/SPEC.md` D1, acceptance 1.
 *
 * Reads a site's `tokens.config.json` plus its CSS and fails on the four things that make
 * a token system drift: a missing required role, a role that exists in only one theme, a
 * role name nobody registered (the typo case), and a bare `var(--x)` reference that will
 * collide with the next third-party stylesheet the site imports.
 *
 * Values are never inspected. Two sites are conformant with completely different palettes;
 * that is the point of the standard.
 *
 *   bun tools/design-tokens/index.ts <path/to/tokens.config.json>
 */

import { dirname, join, relative } from "node:path";
import postcss, { type AtRule, type Declaration, type Rule } from "postcss";
import {
  POLARITY_INDEPENDENT_ROLES,
  REQUIRED_ROLES,
  REQUIRED_ROLE_NAMES,
  SCALE_FAMILIES,
  SCALE_FAMILY_NAMES,
  THEME_MECHANISM,
  type ScaleFamily,
} from "./contract.ts";

export interface TokensConfig {
  /** Per-site custom-property prefix, without dashes — `cs`, `gw`, `dp`. */
  prefix: string;
  /** Globs, relative to the config file, of every CSS file that declares or uses tokens. */
  css: string[];
  /** Which theme plain `:root` establishes. The other one is the alternate. */
  defaultTheme: "light" | "dark";
  /** Role suffixes this site defines beyond the contract — `chart-1`, `code-keyword`. */
  extensions?: string[];
  /** Third-party custom-property prefixes this site legitimately reads — `fd`, `tw`. */
  foreignPrefixes?: string[];
}

export interface Finding {
  code:
    | "missing-role"
    | "single-theme"
    | "theme-gap"
    | "undeclared-role"
    | "bare-var"
    | "thin-scale";
  message: string;
  file?: string;
  line?: number;
}

export interface CssFile {
  path: string;
  css: string;
}

type Scope = "base" | "altMedia" | "altAttr" | "local";

/**
 * Which of the three theme states a declaration lands in. A media query wins over the
 * selector, because the canonical alternate block nests `:root:not([data-theme="…"])`
 * INSIDE `@media (prefers-color-scheme: …)` and would otherwise read as attribute-driven.
 */
function scopeOf(decl: Declaration, altTheme: string): Scope {
  let selector: string | undefined;
  for (let node = decl.parent; node && node.type !== "root"; node = node.parent) {
    if (node.type === "atrule") {
      const at = node as AtRule;
      const match = THEME_MECHANISM.mediaQuery.exec(at.params);
      if (match && match[1] === altTheme) return "altMedia";
      continue;
    }
    if (node.type === "rule" && selector === undefined) {
      selector = (node as Rule).selector;
    }
  }
  if (selector === undefined) return "local";
  if (THEME_MECHANISM.baseSelector.test(selector)) return "base";
  if (THEME_MECHANISM.attrSelector.test(selector) && selector.includes(altTheme)) {
    return "altAttr";
  }
  return "local";
}

/** `space-4` -> `space`, if the first segment names a scale family. */
function familyOf(suffix: string): ScaleFamily | undefined {
  const head = suffix.split("-")[0];
  return SCALE_FAMILY_NAMES.includes(head as ScaleFamily) ? (head as ScaleFamily) : undefined;
}

const VAR_REF = /var\(\s*(--[A-Za-z0-9_-]+)/g;

export function validateTokens(config: TokensConfig, files: CssFile[]): Finding[] {
  const findings: Finding[] = [];
  const altTheme = config.defaultTheme === "dark" ? "light" : "dark";
  const own = `--${config.prefix}-`;
  const extensions = new Set(config.extensions ?? []);
  const foreign = new Set(config.foreignPrefixes ?? []);

  /** role suffix -> the theme scopes it is declared in */
  const declared = new Map<string, Set<Scope>>();
  /** scale family -> distinct step suffixes seen */
  const scaleSteps = new Map<ScaleFamily, Set<string>>();

  for (const file of files) {
    const root = postcss.parse(file.css, { from: file.path });

    root.walkDecls((decl) => {
      // ── every var() reference must be namespaced ──────────────────────────
      for (const [, name] of decl.value.matchAll(VAR_REF)) {
        if (name === undefined || name.startsWith(own)) continue;
        const head = name.slice(2).split("-")[0];
        if (head !== undefined && foreign.has(head)) continue;
        findings.push({
          code: "bare-var",
          message: `\`${name}\` is not namespaced — write \`${own}…\`, or declare \`${head}\` in foreignPrefixes if it belongs to a third party`,
          file: file.path,
          line: decl.source?.start?.line,
        });
      }

      if (!decl.prop.startsWith("--")) return;

      // ── declarations of OUR tokens: name-check and scope-track ────────────
      if (!decl.prop.startsWith(own)) return;
      const suffix = decl.prop.slice(own.length);

      const family = familyOf(suffix);
      if (family !== undefined) {
        const steps = scaleSteps.get(family) ?? new Set<string>();
        steps.add(suffix);
        scaleSteps.set(family, steps);
      } else if (!REQUIRED_ROLE_NAMES.includes(suffix as never) && !extensions.has(suffix)) {
        findings.push({
          code: "undeclared-role",
          message: `\`${decl.prop}\` is neither a contract role nor a declared extension — add \`${suffix}\` to \`extensions\` in tokens.config.json, or fix the typo`,
          file: file.path,
          line: decl.source?.start?.line,
        });
      }

      const scopes = declared.get(suffix) ?? new Set<Scope>();
      scopes.add(scopeOf(decl, altTheme));
      declared.set(suffix, scopes);
    });
  }

  // ── the site must actually be dual-theme ────────────────────────────────
  const hasAlt = [...declared.values()].some((s) => s.has("altMedia") || s.has("altAttr"));
  if (!hasAlt) {
    findings.push({
      code: "single-theme",
      message: `no ${altTheme}-theme tokens found — a site must define both themes, reachable by \`@media (prefers-color-scheme: ${altTheme})\` and by \`[data-theme="${altTheme}"]\``,
    });
  }

  // ── every required role, in every state ─────────────────────────────────
  for (const role of REQUIRED_ROLE_NAMES) {
    const scopes = declared.get(role);
    if (scopes === undefined || !scopes.has("base")) {
      findings.push({
        code: "missing-role",
        message: `\`${own}${role}\` is not defined on \`:root\` — ${REQUIRED_ROLES[role]}`,
      });
      continue;
    }
    if (!hasAlt) continue; // already reported once, as single-theme
    if (POLARITY_INDEPENDENT_ROLES.includes(role as never)) continue; // a face is a face
    const gaps: string[] = [];
    if (!scopes.has("altMedia")) {
      gaps.push(`@media (prefers-color-scheme: ${altTheme})`);
    }
    if (!scopes.has("altAttr")) gaps.push(`[data-theme="${altTheme}"]`);
    if (gaps.length > 0) {
      findings.push({
        code: "theme-gap",
        message: `\`${own}${role}\` has no ${altTheme} value under ${gaps.join(" or ")} — it will fall through to the ${config.defaultTheme} value`,
      });
    }
  }

  // ── scale families must be scales ───────────────────────────────────────
  for (const [family, min] of Object.entries(SCALE_FAMILIES)) {
    const seen = scaleSteps.get(family as ScaleFamily)?.size ?? 0;
    if (seen < min) {
      findings.push({
        code: "thin-scale",
        message: `scale family \`${family}\` has ${seen} step(s); the contract requires at least ${min} (\`${own}${family}-…\`)`,
      });
    }
  }

  return findings;
}

// ── CLI ───────────────────────────────────────────────────────────────────
if (import.meta.main) {
  const configPath = process.argv[2];
  if (configPath === undefined) {
    process.stderr.write("usage: bun tools/design-tokens/index.ts <path/to/tokens.config.json>\n");
    process.exit(2);
  }

  const config = (await Bun.file(configPath).json()) as TokensConfig;
  const root = dirname(configPath);

  const files: CssFile[] = [];
  for (const pattern of config.css) {
    for (const hit of new Bun.Glob(pattern).scanSync({ cwd: root })) {
      const abs = join(root, hit);
      files.push({ path: relative(root, abs), css: await Bun.file(abs).text() });
    }
  }

  if (files.length === 0) {
    process.stderr.write(`no CSS matched ${config.css.join(", ")} under ${root}\n`);
    process.exit(2);
  }

  const findings = validateTokens(config, files);
  for (const f of findings) {
    const where = f.file ? `${f.file}:${f.line ?? 0}: ` : "";
    process.stderr.write(`${where}[${f.code}] ${f.message}\n`);
  }
  process.stdout.write(
    findings.length === 0
      ? `design-tokens: ${files.length} file(s) conform to the house contract\n`
      : `design-tokens: ${findings.length} finding(s) across ${files.length} file(s)\n`,
  );
  process.exit(findings.length === 0 ? 0 : 1);
}
