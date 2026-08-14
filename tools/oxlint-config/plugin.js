/**
 * The `gridwork` oxlint JS plugin — the house security floor as lint rules
 * (`outputs/specs/house-standards/SPEC.md` D2b).
 *
 * Ported from `@gridwork/eslint-config` when ESLint left the house line. The rules and
 * their selectors are unchanged; only the host moved. Dropping typescript-eslint is what
 * frees every repo from the dual-TypeScript install — typescript-eslint peers
 * `typescript >=4.8.4 <6.1.0`, oxlint peers nothing.
 *
 * oxlint has no native `no-restricted-syntax` / `no-restricted-properties`, so these ship
 * as plugin rules. The esquery selectors are the ones ESLint used, verbatim.
 *
 * NOT here, on purpose: import-boundary denylists and brand/anti-slop guards. Those are
 * product-shaped — caisson's provider-SDK boundary is meaningless in a portfolio site —
 * and each repo composes its own on top of this base.
 */

const EXEC_MESSAGE =
  "Command injection: an interpolated template literal in exec/execSync. Use execFile(cmd, [args]) — identity/security.md.";

/** Shell execution: `execSync(`git log --grep="${input}"`)` is command injection. */
const noInterpolatedExec = {
  create(context) {
    const report = (node) => context.report({ node, message: EXEC_MESSAGE });
    return {
      "CallExpression[callee.name=/^exec(Sync)?$/] > TemplateLiteral[expressions.length>0]": report,
      // Member form is `execSync` only, never bare `exec` — `db.exec(sql)` and
      // `regex.exec(s)` are unrelated APIs that a bare-`exec` selector floods with false
      // positives. A bare imported `exec()` is still caught by the selector above.
      "CallExpression[callee.property.name='execSync'] > TemplateLiteral[expressions.length>0]":
        report,
    };
  },
};

/** AbortSignal.timeout leaks on Bun; every outbound fetch uses fetchWithTimeout. */
const noAbortsignalTimeout = {
  create(context) {
    return {
      "MemberExpression[object.name='AbortSignal'][property.name='timeout']": (node) =>
        context.report({
          node,
          message: "AbortSignal.timeout is forbidden on Bun — use fetchWithTimeout(url, init, ms).",
        }),
    };
  },
};

/**
 * Opt-in: bans Bun-runtime-only APIs in code that must also run on Node.
 * Off in the base config — a Bun-only repo has no reason to pay for it.
 */
const FORBIDDEN_BUN = ["serve", "file", "sleep", "write"];
const noBunRuntimeOnlyApis = {
  create(context) {
    return {
      [`MemberExpression[object.name='Bun'][property.name=/^(${FORBIDDEN_BUN.join("|")})$/]`]: (
        node,
      ) =>
        context.report({
          node,
          message:
            "Bun.* runtime-only API; use the Node-compatible equivalent (node:fs, node:timers, undici fetch, http.createServer).",
        }),
    };
  },
};

export default {
  meta: { name: "gridwork" },
  rules: {
    "no-interpolated-exec": noInterpolatedExec,
    "no-abortsignal-timeout": noAbortsignalTimeout,
    "no-bun-runtime-only-apis": noBunRuntimeOnlyApis,
  },
};
