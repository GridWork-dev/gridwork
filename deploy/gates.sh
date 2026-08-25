#!/usr/bin/env bash
set -euo pipefail

bun install --frozen-lockfile --cwd site
./tools/lint.sh .
bun run --cwd site typecheck

type_seed="$(mktemp -p deploy 'gate-typecheck.XXXXXX.ts')"
test_seed="$(mktemp -p deploy 'gate-test.XXXXXX.test.ts')"
case "$type_seed:$test_seed" in
  deploy/gate-typecheck.*.ts:deploy/gate-test.*.test.ts) ;;
  *) echo "deployment gate seeds escaped deploy/" >&2; exit 1 ;;
esac
cleanup() {
  rm -f -- "$type_seed" "$test_seed"
}
trap cleanup EXIT

printf '%s\n' 'const mutationMustFail: string = 42;' > "$type_seed"
if site/node_modules/.bin/tsc -p deploy/tsconfig.json >/dev/null 2>&1; then
  echo "deployment typecheck mutation unexpectedly passed" >&2
  exit 1
fi
rm -f -- "$type_seed"

printf '%s\n' \
  'import { expect, test } from "bun:test";' \
  'test("deployment gate mutation", () => expect(true).toBe(false));' > "$test_seed"
if bun test "$test_seed" >/dev/null 2>&1; then
  echo "deployment test mutation unexpectedly passed" >&2
  exit 1
fi
rm -f -- "$test_seed"
trap - EXIT

site/node_modules/.bin/tsc -p deploy/tsconfig.json
bun run --cwd site test
shopt -s nullglob globstar
deploy_tests=(deploy/**/*.test.ts)
test "${#deploy_tests[@]}" -gt 0
bun test deploy/
