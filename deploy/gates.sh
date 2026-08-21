#!/usr/bin/env bash
set -euo pipefail

bun install --frozen-lockfile --cwd site
./tools/lint.sh .
bun run --cwd site typecheck
site/node_modules/.bin/tsc -p deploy/tsconfig.json
bun run --cwd site test
bun test deploy/
