#!/usr/bin/env bash
# scripts/swap-sha.sh — invoke the extenddb-bench-swap-sha SSM document on the SUT.
#
# Usage:
#   scripts/swap-sha.sh <sha>
#
# Locks ExtendDB on the SUT to the given commit SHA. Postgres untouched.
# Polls until the SSM RunCommand finishes; prints stdout/stderr.
#
# Exit codes:
#   0 -- swap successful, /health returned 200 within timeout
#   non-zero -- swap or health check failed (see printed output)

set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <sha>" >&2
  exit 2
fi
SHA="$1"

PROFILE="${AWS_PROFILE:-asomasun-admin}"
REGION="${AWS_REGION:-us-east-1}"

SUT_ID="$(aws ec2 describe-instances --profile "$PROFILE" --region "$REGION" \
  --filters "Name=tag:project,Values=extenddb-bench" "Name=tag:role,Values=sut" \
            "Name=instance-state-name,Values=running" \
  --query 'Reservations[*].Instances[*].InstanceId' --output text | head -n1)"

if [[ -z "$SUT_ID" ]]; then
  echo "no running SUT instance found in $REGION" >&2
  exit 1
fi
echo "SUT: $SUT_ID  (target sha=$SHA)"

CMD_ID="$(aws ssm send-command --profile "$PROFILE" --region "$REGION" \
  --instance-ids "$SUT_ID" \
  --document-name extenddb-bench-swap-sha \
  --parameters "sha=$SHA" \
  --query 'Command.CommandId' --output text)"
echo "command-id: $CMD_ID"

# Poll until the per-instance invocation reaches a terminal state.
DEADLINE=$((SECONDS + 1200))   # 20 min cap (cargo build dominates).
while (( SECONDS < DEADLINE )); do
  STATUS="$(aws ssm get-command-invocation --profile "$PROFILE" --region "$REGION" \
    --command-id "$CMD_ID" --instance-id "$SUT_ID" \
    --query 'Status' --output text 2>/dev/null || echo Pending)"
  case "$STATUS" in
    Success)
      aws ssm get-command-invocation --profile "$PROFILE" --region "$REGION" \
        --command-id "$CMD_ID" --instance-id "$SUT_ID" \
        --query 'StandardOutputContent' --output text | tail -50
      echo "---"
      echo "swap to $SHA: SUCCESS"
      exit 0
      ;;
    Failed|Cancelled|TimedOut)
      echo "swap to $SHA: $STATUS"
      aws ssm get-command-invocation --profile "$PROFILE" --region "$REGION" \
        --command-id "$CMD_ID" --instance-id "$SUT_ID" \
        --query 'StandardOutputContent' --output text | tail -100
      echo "--- stderr:"
      aws ssm get-command-invocation --profile "$PROFILE" --region "$REGION" \
        --command-id "$CMD_ID" --instance-id "$SUT_ID" \
        --query 'StandardErrorContent' --output text | tail -100
      exit 1
      ;;
  esac
  printf "."
  sleep 5
done
echo
echo "swap timed out after 20m"
exit 1
