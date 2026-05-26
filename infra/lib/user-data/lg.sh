#!/bin/bash
# extenddb-bench LG bootstrap.
#
# Runs once at instance launch via cloud-init user-data. Writes structured logs
# to /var/log/extenddb-bench-bootstrap.log. The LG is stateless. After
# bootstrap, an operator opens an SSM session and runs `extenddb-bench run`
# manually, fetching credentials from SSM Parameter Store.
#
# Required environment placeholders (substituted by CDK at synth time):
#   __BENCH_REPO__       Bench harness repo URL (e.g. https://github.com/yesyayen/extenddb-bench)
#   __BENCH_REF__        git ref to check out (e.g. main)
#   __SSM_PREFIX__       SSM Parameter Store key prefix
#   __AWS_REGION__       AWS region (us-east-1)

set -euxo pipefail

BENCH_REPO="__BENCH_REPO__"
BENCH_REF="__BENCH_REF__"
SSM_PREFIX="__SSM_PREFIX__"
AWS_REGION="__AWS_REGION__"

LOG=/var/log/extenddb-bench-bootstrap.log
exec > >(tee -a "$LOG") 2>&1
echo ">>> LG bootstrap starting at $(date -u +%FT%TZ) ref=$BENCH_REF"

mark_status() {
  echo "$1" > /var/run/extenddb-bench-bootstrap-status
}
mark_status "starting"

# Base toolchain.
dnf install -y --quiet \
  git tar gzip xz jq curl gcc gcc-c++ make pkgconfig openssl-devel \
  python3 python3-pip

# Rust toolchain.
if [ ! -f /root/.cargo/env ]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
fi
. /root/.cargo/env

# Clone the bench harness and build the load gen.
if [ ! -d /opt/extenddb-bench ]; then
  git clone --depth 1 --branch "$BENCH_REF" "$BENCH_REPO" /opt/extenddb-bench
fi
cd /opt/extenddb-bench/loadgen
cargo build --release --bin extenddb-bench
install -m 755 target/release/extenddb-bench /usr/local/bin/extenddb-bench

# Convenience wrapper that fetches creds from SSM and execs extenddb-bench.
cat > /usr/local/bin/bench-run <<EOF
#!/bin/bash
# Fetch bench creds from SSM, materialize the TLS cert, and exec extenddb-bench.
set -euo pipefail
SSM_PREFIX="$SSM_PREFIX"
AWS_REGION="$AWS_REGION"
fetch() { aws ssm get-parameter --region "\$AWS_REGION" --with-decryption --name "\$1" --query 'Parameter.Value' --output text; }
ACCESS_KEY_ID="\$(fetch \${SSM_PREFIX}access-key-id)"
SECRET_ACCESS_KEY="\$(fetch \${SSM_PREFIX}secret-access-key)"
SUT_IP="\$(fetch \${SSM_PREFIX}sut-private-ip)"
EXTENDDB_SHA="\$(fetch \${SSM_PREFIX}extenddb-sha)"
TLS_CERT_B64="\$(fetch \${SSM_PREFIX}tls-cert-b64)"
TABLE_NAME="\$(fetch \${SSM_PREFIX}table-name)"

mkdir -p /etc/extenddb-bench
echo "\$TLS_CERT_B64" | base64 -d > /etc/extenddb-bench/sut-tls.pem
chmod 600 /etc/extenddb-bench/sut-tls.pem

export AWS_ACCESS_KEY_ID="\$ACCESS_KEY_ID"
export AWS_SECRET_ACCESS_KEY="\$SECRET_ACCESS_KEY"
export AWS_REGION="\$AWS_REGION"
export EXTENDDB_BENCH_SHA="\$EXTENDDB_SHA"
export EXTENDDB_BENCH_CA_BUNDLE=/etc/extenddb-bench/sut-tls.pem

exec /usr/local/bin/extenddb-bench run \\
  --target "https://\$SUT_IP:8000" \\
  --table-name "\$TABLE_NAME" \\
  --aws-region "\$AWS_REGION" \\
  --tls-ca-bundle /etc/extenddb-bench/sut-tls.pem \\
  "\$@"
EOF
chmod 755 /usr/local/bin/bench-run

mark_status "ready"
echo ">>> LG bootstrap complete at $(date -u +%FT%TZ)"
