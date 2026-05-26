#!/usr/bin/env bash
# scripts/flamegraph.sh -- capture a perf flamegraph of the running ExtendDB
# process on the SUT while the LG drives steady-state load.
#
# Usage:
#   scripts/flamegraph.sh <run-id> <leg-label> <workload> [options]
#
# Options:
#   --rps N             constant RPS to drive on the LG (default: 5000)
#   --duration N        capture window in seconds (default: 60)
#   --warmup N          seconds of LG load before perf starts (default: 15)
#   --freq N            perf sampling frequency in Hz (default: 99)
#   --skip-load         do not start LG load; assume it is already running
#                       (use when a sweep is in progress and you want to
#                       sample a specific step from outside).
#   --extra-args "..."  extra args forwarded to bench-run on the LG.
#
# Output: s3://<results-bucket>/flamegraphs/<run-id>/<leg-label>/
#         {flame.svg, perf.folded, meta.json}
#
# The local results dir gets a copy of the same artifacts under
#   results/flamegraphs/<run-id>/<leg-label>/
# so you can open the SVG in a browser without round-tripping through S3.

set -euo pipefail

if [[ $# -lt 3 ]]; then
  echo "usage: $0 <run-id> <leg-label> <workload> [options]" >&2
  exit 2
fi

RUN_ID="$1"; shift
LEG_LABEL="$1"; shift
WORKLOAD="$1"; shift

RPS=5000
DURATION=60
WARMUP=15
FREQ=99
SKIP_LOAD=false
EXTRA_ARGS=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --rps)        RPS="$2"; shift 2;;
    --duration)   DURATION="$2"; shift 2;;
    --warmup)     WARMUP="$2"; shift 2;;
    --freq)       FREQ="$2"; shift 2;;
    --skip-load)  SKIP_LOAD=true; shift;;
    --extra-args) EXTRA_ARGS="$2"; shift 2;;
    *) echo "unknown flag: $1" >&2; exit 2;;
  esac
done

PROFILE="${AWS_PROFILE:-asomasun-admin}"
REGION="${AWS_REGION:-us-east-1}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

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

ACCOUNT="$(aws sts get-caller-identity --profile "$PROFILE" --query Account --output text)"
RESULTS_BUCKET="extenddb-bench-results-$ACCOUNT"

# Read the SHA the SUT is currently locked to.
SHA_CMD="$(aws ssm send-command --profile "$PROFILE" --region "$REGION" \
  --instance-ids "$SUT_ID" --document-name AWS-RunShellScript \
  --parameters 'commands=["cat /etc/extenddb-version"]' \
  --query 'Command.CommandId' --output text)"
for _ in $(seq 1 30); do
  status="$(aws ssm get-command-invocation --profile "$PROFILE" --region "$REGION" \
    --command-id "$SHA_CMD" --instance-id "$SUT_ID" --query 'Status' --output text 2>/dev/null || echo Pending)"
  [[ "$status" == "Success" ]] && break
  [[ "$status" =~ ^(Failed|Cancelled|TimedOut)$ ]] && { echo "could not read /etc/extenddb-version" >&2; exit 1; }
  sleep 1
done
SHA="$(aws ssm get-command-invocation --profile "$PROFILE" --region "$REGION" \
  --command-id "$SHA_CMD" --instance-id "$SUT_ID" --query 'StandardOutputContent' --output text | tr -d '[:space:]')"

S3_DIR="s3://$RESULTS_BUCKET/flamegraphs/$RUN_ID/$LEG_LABEL"
LOCAL_DIR="${EXTENDDB_BENCH_RESULTS_DIR:-$REPO_ROOT/results}/flamegraphs/$RUN_ID/$LEG_LABEL"
mkdir -p "$LOCAL_DIR"

echo "run-id    : $RUN_ID"
echo "leg-label : $LEG_LABEL"
echo "workload  : $WORKLOAD"
echo "rps       : $RPS"
echo "warmup    : ${WARMUP}s"
echo "duration  : ${DURATION}s"
echo "freq      : ${FREQ} Hz"
echo "sha       : $SHA"
echo "skip-load : $SKIP_LOAD"
echo "s3-uri    : $S3_DIR/"
echo "local-dir : $LOCAL_DIR/"

# Send LG load in the background. compare-shas.sh has a battle-tested
# base64-wrapped SSM runner; reuse that pattern here in a smaller form.
ssm_run_lg_async() {
  local cmd="$1"
  local b64; b64="$(printf '%s' "$cmd" | base64 -w0)"
  local wrapper="bash -c \"\$(echo $b64 | base64 -d)\""
  local params; params="$(mktemp)"
  jq -n --arg c "$wrapper" --arg lg "$LG_ID" \
    '{InstanceIds: [$lg], DocumentName: "AWS-RunShellScript", Parameters: {commands: [$c]}}' > "$params"
  aws ssm send-command --profile "$PROFILE" --region "$REGION" \
    --cli-input-json "file://$params" --query 'Command.CommandId' --output text
}

