import { afterEach, describe, expect, test } from "bun:test";
import { GET } from "./route";

const saved = process.env.GRIDWORK_GIT_SHA;
afterEach(() => {
  if (saved === undefined) delete process.env.GRIDWORK_GIT_SHA;
  else process.env.GRIDWORK_GIT_SHA = saved;
});

async function body(): Promise<{ status: string; sha: string }> {
  return (await GET().json()) as { status: string; sha: string };
}

describe("/health", () => {
  test("reports the SHA the build was handed", async () => {
    process.env.GRIDWORK_GIT_SHA = "0123456789abcdef0123456789abcdef01234567";
    expect(await body()).toEqual({ status: "ok", sha: "0123456789abcdef0123456789abcdef01234567" });
  });

  test("says unknown when no SHA was set", async () => {
    delete process.env.GRIDWORK_GIT_SHA;
    expect((await body()).sha).toBe("unknown");
  });

  // An empty SHA reported as a SHA is the failure the route exists to prevent.
  test("says unknown for an empty SHA, not the empty string", async () => {
    process.env.GRIDWORK_GIT_SHA = "";
    expect((await body()).sha).toBe("unknown");
  });
});
