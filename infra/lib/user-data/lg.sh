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
export HOME=/root

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
dnf install -y --quiet --allowerasing \
  git tar xz jq gcc gcc-c++ make pkgconfig openssl-devel \
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

# node_exporter for host metrics (CPU, mem, net, disk).
NODE_EXP_VERSION=1.8.2
if [ ! -x /usr/local/bin/node_exporter ]; then
  cd /tmp
  curl -fsSL "https://github.com/prometheus/node_exporter/releases/download/v${NODE_EXP_VERSION}/node_exporter-${NODE_EXP_VERSION}.linux-arm64.tar.gz" -o ne.tgz
  tar xzf ne.tgz
  install -m 755 "node_exporter-${NODE_EXP_VERSION}.linux-arm64/node_exporter" /usr/local/bin/node_exporter
fi
useradd -r -s /sbin/nologin node_exporter 2>/dev/null || true
cat > /etc/systemd/system/node_exporter.service <<EOF
[Unit]
Description=node_exporter
After=network-online.target

[Service]
Type=simple
User=node_exporter
ExecStart=/usr/local/bin/node_exporter --web.listen-address=0.0.0.0:9100
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable --now node_exporter

# Publish LG IP to SSM so the monitor can scrape us.
IMDS_TOKEN=$(curl -s -X PUT 'http://169.254.169.254/latest/api/token' -H 'X-aws-ec2-metadata-token-ttl-seconds: 21600')
LG_IP=$(curl -s -H "X-aws-ec2-metadata-token: $IMDS_TOKEN" http://169.254.169.254/latest/meta-data/local-ipv4)
aws ssm put-parameter --region "$AWS_REGION" --overwrite \
  --name "${SSM_PREFIX}lg-private-ip" --type String --value "$LG_IP"

# Convenience wrapper that fetches creds from SSM and execs extenddb-bench.
cat > /usr/local/bin/bench-run <<EOF
#!/bin/bash
# Fetch bench creds from SSM, install the SUT TLS cert into the OS trust store,
# and exec extenddb-bench.
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

# Install the SUT cert into the AL2023 system trust store. update-ca-trust
# regenerates /etc/pki/ca-trust/extracted/, which rustls-native-certs reads.
mkdir -p /etc/pki/ca-trust/source/anchors
echo "\$TLS_CERT_B64" | base64 -d > /etc/pki/ca-trust/source/anchors/extenddb-sut.pem
chmod 644 /etc/pki/ca-trust/source/anchors/extenddb-sut.pem
update-ca-trust extract

export AWS_ACCESS_KEY_ID="\$ACCESS_KEY_ID"
export AWS_SECRET_ACCESS_KEY="\$SECRET_ACCESS_KEY"
export AWS_REGION="\$AWS_REGION"
export EXTENDDB_BENCH_SHA="\$EXTENDDB_SHA"

exec /usr/local/bin/extenddb-bench run \\
  --target "https://\$SUT_IP:8000" \\
  --table-name "\$TABLE_NAME" \\
  --aws-region "\$AWS_REGION" \\
  "\$@"
EOF
chmod 755 /usr/local/bin/bench-run

mark_status "ready"
echo ">>> LG bootstrap complete at $(date -u +%FT%TZ)"
