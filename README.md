# extenddb-bench

Performance testing harness for [ExtendDB](https://github.com/ExtendDB/extenddb).

Three EC2 instances in one VPC: a load generator, a SUT, and a monitor.
ExtendDB and the load gen are built from source on each deploy. The monitor
runs Prometheus + Grafana with three pre-provisioned dashboards.

```text
                ┌─── VPC ──────────────────────────────────────────┐
                │                                                  │
   operator     │   LG (c7g.8xlarge)         SUT (c7g.4xlarge)     │
   laptop       │   ─ extenddb-bench         ─ extenddb            │
       │        │   ─ :9090 prom             ─ postgres 15         │
       │ SSM    │   ─ :9100 node_exporter    ─ :9100 node_exporter │
       │        │                            ─ :9187 pg_exporter   │
       └────────┤                                                  │
                │                                                  │
                │   Monitor (t4g.medium, 50 GB EBS RETAIN)         │
                │   ─ Prometheus :9090                             │
                │   ─ Grafana    :3000                             │
                └──────────────────────────────────────────────────┘
```

## Status

**v0.15** — instrumentation. Three live dashboards: bench, hosts, storage.
Instances stay alive after the sweep until the operator runs `cdk destroy`.
See [`docs/v0.15-status.md`](docs/v0.15-status.md).

**v0.1** — baseline POC. PutItem 1 KB, RPS sweep, HDR histograms, JSON +
markdown reports, S3 sync. See [`docs/v0.1-status.md`](docs/v0.1-status.md)
and [`docs/design-v0.1.md`](docs/design-v0.1.md).

## Repo layout

```text
extenddb-bench/
├── infra/                    CDK TypeScript
│   ├── bin/stack.ts          Network + Monitor + Compute stacks
│   └── lib/
│       ├── network-stack.ts  VPC, NAT, S3 results, bench SG
│       ├── monitor-stack.ts  Prometheus + Grafana on t4g.medium
│       ├── compute-stack.ts  LG + SUT on c7g.{8,4}xlarge
│       ├── pr-resolver.ts    `--branch+--commit` or `--pr` -> SHA
│       ├── user-data/
│       │   ├── monitor.sh
│       │   ├── lg.sh
│       │   └── sut.sh
│       └── dashboards/       Three Grafana JSON dashboards
├── loadgen/                  Rust load generator (open-loop, HDR)
├── docs/
│   ├── design-v0.1.md
│   ├── v0.1-status.md
│   └── v0.15-status.md
└── scripts/
    ├── cheatsheet.sh         Print all operator commands for a live stack
    ├── run-via-ssm.sh        Pick LG/SUT and start an SSM session
    └── pull-results.sh       aws s3 sync for a run id
```

## Prerequisites

- AWS profile `asomasun-admin` with admin in `us-east-1`
- Node 18+ (for CDK)
- `gh` CLI (for `--pr <id>` resolution)
- AWS Session Manager plugin:

```bash
# AL2 / RHEL x86_64 (e.g. dev-dsk):
sudo yum install -y https://s3.amazonaws.com/session-manager-downloads/plugin/latest/linux_64bit/session-manager-plugin.rpm

# AL2 / RHEL ARM64:
sudo yum install -y https://s3.amazonaws.com/session-manager-downloads/plugin/latest/linux_arm64/session-manager-plugin.rpm

# macOS:
brew install --cask session-manager-plugin
```

## Deploy

Pin an ExtendDB ref one of two ways:

```bash
cd infra
npm install

# Option A: branch + 40-char commit SHA
AWS_REGION=us-east-1 AWS_DEFAULT_REGION=us-east-1 AWS_PROFILE=asomasun-admin \
  npx cdk deploy --all --require-approval=never \
  -c extenddbBranch=main -c extenddbCommit=<40-char-sha>

# Option B: PR id (resolves to head SHA via gh CLI at synth time)
AWS_REGION=us-east-1 AWS_DEFAULT_REGION=us-east-1 AWS_PROFILE=asomasun-admin \
  npx cdk deploy --all --require-approval=never -c extenddbPr=<pr-id>
```

First deploy takes ~13 min (3 min CFN + ~10 min for ExtendDB cargo build on the SUT).
Monitor and LG finish faster (~3 min).

Bootstrap status is in `/var/run/extenddb-bench-bootstrap-status` on each instance
(values: `starting`, `postgres-ready`, `extenddb-built`, `ready`).

## Operator cheatsheet

After deploy, run:

```bash
AWS_PROFILE=asomasun-admin AWS_REGION=us-east-1 \
  ./scripts/cheatsheet.sh
```

It prints instance ids, IPs, the Grafana password, and every command below
filled in for the running stack.

### Open Grafana

Port-forward to the monitor (uses Session Manager):

```bash
aws ssm start-session --profile asomasun-admin --region us-east-1 \
  --target <MONITOR_INSTANCE_ID> \
  --document-name AWS-StartPortForwardingSession \
  --parameters '{"portNumber":["3000"],"localPortNumber":["3080"]}'
```

Open http://localhost:3080. User `admin`, password from:

```bash
aws ssm get-parameter --profile asomasun-admin --region us-east-1 \
  --with-decryption --name /extenddb-bench/grafana-admin-password \
  --query Parameter.Value --output text
```

Three dashboards:
- **extenddb-bench live** — target/achieved RPS, p50/p90/p99/p999 latency, in-flight, errors. Empty when no sweep is running.
- **extenddb-bench hosts** — node_exporter for both LG and SUT (CPU, mem, network, disk IO/IOPS).
- **extenddb-bench storage (postgres)** — connections, TPS, tuples, block I/O, top queries by mean time.

To also see the raw Prometheus targets list:

```bash
aws ssm start-session --profile asomasun-admin --region us-east-1 \
  --target <MONITOR_INSTANCE_ID> \
  --document-name AWS-StartPortForwardingSession \
  --parameters '{"portNumber":["9090"],"localPortNumber":["9091"]}'
# then http://localhost:9091/targets
```

### Get a shell on an instance

```bash
aws ssm start-session --profile asomasun-admin --region us-east-1 --target <INSTANCE_ID>
```

Or via the helper:

```bash
./scripts/run-via-ssm.sh lg       # LG
./scripts/run-via-ssm.sh sut      # SUT
./scripts/run-via-ssm.sh monitor  # Monitor
```

You land as `ssm-user`. Use `sudo -i` for root.

### Run a sweep

On the LG (after `aws ssm start-session ... --target <LG_INSTANCE_ID>`):

```bash
# smoke (~30 s) — prove the bench dashboard lights up
bench-run --rps-sweep 500 --warmup 2s --duration 15s --cooldown 1s --iterations 1

# v0.1 acceptance shape (~7 min)
bench-run --rps-sweep 1000,3000,5000,10000 \
  --warmup 5s --duration 30s --cooldown 2s \
  --iterations 3 --connections 256 \
  --output /tmp/bench-$(date -u +%Y%m%dT%H%M%S)
```

`bench-run` is a wrapper installed at `/usr/local/bin/bench-run` that pulls
bench credentials from SSM Parameter Store and execs the load generator with
`--tls-insecure` (the LG-to-SUT hop is intra-SG with self-signed TLS; SigV4
still authenticates the request). All `extenddb-bench run` flags are forwarded.

While running, watch http://localhost:3080.

### Pull results to your laptop

After a run, results are at `/tmp/bench-<ts>/` on the LG. Copy them to S3
then `aws s3 sync` to the dev-dsk:

```bash
# on LG:
RUN_DIR=$(ls -td /tmp/bench-* | head -1)
RUN_ID=$(basename "$RUN_DIR")
aws s3 sync "$RUN_DIR/" "s3://extenddb-bench-results-$(aws sts get-caller-identity --query Account --output text)/runs/$RUN_ID/"
exit

# on dev-dsk:
./scripts/pull-results.sh "$RUN_ID"
```

`pull-results.sh` syncs to `./results/<run-id>/` and `less`'s the `summary.md`.

### Tail bench logs

```bash
# from your laptop, using SSM run-command (one-shot):
aws ssm send-command --profile asomasun-admin --region us-east-1 \
  --instance-ids <LG_INSTANCE_ID> \
  --document-name AWS-RunShellScript \
  --parameters 'commands=tail -50 /var/log/extenddb-bench-bootstrap.log'

# or get a shell and tail interactively:
aws ssm start-session --profile asomasun-admin --region us-east-1 --target <LG_INSTANCE_ID>
# then on LG: tail -f /var/log/extenddb-bench-bootstrap.log
```

### Teardown

```bash
cd infra

# leave monitor + dashboard alive (recommended — see your run's metrics later):
AWS_REGION=us-east-1 AWS_DEFAULT_REGION=us-east-1 AWS_PROFILE=asomasun-admin \
  npx cdk destroy ExtendDbBenchCompute --force \
  -c extenddbBranch=main -c extenddbCommit=<sha>

# nuke everything (Monitor's data EBS volume is RETAINed; manual delete to recover space):
AWS_REGION=us-east-1 AWS_DEFAULT_REGION=us-east-1 AWS_PROFILE=asomasun-admin \
  npx cdk destroy --all --force \
  -c extenddbBranch=main -c extenddbCommit=<sha>
```

## Cost

| component | hourly | notes |
|---|---|---|
| Compute (LG + SUT) | ~$1.99 | c7g.8xlarge + c7g.4xlarge + 1 TB gp3 16k IOPS |
| Monitor | ~$0.04 | t4g.medium + 50 GB gp3 |
| NAT + S3 | ~$0.05 | trivial |
| **All-on** | **~$2.10/hr** | |

A `deploy → sweep → destroy` cycle (~30 min) is ~$1. Leaving the monitor
alive for review is ~$1/day.

## License

Apache 2.0 (matches ExtendDB).
