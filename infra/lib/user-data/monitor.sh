#!/bin/bash
# extenddb-bench monitor host bootstrap.
#
# Brings up Prometheus + Grafana on a single t4g.medium instance with the
# data dir on a persistent EBS volume. Three dashboards are auto-provisioned:
# bench, hosts, storage.
#
# Required environment placeholders (substituted by CDK at synth time):
#   __SSM_PREFIX__       SSM Parameter Store key prefix
#   __AWS_REGION__       AWS region
#   __DATA_DEVICE__      Block device for the persistent monitor EBS volume
#   __GRAFANA_PASSWORD__ Initial admin password (random per deploy)

set -euxo pipefail
export HOME=/root

SSM_PREFIX="__SSM_PREFIX__"
AWS_REGION="__AWS_REGION__"
DATA_DEVICE="__DATA_DEVICE__"
GRAFANA_PASSWORD="__GRAFANA_PASSWORD__"

LOG=/var/log/extenddb-bench-bootstrap.log
exec > >(tee -a "$LOG") 2>&1
echo ">>> monitor bootstrap starting at $(date -u +%FT%TZ)"

mark_status() { echo "$1" > /var/run/extenddb-bench-bootstrap-status; }
mark_status "starting"

# 1. Base packages.
dnf install -y --quiet --allowerasing \
  tar xz jq gcc make pkgconfig \
  python3 python3-pip ca-certificates

# 2. Persistent data volume at /data.
if ! blkid "$DATA_DEVICE" >/dev/null 2>&1; then
  mkfs.ext4 -F -L monitor-data "$DATA_DEVICE"
fi
mkdir -p /data
if ! mountpoint -q /data; then
  echo "LABEL=monitor-data /data ext4 defaults,nofail,noatime 0 2" >> /etc/fstab
  mount /data
fi
mkdir -p /data/prometheus /data/grafana

# 3. Prometheus.
PROM_VERSION=2.55.1
if [ ! -x /usr/local/bin/prometheus ]; then
  cd /tmp
  curl -fsSL "https://github.com/prometheus/prometheus/releases/download/v${PROM_VERSION}/prometheus-${PROM_VERSION}.linux-arm64.tar.gz" \
    -o prom.tgz
  tar xzf prom.tgz
  install -m 755 "prometheus-${PROM_VERSION}.linux-arm64/prometheus" /usr/local/bin/prometheus
  install -m 755 "prometheus-${PROM_VERSION}.linux-arm64/promtool" /usr/local/bin/promtool
fi

mkdir -p /etc/prometheus
cat > /etc/prometheus/prometheus.yml <<'YAML'
global:
  scrape_interval: 5s
  evaluation_interval: 15s

scrape_configs:
  - job_name: prometheus
    static_configs:
      - targets: ['localhost:9090']

  - job_name: bench
    file_sd_configs:
      - files: ['/etc/prometheus/targets/bench.json']

  - job_name: node
    file_sd_configs:
      - files: ['/etc/prometheus/targets/node.json']

  - job_name: postgres
    file_sd_configs:
      - files: ['/etc/prometheus/targets/postgres.json']
YAML

mkdir -p /etc/prometheus/targets
echo '[]' > /etc/prometheus/targets/bench.json
echo '[]' > /etc/prometheus/targets/node.json
echo '[]' > /etc/prometheus/targets/postgres.json

# Periodic target refresh from SSM Parameter Store. Prometheus picks up changes
# to file_sd targets without a reload.
cat > /usr/local/bin/refresh-targets <<'SCRIPT'
#!/bin/bash
set -euo pipefail
SSM_PREFIX="__SSM_PREFIX_PLACEHOLDER__"
AWS_REGION="__AWS_REGION_PLACEHOLDER__"
fetch() { aws ssm get-parameter --region "$AWS_REGION" --name "$1" --query 'Parameter.Value' --output text 2>/dev/null || true; }
SUT_IP=$(fetch "${SSM_PREFIX}sut-private-ip")
LG_IP=$(fetch "${SSM_PREFIX}lg-private-ip")

emit_json() {
  local out=$1; shift
  jq -n --argjson tg "$1" --argjson lb "$2" '[{targets: $tg, labels: $lb}]' > "$out.tmp"
  mv "$out.tmp" "$out"
}

bench_targets='[]'
node_targets='[]'
pg_targets='[]'
if [ -n "$LG_IP" ] && [ "$LG_IP" != "None" ]; then
  bench_targets=$(jq -n --arg ip "$LG_IP" '["\($ip):9090"]')
fi
if [ -n "$LG_IP" ] && [ "$LG_IP" != "None" ] && [ -n "$SUT_IP" ] && [ "$SUT_IP" != "None" ]; then
  node_targets=$(jq -n --arg lg "$LG_IP" --arg sut "$SUT_IP" '["\($lg):9100", "\($sut):9100"]')
elif [ -n "$LG_IP" ] && [ "$LG_IP" != "None" ]; then
  node_targets=$(jq -n --arg lg "$LG_IP" '["\($lg):9100"]')
elif [ -n "$SUT_IP" ] && [ "$SUT_IP" != "None" ]; then
  node_targets=$(jq -n --arg sut "$SUT_IP" '["\($sut):9100"]')
fi
if [ -n "$SUT_IP" ] && [ "$SUT_IP" != "None" ]; then
  pg_targets=$(jq -n --arg ip "$SUT_IP" '["\($ip):9187"]')
