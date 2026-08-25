import { afterEach, beforeEach, expect, test } from "bun:test";
import { Buffer } from "node:buffer";
import { NextRequest } from "next/server";
import { matchesOriginSecret, originVerificationDisabledFor } from "./lib/origin-verification";
import { proxy } from "./proxy";

const currentSecret = Buffer.alloc(32, 0xa1).toString("base64url");
const nextSecret = Buffer.alloc(32, 0xb2).toString("base64url");
const wrongSecret = Buffer.alloc(32, 0xc3).toString("base64url");

const originalMode = process.env.GRIDWORK_ORIGIN_SECRET_MODE;
const originalCurrent = process.env.GRIDWORK_ORIGIN_SECRET_CURRENT;
const originalNext = process.env.GRIDWORK_ORIGIN_SECRET_NEXT;
const suiteMode = originalMode === "disabled" ? "disabled" : "required";

beforeEach(() => {
  process.env.GRIDWORK_ORIGIN_SECRET_MODE = suiteMode;
  process.env.GRIDWORK_ORIGIN_SECRET_CURRENT = currentSecret;
  process.env.GRIDWORK_ORIGIN_SECRET_NEXT = nextSecret;
});

afterEach(() => {
  restoreEnvironmentValue("GRIDWORK_ORIGIN_SECRET_MODE", originalMode);
  restoreEnvironmentValue("GRIDWORK_ORIGIN_SECRET_CURRENT", originalCurrent);
  restoreEnvironmentValue("GRIDWORK_ORIGIN_SECRET_NEXT", originalNext);
});

for (const path of [
  "/health",
  "/healthz",
  "/api/t30-probe",
  "/robots.txt",
  "/sitemap.xml",
  "/_next/static/chunks/app.js",
]) {
  test(`rejects a direct request to ${path}`, () => {
    const response = proxy(originRequest(path));

    expect(response.status).toBe(403);
  });
}

test("accepts the configured current origin secret", () => {
  const response = proxy(originRequest("/health", currentSecret));

  expect(response.headers.get("x-middleware-next")).toBe("1");
});

test("accepts the configured next origin secret during rotation", () => {
  const response = proxy(originRequest("/api/t30-probe", nextSecret));

  expect(response.headers.get("x-middleware-next")).toBe("1");
});

test("rejects a wrong fixed-length origin secret", () => {
  const response = proxy(originRequest("/healthz", wrongSecret));

  expect(response.status).toBe(403);
});

test("rejects a malformed origin secret", () => {
  const response = proxy(originRequest("/health", "not-base64url"));

  expect(response.status).toBe(403);
});

test("rejects a trailing line break before base64url decoding", () => {
  expect(matchesOriginSecret(`${currentSecret}\n`, [currentSecret])).toBe(false);
  expect(matchesOriginSecret(currentSecret, [`${currentSecret}\n`])).toBe(false);
});

test("rejects a wrong-length origin secret", () => {
  const wrongLength = Buffer.alloc(31, 0xd4).toString("base64url");
  const response = proxy(originRequest("/health", wrongLength));

  expect(response.status).toBe(403);
});

test("fails closed when origin verification mode and configuration are absent", () => {
  delete process.env.GRIDWORK_ORIGIN_SECRET_MODE;
  delete process.env.GRIDWORK_ORIGIN_SECRET_CURRENT;
  delete process.env.GRIDWORK_ORIGIN_SECRET_NEXT;

  const response = proxy(originRequest("/health"));

  expect(response.status).toBe(403);
});

test("fails closed when current configuration is absent during rotation", () => {
  delete process.env.GRIDWORK_ORIGIN_SECRET_CURRENT;

  const response = proxy(originRequest("/health", nextSecret));

  expect(response.status).toBe(403);
});

test("fails closed when current configuration is malformed during rotation", () => {
  process.env.GRIDWORK_ORIGIN_SECRET_CURRENT = "not-base64url";

  const response = proxy(originRequest("/health", nextSecret));

  expect(response.status).toBe(403);
});

test("allows the test-only mutation suite to disable origin verification", () => {
  expect(originVerificationDisabledFor("test", "disabled")).toBe(true);

  process.env.GRIDWORK_ORIGIN_SECRET_MODE = "disabled";
  delete process.env.GRIDWORK_ORIGIN_SECRET_CURRENT;
  delete process.env.GRIDWORK_ORIGIN_SECRET_NEXT;

  const response = proxy(originRequest("/healthz"));

  expect(response.headers.get("x-middleware-next")).toBe("1");
});

test("allows local development to disable origin verification", () => {
  expect(originVerificationDisabledFor("development", "disabled")).toBe(true);
});

test("does not allow production configuration to disable origin verification", () => {
  expect(originVerificationDisabledFor("production", "disabled")).toBe(false);
  expect(originVerificationDisabledFor(undefined, "disabled")).toBe(false);
});

function originRequest(path: string, presentedSecret?: string): NextRequest {
  const headers = new Headers();
  if (presentedSecret !== undefined) {
    headers.set("X-Gridwork-Origin-Secret", presentedSecret);
  }

  return new NextRequest(new URL(path, "https://gridwork.test"), { headers });
}

function restoreEnvironmentValue(name: string, value: string | undefined): void {
  if (value === undefined) {
    delete process.env[name];
  } else {
    process.env[name] = value;
  }
}
