#!/usr/bin/env bash
# scripts/compare-flamegraphs.sh -- multi-leg flamegraph comparison.
#
# Runs scripts/flamegraph.sh once per leg with full control over the SHA and
# config patch applied to each leg. After all legs are captured, generates
# pairwise diff flamegraphs (inferno-diff-folded) and a markdown report
# embedding all SVGs.
#
# Generic usage (one --leg flag per leg):
#   scripts/compare-flamegraphs.sh \
#     --workload getitem-1kb --rps 5000 --duration 60 \
#     --leg 'baseline:sha=140a1e5ee5d6f96251f29d1703d0b48ecd19efb1' \
#     --leg 'pr-off:sha=d640a7764bbd5418216fb455323b46c0530f6f14:patch=configs/auth-cache-off.toml' \
#     --leg 'pr-on:sha=d640a7764bbd5418216fb455323b46c0530f6f14:patch=configs/auth-cache-on.toml'
#
# Each --leg value has the form: <label>:sha=<sha>[:patch=<toml-file>|:clear]
#   - patch=<file>: apply this TOML fragment via apply-config-patch.sh
#   - clear: strip any existing managed block before this leg
#   - omit both: leave the existing config alone
#
# Convenience shortcut for PR #122 (auth-cache):
#   scripts/compare-flamegraphs.sh --pr-cache \
#     --workload getitem-1kb --rps 5000 --duration 60
# This wires up the three canonical legs (baseline, pr-off, pr-on) using the
# patch files at configs/auth-cache-off.toml and configs/auth-cache-on.toml.
#
# Output: results/compare-flamegraphs/<run-id>/
#   - <leg>/{flame.svg,perf.folded,meta.json} per leg
#   - diffs/<a>-vs-<b>.svg for every ordered pair
#   - report.md
#   - all artifacts also synced to s3://<bucket>/flamegraphs/<run-id>/

set -euo pipefail

PROFILE="${AWS_PROFILE:-asomasun-admin}"
REGION="${AWS_REGION:-us-east-1}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

WORKLOAD=""
RPS=5000
DURATION=60
WARMUP=15
FREQ=99
EXTRA_ARGS=""
LEGS=()
PR_CACHE_SHORTCUT=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --workload)   WORKLOAD="$2"; shift 2;;
    --rps)        RPS="$2"; shift 2;;
    --duration)   DURATION="$2"; shift 2;;
    --warmup)     WARMUP="$2"; shift 2;;
    --freq)       FREQ="$2"; shift 2;;
    --extra-args) EXTRA_ARGS="$2"; shift 2;;
    --leg)        LEGS+=("$2"); shift 2;;
    --pr-cache)   PR_CACHE_SHORTCUT=true; shift;;
    -h|--help)
      sed -n '3,30p' "$0"; exit 0;;
    *) echo "unknown flag: $1" >&2; exit 2;;
  esac
done

