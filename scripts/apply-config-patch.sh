#!/usr/bin/env bash
# scripts/apply-config-patch.sh -- apply (or clear) a TOML config patch on the
# running ExtendDB SUT inside a fenced bench-managed block, restart, and
# health-check.
#
# Usage:
#   scripts/apply-config-patch.sh <label> <patch-file-or-->
#   scripts/apply-config-patch.sh --clear
#
# Examples:
#   # Apply a patch from a file, label as "cache-on":
#   scripts/apply-config-patch.sh cache-on configs/auth-cache-on.toml
#
#   # Apply a patch from stdin:
#   echo '[auth.cache]
#   enabled = false' | scripts/apply-config-patch.sh cache-off -
#
#   # Strip the existing managed block (back to default):
#   scripts/apply-config-patch.sh --clear

set -euo pipefail

PROFILE="${AWS_PROFILE:-asomasun-admin}"
REGION="${AWS_REGION:-us-east-1}"

if [[ $# -eq 0 ]]; then
  echo "usage: $0 <label> <patch-file|->   or   $0 --clear" >&2
  exit 2
fi

CLEAR=false
PATCH_B64=""
LABEL=""

if [[ "$1" == "--clear" ]]; then
  CLEAR=true
  LABEL="cleared"
else
  if [[ $# -lt 2 ]]; then
    echo "usage: $0 <label> <patch-file|->   or   $0 --clear" >&2
    exit 2
  fi
  LABEL="$1"
  PATCH_SRC="$2"
  if [[ "$PATCH_SRC" == "-" ]]; then
    PATCH_B64="$(base64 -w0)"
  else
    [[ -f "$PATCH_SRC" ]] || { echo "patch file not found: $PATCH_SRC" >&2; exit 1; }
    PATCH_B64="$(base64 -w0 < "$PATCH_SRC")"
  fi
fi

SUT_ID="$(aws ec2 describe-instances --profile "$PROFILE" --region "$REGION" \
  --filters "Name=tag:project,Values=extenddb-bench" "Name=tag:role,Values=sut" \
            "Name=instance-state-name,Values=running" \
  --query 'Reservations[*].Instances[*].InstanceId' --output text | head -n1)"
[[ -z "$SUT_ID" ]] && { echo "no running SUT instance" >&2; exit 1; }
echo "SUT: $SUT_ID  label=$LABEL  clear=$CLEAR  patch_bytes=${#PATCH_B64}"

PARAMS="$(mktemp)"
jq -n \
  --arg sut "$SUT_ID" \
  --arg b64 "$PATCH_B64" \
  --arg cl  "$([[ "$CLEAR" == true ]] && echo true || echo "")" \
  --arg lab "$LABEL" \
  '{InstanceIds:[$sut], DocumentName:"extenddb-bench-apply-config-patch",
    Parameters:{patchB64:[$b64], clear:[$cl], label:[$lab]}}' > "$PARAMS"

CMD_ID="$(aws ssm send-command --profile "$PROFILE" --region "$REGION" \
  --cli-input-json "file://$PARAMS" --query 'Command.CommandId' --output text)"
rm -f "$PARAMS"
echo "command-id: $CMD_ID"

DEADLINE=$((SECONDS + 300))
while (( SECONDS < DEADLINE )); do
  STATUS="$(aws ssm get-command-invocation --profile "$PROFILE" --region "$REGION" \
    --command-id "$CMD_ID" --instance-id "$SUT_ID" \
    --query 'Status' --output text 2>/dev/null || echo Pending)"
  case "$STATUS" in
    Success)
      aws ssm get-command-invocation --profile "$PROFILE" --region "$REGION" \
        --command-id "$CMD_ID" --instance-id "$SUT_ID" \
        --query 'StandardOutputContent' --output text | tail -20
      echo "apply-config-patch ($LABEL): SUCCESS"
      exit 0;;
    Failed|Cancelled|TimedOut)
      echo "apply-config-patch ($LABEL): $STATUS" >&2
      aws ssm get-command-invocation --profile "$PROFILE" --region "$REGION" \
        --command-id "$CMD_ID" --instance-id "$SUT_ID" \
        --query 'StandardErrorContent' --output text >&2
      aws ssm get-command-invocation --profile "$PROFILE" --region "$REGION" \
        --command-id "$CMD_ID" --instance-id "$SUT_ID" \
        --query 'StandardOutputContent' --output text | tail -50 >&2
      exit 1;;
  esac
  printf "."; sleep 3
done
echo "apply-config-patch ($LABEL): timed out"
exit 1
