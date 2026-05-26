#!/usr/bin/env bash
# scripts/compare-shas.sh — sequential same-SUT compare of two ExtendDB SHAs.
#
# Usage:
#   scripts/compare-shas.sh <baseline-sha> <candidate-sha> <workload> [extra args]
#
# Examples:
#   scripts/compare-shas.sh main 140a1e5e putitem-1kb
#   scripts/compare-shas.sh main 140a1e5e mixed-rw --rps-sweep-file loadgen/sweeps/mixed.csv
#
# Flow per leg:
#   1. swap-sha to leg's SHA (waits for /health 200).
#   2. drop and recreate the bench table (no warm-cache bleed across legs).
#   3. ensure pre-seed (S3 stamp keyed by SHA -> candidate re-seeds).
#   4. run the sweep on the LG (results synced to the LG's local results dir).
#   5. pull both legs' results to operator laptop.
#   6. extenddb-bench report-compare -> compare-summary.{json,md}.

set -euo pipefail

if [[ $# -lt 3 ]]; then
  echo "usage: $0 <baseline-sha> <candidate-sha> <workload> [extra-bench-run-args...]" >&2
  exit 2
fi

BASELINE="$1"; shift
CANDIDATE="$1"; shift
WORKLOAD="$1"; shift
EXTRA_ARGS="$@"

PROFILE="${AWS_PROFILE:-asomasun-admin}"
REGION="${AWS_REGION:-us-east-1}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

COMPARE_ID="$(date -u +%Y%m%dT%H%M%S)"
LOCAL_OUT="${EXTENDDB_BENCH_RESULTS_DIR:-$REPO_ROOT/results}/compare/$COMPARE_ID"
mkdir -p "$LOCAL_OUT/baseline" "$LOCAL_OUT/candidate"

echo "compare-id=$COMPARE_ID"
echo "baseline=$BASELINE  candidate=$CANDIDATE  workload=$WORKLOAD"
echo "local out: $LOCAL_OUT"

ec2_id() {
  aws ec2 describe-instances --profile "$PROFILE" --region "$REGION" \
    --filters "Name=tag:project,Values=extenddb-bench" "Name=tag:role,Values=$1" \
              "Name=instance-state-name,Values=running" \
    --query 'Reservations[*].Instances[*].InstanceId' --output text | head -n1
}
ssm_get() {
  aws ssm get-parameter --profile "$PROFILE" --region "$REGION" \
    --name "$1" --with-decryption --query 'Parameter.Value' --output text 2>/dev/null || echo ""
}

LG_ID="$(ec2_id lg)"
SUT_ID="$(ec2_id sut)"
[[ -z "$LG_ID" || -z "$SUT_ID" ]] && { echo "missing LG or SUT instance" >&2; exit 1; }

SUT_IP="$(ssm_get /extenddb-bench/sut-private-ip)"
TABLE="$(ssm_get /extenddb-bench/table-name)"
TABLE="${TABLE:-bench}"
ACCOUNT="$(aws sts get-caller-identity --profile "$PROFILE" --query Account --output text)"
RESULTS_BUCKET="extenddb-bench-results-$ACCOUNT"

echo "LG=$LG_ID  SUT=$SUT_ID  SUT_IP=$SUT_IP  table=$TABLE  bucket=$RESULTS_BUCKET"

# Issue a remote command on the LG and stream stdout/stderr.
ssm_run_lg() {
  local cmd="$1"
  # Base64 + jq for clean escaping. Pipe the b64 payload to bash -c "$(...)"
  # on the remote so the multi-line script is reconstructed losslessly.
  local b64
  b64="$(printf '%s' "$cmd" | base64 -w0)"
  local wrapper="bash -c \"\$(echo $b64 | base64 -d)\""
  local params_file
  params_file="$(mktemp)"
  jq -n --arg c "$wrapper" '{InstanceIds: [env.LG_ID], DocumentName: "AWS-RunShellScript", Parameters: {commands: [$c]}}' > "$params_file"
  local cmd_id status
  cmd_id="$(LG_ID="$LG_ID" aws ssm send-command --profile "$PROFILE" --region "$REGION" \
    --cli-input-json "file://$params_file" \
    --query 'Command.CommandId' --output text)"
  rm -f "$params_file"
  echo "ssm-cmd: $cmd_id"
  local deadline=$((SECONDS + 3600))
  while (( SECONDS < deadline )); do
    status="$(aws ssm get-command-invocation --profile "$PROFILE" --region "$REGION" \
      --command-id "$cmd_id" --instance-id "$LG_ID" --query 'Status' --output text 2>/dev/null || echo Pending)"
    case "$status" in
      Success)
        aws ssm get-command-invocation --profile "$PROFILE" --region "$REGION" \
          --command-id "$cmd_id" --instance-id "$LG_ID" --query 'StandardOutputContent' --output text
        return 0;;
      Failed|Cancelled|TimedOut)
        echo "lg cmd $status:" >&2
        aws ssm get-command-invocation --profile "$PROFILE" --region "$REGION" \
          --command-id "$cmd_id" --instance-id "$LG_ID" --query 'StandardErrorContent' --output text >&2
        aws ssm get-command-invocation --profile "$PROFILE" --region "$REGION" \
          --command-id "$cmd_id" --instance-id "$LG_ID" --query 'StandardOutputContent' --output text >&2
        return 1;;
    esac
    printf "."; sleep 5
  done
  echo "lg cmd timed out" >&2
  return 1
}

