#!/usr/bin/env bash
# Pull a results dir from the bench S3 bucket and open the summary.
# Usage:
#   scripts/pull-results.sh <run-id>
#
# Run IDs are the timestamp directories under s3://extenddb-bench-results-<account>/runs/.

set -euo pipefail

RUN_ID="${1:-}"
if [[ -z "$RUN_ID" ]]; then
  echo "usage: $0 <run-id>" >&2
  echo "available runs:" >&2
  aws s3 ls "s3://extenddb-bench-results-$(aws sts get-caller-identity --profile "${AWS_PROFILE:-asomasun-admin}" --query Account --output text)/runs/" \
    --profile "${AWS_PROFILE:-asomasun-admin}" >&2 || true
  exit 2
fi

PROFILE="${AWS_PROFILE:-asomasun-admin}"
ACCOUNT="$(aws sts get-caller-identity --profile "$PROFILE" --query Account --output text)"
DEST="${EXTENDDB_BENCH_RESULTS_DIR:-./results}/$RUN_ID"
mkdir -p "$DEST"
aws s3 sync \
  "s3://extenddb-bench-results-$ACCOUNT/runs/$RUN_ID/" \
  "$DEST/" \
  --profile "$PROFILE"

echo "results: $DEST"
echo "summary: $DEST/summary.md"
if command -v less >/dev/null 2>&1 && [[ -f "$DEST/summary.md" ]]; then
  less "$DEST/summary.md"
fi