LOAD_DURATION=$((WARMUP + DURATION + 30))
LOAD_CMD_ID=""
if [[ "$SKIP_LOAD" == "false" ]]; then
  echo "[load] starting bench-run constant-rps load on LG for ${LOAD_DURATION}s"
  LOAD_SCRIPT="set -euxo pipefail
mkdir -p /tmp/flamegraph-load/$RUN_ID/$LEG_LABEL
bench-run \\
  --workload $WORKLOAD \\
  --constant-rps $RPS \\
  --duration ${LOAD_DURATION}s \\
  --output /tmp/flamegraph-load/$RUN_ID/$LEG_LABEL \\
  $EXTRA_ARGS"
  LOAD_CMD_ID="$(ssm_run_lg_async "$LOAD_SCRIPT")"
  echo "[load] ssm-cmd: $LOAD_CMD_ID"
  echo "[load] warming for ${WARMUP}s..."
  sleep "$WARMUP"
fi

# Kick the flamegraph SSM doc on the SUT and wait. We use --cli-input-json
# so title/subtitle can contain spaces and commas without SSM parameter-list
# parsing breaking.
TITLE="extenddb $LEG_LABEL"
SUBTITLE="$WORKLOAD $RPS rps ${DURATION}s sha=${SHA:0:8}"

echo "[capture] launching flamegraph SSM doc on SUT"
FG_PARAMS="$(mktemp)"
jq -n \
  --arg sut "$SUT_ID" \
  --arg dur "$DURATION" \
  --arg freq "$FREQ" \
  --arg s3 "$S3_DIR" \
  --arg title "$TITLE" \
  --arg sub "$SUBTITLE" \
  '{InstanceIds:[$sut], DocumentName:"extenddb-bench-flamegraph",
    Parameters:{durationSeconds:[$dur], freqHz:[$freq], s3Uri:[$s3],
                title:[$title], subtitle:[$sub]}}' > "$FG_PARAMS"
FG_CMD_ID="$(aws ssm send-command --profile "$PROFILE" --region "$REGION" \
  --cli-input-json "file://$FG_PARAMS" --query 'Command.CommandId' --output text)"
rm -f "$FG_PARAMS"
echo "[capture] ssm-cmd: $FG_CMD_ID"

DEADLINE=$((SECONDS + DURATION + 240))
while (( SECONDS < DEADLINE )); do
  status="$(aws ssm get-command-invocation --profile "$PROFILE" --region "$REGION" \
    --command-id "$FG_CMD_ID" --instance-id "$SUT_ID" --query 'Status' --output text 2>/dev/null || echo Pending)"
  case "$status" in
    Success)
      aws ssm get-command-invocation --profile "$PROFILE" --region "$REGION" \
        --command-id "$FG_CMD_ID" --instance-id "$SUT_ID" --query 'StandardOutputContent' --output text | tail -20
      break;;
    Failed|Cancelled|TimedOut)
      echo "[capture] $status" >&2
      aws ssm get-command-invocation --profile "$PROFILE" --region "$REGION" \
        --command-id "$FG_CMD_ID" --instance-id "$SUT_ID" --query 'StandardErrorContent' --output text >&2
      exit 1;;
  esac
  printf "."; sleep 5
done

if [[ "$SKIP_LOAD" == "false" && -n "$LOAD_CMD_ID" ]]; then
  echo "[load] waiting for LG load to finish (or until 90s elapse)"
  for _ in $(seq 1 18); do
    s="$(aws ssm get-command-invocation --profile "$PROFILE" --region "$REGION" \
      --command-id "$LOAD_CMD_ID" --instance-id "$LG_ID" --query 'Status' --output text 2>/dev/null || echo Pending)"
    [[ "$s" == "Success" || "$s" =~ ^(Failed|Cancelled|TimedOut)$ ]] && { echo "[load] terminal: $s"; break; }
    sleep 5
  done
fi

echo "[pull] $S3_DIR -> $LOCAL_DIR"
aws s3 sync --profile "$PROFILE" "$S3_DIR/" "$LOCAL_DIR/"

# Drop a small meta.json next to the artifacts.
TS="$(date -u +%FT%TZ)"
cat > "$LOCAL_DIR/meta.json" <<EOF
{
  "run_id": "$RUN_ID",
  "leg_label": "$LEG_LABEL",
  "workload": "$WORKLOAD",
  "rps": $RPS,
  "duration_seconds": $DURATION,
  "warmup_seconds": $WARMUP,
  "freq_hz": $FREQ,
  "sha": "$SHA",
  "captured_at": "$TS"
}
EOF
aws s3 cp --profile "$PROFILE" --no-progress "$LOCAL_DIR/meta.json" "$S3_DIR/meta.json" >/dev/null

echo "flamegraph: $LOCAL_DIR/flame.svg"
echo "folded    : $LOCAL_DIR/perf.folded"
echo "meta      : $LOCAL_DIR/meta.json"
