#!/usr/bin/env bash
# State reduction for the two-run hosted performance check. The first attempt is
# allowed to miss because runner-wide contention is outside the sample-level
# slack model; only a wholly successful attempt or fresh-runner retry passes.
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: perf-job-guard.sh <attempt|final> <first-outcome> <second-outcome>" >&2
  exit 2
fi

mode="$1"
first_outcome="$2"
second_outcome="$3"

if [[ "$mode" == "attempt" ]]; then
  if [[ "$first_outcome" == "success" && "$second_outcome" == "success" ]]; then
    echo "outcome=success"
  else
    echo "outcome=failure"
  fi
  exit 0
fi

if [[ "$mode" == "final" ]]; then
  if [[ "$first_outcome" == "success" && "$second_outcome" == "skipped" ]]; then
    echo "perf-job-guard: first runner passed; no retry needed"
    exit 0
  fi
  if [[ "$first_outcome" != "success" && "$second_outcome" == "success" ]]; then
    echo "perf-job-guard: first runner missed; fresh-runner retry passed"
    exit 0
  fi
  echo "perf-job-guard: first runner outcome '$first_outcome'; fresh-runner retry '$second_outcome'" >&2
  exit 1
fi

echo "perf-job-guard: unknown mode '$mode'" >&2
exit 2
