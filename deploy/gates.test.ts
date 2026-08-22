import { expect, test } from "bun:test";

test("deployment gates discover tests without an uninstalled runner dependency", async () => {
  const source = await Bun.file(new URL("./gates.sh", import.meta.url)).text();

  expect(source).toContain("shopt -s nullglob globstar");
  expect(source).toContain("deploy_tests=(deploy/**/*.test.ts)");
  expect(source).toContain('test "${#deploy_tests[@]}" -gt 0');
  expect(source).not.toContain("rg --files");
});
