#!/usr/bin/env bash
# Convenience wrapper: open an SSM Session Manager session against a stack instance.
# Usage:
#   scripts/run-via-ssm.sh                     # picks the LG instance by tag
#   scripts/run-via-ssm.sh sut                 # picks the SUT instance
#   scripts/run-via-ssm.sh -- bench-run --rps-sweep 1000,5000  # exec bench-run on LG

set -euo pipefail

ROLE="${1:-lg}"
shift || true
if [[ "${1:-}" == "--" ]]; then shift; fi

if [[ "$ROLE" != "lg" && "$ROLE" != "sut" ]]; then
  echo "usage: $0 [lg|sut] [-- <command-to-exec>...]" >&2
  exit 2
fi

PROFILE="${AWS_PROFILE:-asomasun-admin}"
REGION="${AWS_REGION:-us-east-1}"

INSTANCE_ID="$(aws ec2 describe-instances \
  --profile "$PROFILE" --region "$REGION" \
  --filters "Name=tag:project,Values=extenddb-bench" "Name=tag:role,Values=$ROLE" "Name=instance-state-name,Values=running" \
  --query 'Reservations[*].Instances[*].InstanceId' --output text | head -n1)"

if [[ -z "$INSTANCE_ID" ]]; then
  echo "no running $ROLE instance found in $REGION" >&2
  exit 1
fi
echo "$ROLE instance: $INSTANCE_ID"

if [[ $# -eq 0 ]]; then
  exec aws ssm start-session --profile "$PROFILE" --region "$REGION" --target "$INSTANCE_ID"
fi

# Run a command non-interactively via run-command and stream the output.
exec aws ssm send-command --profile "$PROFILE" --region "$REGION" \
  --instance-ids "$INSTANCE_ID" \
  --document-name AWS-RunShellScript \
  --parameters "commands=$(printf '%s ' "$@")" \
  --output json
