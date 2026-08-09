#!/usr/bin/env bash
# State reduction for two parallel fresh-runner performance measurements.
# Runner-wide contention is outside the sample-level slack model, so one clean
# run passes and only two misses fail.
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
  elif [[ "$first_outcome" == "failure" && "$second_outcome" == "success" ]]; then
    echo "outcome=failure"
  else
    echo "outcome=invalid"
  fi
  exit 0
fi

if [[ "$mode" == "final" ]]; then
  if [[ "$first_outcome" == "success" && "$second_outcome" == "success" ]]; then
    echo "perf-job-guard: both fresh runners passed"
    exit 0
  fi
  if [[ "$first_outcome" == "success" && "$second_outcome" == "failure" ]]; then
    echo "perf-job-guard: first runner passed; second fresh runner missed"
    exit 0
  fi
  if [[ "$first_outcome" == "failure" && "$second_outcome" == "success" ]]; then
    echo "perf-job-guard: first runner missed; second fresh runner passed"
    exit 0
  fi
  echo "perf-job-guard: first runner outcome '$first_outcome'; second fresh runner '$second_outcome'" >&2
  exit 1
fi

echo "perf-job-guard: unknown mode '$mode'" >&2
exit 2