if $PR_CACHE_SHORTCUT; then
  if [[ ${#LEGS[@]} -gt 0 ]]; then
    echo "--pr-cache and --leg are mutually exclusive" >&2; exit 2
  fi
  BASELINE_SHA="140a1e5ee5d6f96251f29d1703d0b48ecd19efb1"
  PR_SHA="d640a7764bbd5418216fb455323b46c0530f6f14"
  LEGS=(
    "baseline:sha=${BASELINE_SHA}:clear"
    "pr-off:sha=${PR_SHA}:patch=${REPO_ROOT}/configs/auth-cache-off.toml"
    "pr-on:sha=${PR_SHA}:patch=${REPO_ROOT}/configs/auth-cache-on.toml"
  )
fi

if [[ -z "$WORKLOAD" ]]; then echo "--workload is required" >&2; exit 2; fi
if [[ ${#LEGS[@]} -lt 2 ]]; then echo "need at least 2 --leg specs" >&2; exit 2; fi

RUN_ID="$(date -u +%Y%m%dT%H%M%S)"
LOCAL_OUT="${EXTENDDB_BENCH_RESULTS_DIR:-$REPO_ROOT/results}/compare-flamegraphs/$RUN_ID"
mkdir -p "$LOCAL_OUT/diffs"

ACCOUNT="$(aws sts get-caller-identity --profile "$PROFILE" --query Account --output text)"
RESULTS_BUCKET="extenddb-bench-results-$ACCOUNT"
S3_BASE="s3://$RESULTS_BUCKET/flamegraphs/$RUN_ID"

echo "run-id   : $RUN_ID"
echo "workload : $WORKLOAD"
echo "rps      : $RPS"
echo "duration : ${DURATION}s"
echo "legs     :"
for spec in "${LEGS[@]}"; do echo "  $spec"; done
echo "out-dir  : $LOCAL_OUT"
echo "s3-base  : $S3_BASE"

parse_leg() {
  # parse_leg "<label>:sha=<sha>[:patch=<file>|:clear]"
  # echoes 3 lines: label, sha, action ("patch=<file>" | "clear" | "noop")
  local spec="$1"
  local label sha action="noop"
  IFS=':' read -r -a parts <<< "$spec"
  label="${parts[0]}"
  for p in "${parts[@]:1}"; do
    case "$p" in
      sha=*)    sha="${p#sha=}";;
      patch=*)  action="patch=${p#patch=}";;
      clear)    action="clear";;
      *) echo "bad leg field: $p" >&2; return 1;;
    esac
  done
  [[ -z "$label" || -z "$sha" ]] && { echo "leg spec missing label or sha: $spec" >&2; return 1; }
  printf '%s\n%s\n%s\n' "$label" "$sha" "$action"
}

CURRENT_SHA=""

run_leg_spec() {
  local spec="$1"
  local label sha action
  { read -r label; read -r sha; read -r action; } < <(parse_leg "$spec")

  echo
  echo "==================== leg: $label ===================="
  echo "  sha    : $sha"
  echo "  action : $action"

  if [[ "$sha" != "$CURRENT_SHA" ]]; then
    echo "[swap] $sha"
    "$SCRIPT_DIR/swap-sha.sh" "$sha"
    CURRENT_SHA="$sha"
    # An SHA swap restarts the binary with the original config; previous
    # managed-block state is preserved on disk but the caller may want
    # patch=... on this leg too. Apply that next.
  fi

  case "$action" in
    clear)
      echo "[config] clearing bench-managed block"
      "$SCRIPT_DIR/apply-config-patch.sh" --clear ;;
    patch=*)
      local file="${action#patch=}"
      [[ -f "$file" ]] || { echo "patch file not found: $file" >&2; return 1; }
      echo "[config] applying patch from $file"
      "$SCRIPT_DIR/apply-config-patch.sh" "$label" "$file" ;;
    noop)
      echo "[config] no change" ;;
  esac

  echo "[capture] flamegraph"
  "$SCRIPT_DIR/flamegraph.sh" "$RUN_ID" "$label" "$WORKLOAD" \
    --rps "$RPS" --duration "$DURATION" --warmup "$WARMUP" --freq "$FREQ" \
    ${EXTRA_ARGS:+--extra-args "$EXTRA_ARGS"}
}

LABELS=()
for spec in "${LEGS[@]}"; do
  run_leg_spec "$spec"
  label_only="$(echo "$spec" | cut -d: -f1)"
  LABELS+=("$label_only")
done

echo
echo "==================== diffs ===================="

# Confirm inferno-diff-folded is available locally. If not, skip diffs and
# warn -- the per-leg SVGs are still useful.
if ! command -v inferno-diff-folded >/dev/null 2>&1; then
  echo "warn: inferno-diff-folded not on PATH locally; skipping diff SVGs."
  echo "      cargo install inferno --features cli   to enable."
  HAVE_DIFF=false
else
  HAVE_DIFF=true
fi