drop_recreate_table() {
  echo "[reset] drop+recreate $TABLE"
  ssm_run_lg "set -euxo pipefail
export AWS_DEFAULT_REGION=$REGION
ACCESS_KEY=\$(aws ssm get-parameter --region $REGION --name /extenddb-bench/access-key-id --with-decryption --query Parameter.Value --output text)
SECRET=\$(aws ssm get-parameter --region $REGION --name /extenddb-bench/secret-access-key --with-decryption --query Parameter.Value --output text)
TLS_B64=\$(aws ssm get-parameter --region $REGION --name /extenddb-bench/tls-cert-b64 --query Parameter.Value --output text)
echo \"\$TLS_B64\" | base64 -d > /tmp/extenddb-ca.pem
export AWS_ACCESS_KEY_ID=\"\$ACCESS_KEY\" AWS_SECRET_ACCESS_KEY=\"\$SECRET\" AWS_CA_BUNDLE=/tmp/extenddb-ca.pem
SUT_IP=\$(aws ssm get-parameter --region $REGION --name /extenddb-bench/sut-private-ip --query Parameter.Value --output text)
EP=\"https://\${SUT_IP}:8000\"
aws dynamodb delete-table --endpoint-url \"\$EP\" --table-name $TABLE 2>/dev/null || true
for i in \$(seq 1 30); do
  if ! aws dynamodb describe-table --endpoint-url \"\$EP\" --table-name $TABLE >/dev/null 2>&1; then
    break
  fi
  sleep 2
done
aws dynamodb create-table \\
  --endpoint-url \"\$EP\" --table-name $TABLE \\
  --attribute-definitions AttributeName=pk,AttributeType=S \\
  --key-schema AttributeName=pk,KeyType=HASH \\
  --billing-mode PAY_PER_REQUEST --no-cli-pager
"
}

run_leg() {
  local leg="$1" sha="$2"
  echo
  echo "==== leg=$leg sha=$sha ===="
  echo "[swap] $sha"
  "$SCRIPT_DIR/swap-sha.sh" "$sha"
  drop_recreate_table

  local out_remote="/tmp/bench-results/compare/$COMPARE_ID/$leg"
  echo "[run] sweep into $out_remote"
  ssm_run_lg "set -euxo pipefail
mkdir -p $out_remote
export EXTENDDB_BENCH_SHA=$sha
bench-run \\
  --workload $WORKLOAD \\
  --table-name $TABLE \\
  --output $out_remote \\
  --leg-tag $leg \\
  --compare-id $COMPARE_ID \\
  --stamp-bucket $RESULTS_BUCKET \\
  $EXTRA_ARGS
aws s3 sync $out_remote s3://$RESULTS_BUCKET/runs/compare/$COMPARE_ID/$leg/
"
  echo "[pull] s3 -> $LOCAL_OUT/$leg"
  aws s3 sync --profile "$PROFILE" \
    "s3://$RESULTS_BUCKET/runs/compare/$COMPARE_ID/$leg/" "$LOCAL_OUT/$leg/"
}

run_leg baseline  "$BASELINE"
run_leg candidate "$CANDIDATE"

echo
echo "==== report-compare ===="
"$REPO_ROOT/loadgen/target/release/extenddb-bench" report-compare \
  --baseline "$LOCAL_OUT/baseline" \
  --candidate "$LOCAL_OUT/candidate" \
  --output    "$LOCAL_OUT" \
  --compare-id "$COMPARE_ID"
