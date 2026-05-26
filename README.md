# extenddb-bench

Performance testing harness for [ExtendDB](https://github.com/ExtendDB/extenddb).

Three Amazon Elastic Compute Cloud (EC2) instances in one Virtual Private Cloud (VPC):

```text
                ┌─── VPC ──────────────────────────────────────────┐
                │                                                  │
   operator     │   LG (c7g.8xlarge)         SUT (c7g.4xlarge)     │
   laptop       │   - extenddb-bench         - extenddb            │
       │        │   - :9090 prom             - postgres 15         │
       │ SSM    │   - :9100 node_exporter    - :9100 node_exporter │
       │        │                            - :9187 pg_exporter   │
       └────────┤                                                  │
                │                                                  │
                │   Monitor (t4g.medium, 50 GB EBS RETAIN)         │
                │   - Prometheus :9090                             │
                │   - Grafana    :3000                             │
                └──────────────────────────────────────────────────┘
```

LG is the Load Generator. SUT is the System Under Test. Monitor runs Prometheus and Grafana with three pre-provisioned dashboards. Design and rationale live in [`docs/design-v0.1.md`](docs/design-v0.1.md), [`docs/design-v0.3.md`](docs/design-v0.3.md), and [`docs/v0.15-status.md`](docs/v0.15-status.md).

## Contents

- [Status](#status)
- [Prerequisites](#prerequisites)
- [Repo layout](#repo-layout)
- [Scenarios](#scenarios)
  - [Deploy a stack](#deploy-a-stack)
  - [Open Grafana](#open-grafana)
  - [Get a shell on an instance](#get-a-shell-on-an-instance)
  - [Run a smoke sweep](#run-a-smoke-sweep)
  - [Run a real sweep](#run-a-real-sweep)
  - [Pull results to your laptop](#pull-results-to-your-laptop)
  - [Tail bootstrap and bench logs](#tail-bootstrap-and-bench-logs)
  - [Compare two SHAs (A/B)](#compare-two-shas-ab)
  - [Tear it all down](#tear-it-all-down)
- [License](#license)

## Status

- **v0.15** instrumentation. Three live dashboards: bench, hosts, storage. Instances stay alive after the sweep until the operator runs `cdk destroy`. See [`docs/v0.15-status.md`](docs/v0.15-status.md).
- **v0.3** A/B SHA comparisons. See [`docs/design-v0.3.md`](docs/design-v0.3.md).
- **v0.1** baseline POC. PutItem 1 KiB, RPS sweep, HDR histograms, JSON and markdown reports. See [`docs/design-v0.1.md`](docs/design-v0.1.md).

## Prerequisites

- AWS profile `asomasun-admin` with admin in `us-east-1`
- Node.js 18+ (for AWS Cloud Development Kit (CDK))
- `gh` CLI (for `--pr <id>` resolution)
- AWS Systems Manager (SSM) Session Manager plugin:

```bash
# Amazon Linux 2 / RHEL x86_64 (e.g. dev-dsk):
sudo yum install -y https://s3.amazonaws.com/session-manager-downloads/plugin/latest/linux_64bit/session-manager-plugin.rpm

# Amazon Linux 2 / RHEL ARM64:
sudo yum install -y https://s3.amazonaws.com/session-manager-downloads/plugin/latest/linux_arm64/session-manager-plugin.rpm

# macOS:
brew install --cask session-manager-plugin
```

## Repo layout

```text
extenddb-bench/
├── infra/      AWS CDK TypeScript (network, monitor, compute stacks; user-data; dashboards)
├── loadgen/    Rust load generator (open-loop, HDR histograms)
├── docs/       Design docs and per-version status notes
├── scripts/    cheatsheet.sh, run-via-ssm.sh, pull-results.sh, swap-sha.sh, compare-shas.sh
└── results/    Local sync target for finished runs
```

Stack-by-stack breakdown is in [`docs/design-v0.1.md`](docs/design-v0.1.md#repository-structure).

## Scenarios

### Deploy a stack

Pin an ExtendDB ref one of two ways:

```bash
cd infra
npm install

# A) branch + 40-char commit Secure Hash Algorithm (SHA)
AWS_REGION=us-east-1 AWS_DEFAULT_REGION=us-east-1 AWS_PROFILE=asomasun-admin \
  npx cdk deploy --all --require-approval=never \
  -c extenddbBranch=main -c extenddbCommit=<40-char-sha>

# B) Pull Request id (resolves to head SHA via gh CLI at synth time)
AWS_REGION=us-east-1 AWS_DEFAULT_REGION=us-east-1 AWS_PROFILE=asomasun-admin \
  npx cdk deploy --all --require-approval=never -c extenddbPr=<pr-id>
```

First deploy is ~13 min (3 min CloudFormation, ~10 min for the cargo build on the SUT). Bootstrap status is in `/var/run/extenddb-bench-bootstrap-status` on each instance: `starting`, `postgres-ready`, `extenddb-built`, `ready`.

After deploy, get every command pre-filled with the live ids and IPs:

```bash
AWS_PROFILE=asomasun-admin AWS_REGION=us-east-1 ./scripts/cheatsheet.sh
```

### Open Grafana

```bash
aws ssm start-session --profile asomasun-admin --region us-east-1 \
  --target <MONITOR_INSTANCE_ID> \
  --document-name AWS-StartPortForwardingSession \
  --parameters '{"portNumber":["3000"],"localPortNumber":["3080"]}'

# admin password:
aws ssm get-parameter --profile asomasun-admin --region us-east-1 \
  --with-decryption --name /extenddb-bench/grafana-admin-password \
  --query Parameter.Value --output text
```

Open <http://localhost:3080>, user `admin`. Three dashboards: `extenddb-bench live`, `extenddb-bench hosts`, `extenddb-bench storage (postgres)`. Panel inventory is in [`docs/v0.15-status.md`](docs/v0.15-status.md#dashboards).

### Get a shell on an instance

```bash
./scripts/run-via-ssm.sh lg       # Load Generator
./scripts/run-via-ssm.sh sut      # System Under Test
./scripts/run-via-ssm.sh monitor  # Monitor
```

You land as `ssm-user`. Use `sudo -i` for root.

### Run a smoke sweep

On the LG, ~30 s. Proves the bench dashboard lights up:

```bash
bench-run --rps-sweep 500 --warmup 2s --duration 15s --cooldown 1s --iterations 1
```

### Run a real sweep

On the LG, ~7 min, v0.1 acceptance shape:

```bash
bench-run --rps-sweep 1000,3000,5000,10000 \
  --warmup 5s --duration 30s --cooldown 2s \
  --iterations 3 --connections 256 \
  --output /tmp/bench-$(date -u +%Y%m%dT%H%M%S)
```

`bench-run` is a wrapper at `/usr/local/bin/bench-run` that pulls bench credentials from SSM Parameter Store and execs the load generator with `--tls-insecure`. Signature Version 4 (SigV4) still authenticates the request. All `extenddb-bench run` flags are forwarded.

While running, watch <http://localhost:3080>.

### Pull results to your laptop

After a run, results are at `/tmp/bench-<ts>/` on the LG. Push to S3, then sync down:

```bash
# on LG:
RUN_DIR=$(ls -td /tmp/bench-* | head -1)
RUN_ID=$(basename "$RUN_DIR")
aws s3 sync "$RUN_DIR/" \
  "s3://extenddb-bench-results-$(aws sts get-caller-identity --query Account --output text)/runs/$RUN_ID/"
exit

# on dev-dsk:
./scripts/pull-results.sh "$RUN_ID"
```

`pull-results.sh` syncs to `./results/<run-id>/` and pages the `summary.md`.

### Tail bootstrap and bench logs

```bash
# one-shot, from your laptop:
aws ssm send-command --profile asomasun-admin --region us-east-1 \
  --instance-ids <LG_INSTANCE_ID> \
  --document-name AWS-RunShellScript \
  --parameters 'commands=tail -50 /var/log/extenddb-bench-bootstrap.log'

# interactive:
./scripts/run-via-ssm.sh lg
# then on LG:
tail -f /var/log/extenddb-bench-bootstrap.log
```

### Compare two SHAs (A/B)

Same SUT, sequential head-to-head run of two ExtendDB SHAs. Emits a single `compare-summary.md` with bootstrap-Confidence-Interval-backed verdicts per step.

```bash
# Workload diversity: read, update, mixed.
scripts/compare-shas.sh main 140a1e5e getitem-1kb
scripts/compare-shas.sh main 140a1e5e updateitem-1kb
scripts/compare-shas.sh main 140a1e5e mixed-rw \
    --rps-sweep-file loadgen/sweeps/mixed.csv

# Guard test: same SHA on both sides MUST be `within_noise` on every step.
scripts/compare-shas.sh main main putitem-1kb
```

Per leg: `swap-sha.sh <sha>` (cargo build + `/health` poll) → drop-and-recreate the bench table → ensure pre-seed (S3 stamp keyed by SHA) → sweep → S3 sync. The operator laptop fuses both legs with `extenddb-bench report-compare`.

Verdict labels: `regression` (exit 1), `within_noise`, `improvement`. The per-step verdict is the worse of `achieved_rps` and `p99_us`; the headline is the worst step. Full method in [`docs/design-v0.3.md`](docs/design-v0.3.md).

The `bench` and `extenddb-app` dashboards render annotation markers (`leg=baseline`, `leg=candidate`) at each leg boundary.

### Tear it all down

```bash
cd infra

# Keep monitor + dashboard alive (recommended; review the run later):
AWS_REGION=us-east-1 AWS_DEFAULT_REGION=us-east-1 AWS_PROFILE=asomasun-admin \
  npx cdk destroy ExtendDbBenchCompute --force \
  -c extenddbBranch=main -c extenddbCommit=<sha>

# Nuke everything. The Monitor data Elastic Block Store (EBS) volume is RETAINed;
# delete it manually to recover space.
AWS_REGION=us-east-1 AWS_DEFAULT_REGION=us-east-1 AWS_PROFILE=asomasun-admin \
  npx cdk destroy --all --force \
  -c extenddbBranch=main -c extenddbCommit=<sha>
```

## License

Apache 2.0 (matches ExtendDB).