if $HAVE_DIFF; then
  for ((i=0; i<${#LABELS[@]}; i++)); do
    for ((j=0; j<${#LABELS[@]}; j++)); do
      [[ $i -eq $j ]] && continue
      a="${LABELS[$i]}"; b="${LABELS[$j]}"
      # Per-leg artifacts live under results/flamegraphs/<run-id>/<leg>/,
      # not under the compare-flamegraphs output dir. flamegraph.sh writes
      # there directly; the orchestrator copies refs into its own report.
      ap="${EXTENDDB_BENCH_RESULTS_DIR:-$REPO_ROOT/results}/flamegraphs/$RUN_ID/$a/perf.folded"
      bp="${EXTENDDB_BENCH_RESULTS_DIR:-$REPO_ROOT/results}/flamegraphs/$RUN_ID/$b/perf.folded"
      if [[ ! -f "$ap" || ! -f "$bp" ]]; then
        echo "skip diff $a -> $b (missing folded stacks at $ap or $bp)"; continue
      fi
      out="$LOCAL_OUT/diffs/${a}-vs-${b}.svg"
      echo "[diff] $a -> $b  =>  $(basename "$out")"
      inferno-diff-folded "$ap" "$bp" \
        | inferno-flamegraph \
            --title "$a vs $b ($WORKLOAD @ ${RPS} rps)" \
            --subtitle "red = hotter on $b, blue = cooler on $b" \
            > "$out"
    done
  done
fi

echo
echo "==================== report ===================="
REPORT="$LOCAL_OUT/report.md"
{
  echo "# Flamegraph compare: $RUN_ID"
  echo
  echo "- Workload: \`$WORKLOAD\`"
  echo "- Steady-state RPS: $RPS"
  echo "- Capture window: ${DURATION}s (warmup ${WARMUP}s, freq ${FREQ}Hz)"
  echo "- Run id: \`$RUN_ID\`"
  echo
  echo "## Legs"
  echo
  echo "| label | sha | action |"
  echo "|---|---|---|"
  for spec in "${LEGS[@]}"; do
    label="$(echo "$spec" | cut -d: -f1)"
    sha="$(echo "$spec" | tr ':' '\n' | grep '^sha=' | head -1 | cut -d= -f2)"
    action="$(echo "$spec" | tr ':' '\n' | grep -E '^(patch=|clear)' | head -1 || echo 'noop')"
    echo "| $label | \`${sha:0:12}\` | $action |"
  done
  echo
  echo "## Per-leg flamegraphs"
  echo
  for label in "${LABELS[@]}"; do
    fg_dir="${EXTENDDB_BENCH_RESULTS_DIR:-$REPO_ROOT/results}/flamegraphs/$RUN_ID/$label"
    if [[ -f "$fg_dir/flame.svg" ]]; then
      rel="../../flamegraphs/$RUN_ID/$label/flame.svg"
      echo "### $label"
      echo
      echo "[$label/flame.svg]($rel)"
      echo
    fi
  done
  if $HAVE_DIFF; then
    echo "## Diff flamegraphs"
    echo
    echo "Red = function hotter on the right-hand leg. Blue = cooler. Width = absolute time on the right-hand leg."
    echo
    for ((i=0; i<${#LABELS[@]}; i++)); do
      for ((j=0; j<${#LABELS[@]}; j++)); do
        [[ $i -eq $j ]] && continue
        a="${LABELS[$i]}"; b="${LABELS[$j]}"
        f="diffs/${a}-vs-${b}.svg"
        [[ -f "$LOCAL_OUT/$f" ]] && { echo "- [$a -> $b]($f)"; }
      done
    done
    echo
  fi
  echo "## Reproducing"
  echo
  echo '```'
  printf 'scripts/compare-flamegraphs.sh \\\n  --workload %s --rps %s --duration %s' "$WORKLOAD" "$RPS" "$DURATION"
  for spec in "${LEGS[@]}"; do
    printf ' \\\n  --leg %q' "$spec"
  done
  echo
  echo '```'
} > "$REPORT"

echo "[upload] $LOCAL_OUT -> $S3_BASE/"
aws s3 sync --profile "$PROFILE" --no-progress "$LOCAL_OUT/" "$S3_BASE/"

echo
echo "report : $REPORT"
echo "s3     : $S3_BASE/"
