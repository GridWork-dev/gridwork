/**
 * The house design-token contract — `outputs/specs/house-standards/SPEC.md` D1.
 *
 * Every GridWork site keeps its OWN prefix and its OWN values. What is shared is the
 * vocabulary below and the theme mechanism: a reader who knows one site's tokens can
 * read another's, and a component written against these role names is portable.
 *
 * Roles are declared as `--<prefix>-<role>`. The prefix is per-site (`cs`, `gw`, `dp`, …)
 * and declared in that site's `tokens.config.json`. Bare names (`--bg`) are forbidden —
 * they collide with third-party CSS the moment a site imports one.
 */

/** The 24 roles every site MUST define, in both themes. Descriptions feed error output. */
export const REQUIRED_ROLES = {
  // ── surfaces ────────────────────────────────────────────────────────────
  bg: "page ground",
  surface: "card / panel over the ground",
  "surface-raised": "elevated panel (menu, popover, sticky bar)",
  "surface-sunken": "inset well (code block, input trough)",

  // ── text ────────────────────────────────────────────────────────────────
  text: "primary body + heading colour",
  "text-muted": "secondary — labels, captions, metadata",
  "text-faint": "tertiary — disabled, placeholder, watermark",

  // ── lines ───────────────────────────────────────────────────────────────
  border: "default hairline",
  "border-strong": "emphasis rule, active field, section divider",

  // ── brand ───────────────────────────────────────────────────────────────
  accent: "brand / primary action",
  "accent-hover": "accent under hover or press",
  "accent-contrast": "text and icons drawn ON accent",
  link: "inline link — distinct from accent so body copy stays readable",

  // ── state ───────────────────────────────────────────────────────────────
  focus: "focus-ring colour (a11y — never remove the ring, recolour it)",
  scrim: "overlay wash behind a modal or drawer",

  // ── status ──────────────────────────────────────────────────────────────
  danger: "destructive action, error state",
  warning: "caution, degraded state",
  success: "confirmed, healthy state",
  info: "neutral notice",

  // ── elevation ───────────────────────────────────────────────────────────
  "shadow-sm": "elevation 1 — resting card",
  "shadow-md": "elevation 2 — hovered card, dropdown",
  "shadow-lg": "elevation 3 — modal, command palette",

  // ── type ────────────────────────────────────────────────────────────────
  "font-sans": "body face",
  "font-mono": "code / tabular-data face",
} as const satisfies Record<string, string>;

export type Role = keyof typeof REQUIRED_ROLES;

export const REQUIRED_ROLE_NAMES = Object.keys(REQUIRED_ROLES) as Role[];

/**
 * Roles whose value cannot depend on theme polarity. A typeface is the same face in dark
 * mode, so demanding it in both alternate blocks buys a duplicate declaration that says
 * nothing — and a validator that mandates dead CSS teaches people to distrust it.
 * `dev-profile` hit this on first adoption (2026-08-13) and worked around it with a
 * comment; the fix belongs here.
 *
 * These are still REQUIRED on `:root`. Only the theme-gap check skips them. Shadows are
 * deliberately NOT here: elevation legitimately differs between themes.
 */
export const POLARITY_INDEPENDENT_ROLES = [
  "font-sans",
  "font-mono",
] as const satisfies readonly Role[];

/**
 * Scale families that must exist, with a minimum step count. Step NAMES and VALUES are
 * free — `--cs-space-4` and `--gw-space-md` both satisfy `space`. The contract is that a
 * scale exists and is a scale, not that two sites agree on how many rem a step is.
 *
 * `size` (not `text`) is the type-scale family, so it cannot collide with the `text`,
 * `text-muted`, and `text-faint` colour roles above.
 */
export const SCALE_FAMILIES = {
  space: 3,
  size: 3,
  radius: 3,
  duration: 3,
} as const satisfies Record<string, number>;

export type ScaleFamily = keyof typeof SCALE_FAMILIES;

export const SCALE_FAMILY_NAMES = Object.keys(SCALE_FAMILIES) as ScaleFamily[];

/**
 * The three-state theme mechanism. A site declares a `defaultTheme`; every required role
 * must resolve in the default theme AND in the alternate, reached BOTH ways:
 *
 *   :root                                              -> default theme
 *   @media (prefers-color-scheme: <alt>) {
 *     :root:not([data-theme="<default>"]) { … }        -> alternate, system-driven
 *   }
 *   :root[data-theme="<alt>"] { … }                    -> alternate, toggle-driven
 *
 * Both alternate paths are required. A site with only the media query cannot honour an
 * explicit user toggle; a site with only the attribute ignores the OS setting until the
 * user finds the toggle. Declaring a colour ONLY inside a media or [data-theme] block is
 * the specific bug this catches — it leaves the token undefined in the default state.
 */
export const THEME_MECHANISM = {
  /** Selectors that establish the default theme. */
  baseSelector: /^\s*(:root|html)\s*$/,
  /** A selector that keys on the explicit toggle. */
  attrSelector: /\[data-theme\s*[~^|*$]?=/,
  /** An at-rule that keys on the OS setting. */
  mediaQuery: /prefers-color-scheme\s*:\s*(dark|light)/,
} as const;
