# extenddb-bench

Performance testing harness for [ExtendDB](https://github.com/ExtendDB/extenddb).

Two-instance EC2 framework that measures ExtendDB's PutItem saturation throughput
end-to-end. CDK-provisioned, source-built (no Docker), open-loop Rust load gen,
HDR-histogram tail latency. Reproducible to within ±5% relative stddev across
iterations.

## Status

**v0.1 POC.** Single workload (`putitem-1kb`), single config, no comparison
oracle. See [`docs/design-v0.1.md`](docs/design-v0.1.md) for the locked design
and explicit out-of-scope items.

## Quick start

Requirements: AWS profile `asomasun-admin` with admin in `us-east-1`,
Node 18+, Rust 1.85+, `gh` CLI for PR-id resolution mode.

```bash
# 1. Resolve a target ExtendDB ref and deploy
cd infra
npm install
npx cdk deploy -c extenddbBranch=main -c extenddbCommit=<40-char-sha>
# or:
npx cdk deploy -c extenddbPr=<pr-id>

# 2. Find the LG instance and start a session
aws ssm start-session --target <lg-instance-id> --profile asomasun-admin

# 3. Run a sweep on the LG
extenddb-bench run \
  --target https://<sut-private-ip>:8000 \
  --rps-sweep 1000,5000,25000,100000,250000

# 4. Pull results and tear down
aws s3 sync s3://extenddb-bench-results-<account>/runs/<ts>/ ./results/<ts>/
cd ../infra && npx cdk destroy
```

## Repo layout

```text
extenddb-bench/
├── infra/         CDK TypeScript stack (network + compute)
├── loadgen/       Rust load generator (open-loop, HDR histograms)
├── docs/          Design doc + operator notes
└── scripts/       Operator convenience wrappers
```

## Cost

A full `deploy → sweep → destroy` cycle is ~30 min and ~$1 in `us-east-1`.

## License

Apache 2.0 (matches ExtendDB).
