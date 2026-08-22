// T28 reference: the operator deploy receipt (doc 07 §11).
//
// COPY VERBATIM into each app repo as `deploy/verify-receipt.ts`. This is the one script
// in the pipeline that is NOT per-repo: it reads no manifest and knows nothing about which
// services a repo owns, so four divergent hand-written copies of a constant-time
// comparison would be four chances to get it subtly wrong for no gain.
//
// WHY IT EXISTS. Doc 07 §11 names the GitHub `production` environment's required reviewers
// as the operator gate receipt. Environment protection is plan-gated and on this account
// only a PUBLIC repo gets it — measured 2026-08-21, three of the four app repos carry ZERO
// protection rules on `production`, so any `workflow_dispatch` reached production
// unreviewed. The gate therefore moves into the workflow: the dispatcher supplies a
// `receipt` input, this script compares it against the `PROD_DEPLOY_RECEIPT` environment
// secret, and a mismatch fails the run before `auth` — before a production credential is
// ever minted.
//
// WHAT IT DOES NOT BUY, stated so it is not overread: this proves the dispatcher held a
// secret, not that a second person reviewed the change. It is a possession check, not a
// four-eyes gate, and on a single-operator account those were never the same thing. It
// closes "any dispatch reaches production unreviewed". It does not close "the operator can
// deploy without a second opinion", which no mechanism available here closes.
//
// THE COMPARISON is SHA-256-prehash + `timingSafeEqual`, per identity/security.md. Both
// sides are variable-length operator-supplied strings, and `timingSafeEqual` on buffers of
// unequal length THROWS rather than returning false — caught, that exception is itself a
// length oracle, so the digests (always 32 bytes) are what get compared. Neither value is
// written to stdout, stderr, an annotation, or a step output on any path.

import { createHash, timingSafeEqual } from "node:crypto";

// Deliberately does NOT import `reportCliError` from `./manifest.ts`, which is where every
// other deploy script gets it. `manifest.ts` imports zod, and this script runs BEFORE the
// dependency-install step so that the receipt is checked before anything else in the job
// happens at all. Zero imports outside `node:` is what buys that position.
function reportCliError(error: unknown): void {
  const message = error instanceof Error ? error.message : "unknown failure";
  process.stderr.write(`error: ${message}\n`);
  process.exitCode = 1;
}

// GitHub applies no minimum length to a secret. A four-character receipt is guessable in
// far fewer dispatches than this workflow would refuse, so the length floor is part of the
// gate rather than hygiene. It is an assertion about the CONFIGURATION: its failure names
// the secret and never its value.
const MINIMUM_RECEIPT_LENGTH = 32;

export function receiptMatches(submitted: string, expected: string): boolean {
  const a = createHash("sha256").update(submitted, "utf8").digest();
  const b = createHash("sha256").update(expected, "utf8").digest();
  return timingSafeEqual(a, b);
}

async function main(): Promise<void> {
  // Deliberately NOT trimmed on either side. Trimming validates one value and compares
  // another — the same shape of defect `validate-rollback.ts` carried, where a trimmed
  // input passed the check and the untrimmed one reached the command. Whitespace is part
  // of the receipt.
  const submitted = process.env.RECEIPT;
  const expected = process.env.PROD_DEPLOY_RECEIPT;

  // Check the SECRET before the input, and state precisely what this check buys — it was
  // written as a fail-closed guard and mutation-testing on 2026-08-21 showed that framing
  // was wrong. Deleting it does NOT open a bypass: an unset `PROD_DEPLOY_RECEIPT` still
  // fails, because `createHash().update(undefined)` throws, and an empty one still fails
  // on the `!submitted` line below. What deleting it DOES open is a four-character
  // `PROD_DEPLOY_RECEIPT` being accepted — measured, exit 0 — which is guessable in far
  // fewer dispatches than this workflow would refuse. So: the length floor is the
  // load-bearing half, and naming the misconfigured secret instead of surfacing a crypto
  // TypeError is the other half. Neither is "fail closed"; that part was already true.
  if (!expected || expected.length < MINIMUM_RECEIPT_LENGTH) {
    throw new Error(
      `PROD_DEPLOY_RECEIPT is unset or shorter than ${MINIMUM_RECEIPT_LENGTH} characters on the production environment of this repo`,
    );
  }
  if (!submitted) throw new Error("receipt is required");
  if (!receiptMatches(submitted, expected)) {
    throw new Error("receipt does not match; production deploy refused");
  }
}

if (import.meta.main) main().catch(reportCliError);