fi

emit_json /etc/prometheus/targets/bench.json    "$bench_targets" '{"role":"lg"}'
emit_json /etc/prometheus/targets/node.json     "$node_targets"  '{}'
emit_json /etc/prometheus/targets/postgres.json "$pg_targets"   '{"role":"sut"}'
SCRIPT
sed -i "s|__SSM_PREFIX_PLACEHOLDER__|$SSM_PREFIX|; s|__AWS_REGION_PLACEHOLDER__|$AWS_REGION|" /usr/local/bin/refresh-targets
chmod 755 /usr/local/bin/refresh-targets
/usr/local/bin/refresh-targets || true

cat > /etc/systemd/system/refresh-targets.service <<EOF
[Unit]
Description=Refresh Prometheus file_sd targets from SSM
After=network-online.target

[Service]
Type=oneshot
ExecStart=/usr/local/bin/refresh-targets
EOF

cat > /etc/systemd/system/refresh-targets.timer <<EOF
[Unit]
Description=Refresh Prometheus targets every 30s

[Timer]
OnBootSec=15s
OnUnitActiveSec=30s

[Install]
WantedBy=timers.target
EOF

useradd -r -s /sbin/nologin prometheus 2>/dev/null || true
chown -R prometheus:prometheus /data/prometheus

cat > /etc/systemd/system/prometheus.service <<EOF
[Unit]
Description=Prometheus
After=network-online.target

[Service]
Type=simple
User=prometheus
ExecStart=/usr/local/bin/prometheus \\
  --config.file=/etc/prometheus/prometheus.yml \\
  --storage.tsdb.path=/data/prometheus \\
  --storage.tsdb.retention.time=14d \\
  --web.listen-address=0.0.0.0:9090
Restart=on-failure
RestartSec=5
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now prometheus
systemctl enable --now refresh-targets.timer

# 4. Grafana (OSS) from official RPM repo.
cat > /etc/yum.repos.d/grafana.repo <<'REPO'
[grafana]
name=grafana
baseurl=https://rpm.grafana.com
repo_gpgcheck=1
enabled=1
gpgcheck=1
gpgkey=https://rpm.grafana.com/gpg.key
sslverify=1
sslcacert=/etc/pki/tls/certs/ca-bundle.crt
REPO

dnf install -y --quiet grafana

# Grafana data on the persistent EBS volume.
mkdir -p /data/grafana
chown -R grafana:grafana /data/grafana
mkdir -p /etc/grafana/provisioning/{datasources,dashboards}
mkdir -p /var/lib/grafana/dashboards

cat > /etc/grafana/grafana.ini <<EOF
[server]
http_addr = 0.0.0.0
http_port = 3000

[paths]
data = /data/grafana
logs = /data/grafana/logs
plugins = /data/grafana/plugins
provisioning = /etc/grafana/provisioning

[security]
admin_user = admin
admin_password = $GRAFANA_PASSWORD

[users]
allow_sign_up = false

[auth.anonymous]
enabled = false
EOF

cat > /etc/grafana/provisioning/datasources/prometheus.yaml <<'YAML'
apiVersion: 1
datasources:
  - name: Prometheus
    uid: PBFA97CFB590B2093
    type: prometheus
    access: proxy
    url: http://localhost:9090
    isDefault: true
    editable: false
YAML

cat > /etc/grafana/provisioning/dashboards/default.yaml <<'YAML'
apiVersion: 1
providers:
  - name: extenddb-bench
    orgId: 1
    folder: ''
    type: file
    disableDeletion: false
    updateIntervalSeconds: 30
    allowUiUpdates: true
    options:
      path: /var/lib/grafana/dashboards
YAML

# Dashboard JSON is fetched from the bench repo at build time so the
# monitor doesn't need to know about it at synth time.
DASHBOARDS_DIR=/var/lib/grafana/dashboards
curl -fsSL https://raw.githubusercontent.com/yesyayen/extenddb-bench/main/infra/lib/dashboards/bench.json   -o "$DASHBOARDS_DIR/bench.json"
curl -fsSL https://raw.githubusercontent.com/yesyayen/extenddb-bench/main/infra/lib/dashboards/hosts.json   -o "$DASHBOARDS_DIR/hosts.json"
curl -fsSL https://raw.githubusercontent.com/yesyayen/extenddb-bench/main/infra/lib/dashboards/storage.json -o "$DASHBOARDS_DIR/storage.json"
chown -R grafana:grafana "$DASHBOARDS_DIR" /etc/grafana/provisioning

systemctl enable --now grafana-server

# Publish the Grafana URL hint (private IP, port-forward via SSM).
IMDS_TOKEN=$(curl -s -X PUT 'http://169.254.169.254/latest/api/token' -H 'X-aws-ec2-metadata-token-ttl-seconds: 21600')
MON_IP=$(curl -s -H "X-aws-ec2-metadata-token: $IMDS_TOKEN" http://169.254.169.254/latest/meta-data/local-ipv4)

aws ssm put-parameter --region "$AWS_REGION" --overwrite \
  --name "${SSM_PREFIX}monitor-private-ip" --type String --value "$MON_IP"
aws ssm put-parameter --region "$AWS_REGION" --overwrite \
  --name "${SSM_PREFIX}grafana-admin-password" --type SecureString --value "$GRAFANA_PASSWORD"

mark_status "ready"
echo ">>> monitor bootstrap complete at $(date -u +%FT%TZ)"
