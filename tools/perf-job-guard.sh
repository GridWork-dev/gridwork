#!/usr/bin/env bash
# Final verdict for the two-run hosted performance check. The first attempt is
# allowed to miss because runner-wide contention is outside the sample-level
# slack model; only a successful retry in a separate job can recover that miss.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: perf-job-guard.sh <first-outcome> <retry-result>" >&2
  exit 2
fi

first_outcome="$1"
retry_result="$2"

if [[ "$first_outcome" == "success" && "$retry_result" == "skipped" ]]; then
  echo "perf-job-guard: first runner passed; no retry needed"
  exit 0
fi

if [[ "$first_outcome" != "success" && "$retry_result" == "success" ]]; then
  echo "perf-job-guard: first runner missed; fresh-runner retry passed"
  exit 0
fi

echo "perf-job-guard: first runner outcome '$first_outcome'; fresh-runner retry '$retry_result'" >&2
exit 1
