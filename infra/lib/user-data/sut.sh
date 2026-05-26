#!/bin/bash
# extenddb-bench SUT bootstrap.
#
# Runs once at instance launch via cloud-init user-data. Writes structured logs
# to /var/log/extenddb-bench-bootstrap.log (also tee'd to console).
#
# Required environment placeholders (substituted by CDK at synth time):
#   __EXTENDDB_SHA__     40-char commit SHA (locked at synth time)
#   __SSM_PREFIX__       SSM Parameter Store key prefix (e.g. /extenddb-bench/)
#   __AWS_REGION__       AWS region (us-east-1)
#   __DATA_DEVICE__      Block device for the data EBS volume (/dev/nvme1n1)
#
# This script:
#   1. installs base toolchain (rust, postgres15, git, jq, python)
#   2. mounts the data EBS volume at /data
#   3. starts postgres15, creates an admin role
#   4. clones + builds ExtendDB at the pinned SHA
#   5. runs `extenddb init` (creates DBs, admin user, TLS cert)
#   6. starts ExtendDB via systemd
#   7. provisions a bench IAM user + access key via the management API
#   8. writes credentials to SSM Parameter Store (encrypted)
#   9. creates the `bench` table

set -euxo pipefail
export HOME=/root

EXTENDDB_SHA="__EXTENDDB_SHA__"
SSM_PREFIX="__SSM_PREFIX__"
AWS_REGION="__AWS_REGION__"
DATA_DEVICE="__DATA_DEVICE__"

LOG=/var/log/extenddb-bench-bootstrap.log
exec > >(tee -a "$LOG") 2>&1
echo ">>> SUT bootstrap starting at $(date -u +%FT%TZ) for SHA=$EXTENDDB_SHA"

mark_status() {
  echo "$1" > /var/run/extenddb-bench-bootstrap-status
}
mark_status "starting"

# 1. Base packages.
dnf install -y --quiet --allowerasing \
  git tar xz jq gcc gcc-c++ make pkgconfig openssl-devel \
  python3 python3-pip \
  postgresql15 postgresql15-server postgresql15-contrib

# 2. Data volume layout.
# AL2023 NVMe naming: the secondary EBS volume is /dev/nvme1n1.
# Format if not already formatted; mount at /data; persist via fstab.
if ! blkid "$DATA_DEVICE" >/dev/null 2>&1; then
  mkfs.ext4 -F -L extenddb-data "$DATA_DEVICE"
fi
mkdir -p /data
if ! mountpoint -q /data; then
  echo "LABEL=extenddb-data /data ext4 defaults,nofail,noatime 0 2" >> /etc/fstab
  mount /data
fi
mkdir -p /data/pgsql /data/extenddb /data/extenddb-config
chown postgres:postgres /data/pgsql

# 3. Initialize Postgres (data dir on EBS) and start it.
export PGDATA=/data/pgsql/data
if [ ! -d "$PGDATA" ]; then
  sudo -u postgres /usr/bin/initdb -D "$PGDATA" --auth-local=peer --auth-host=md5 --no-locale --encoding=UTF8
  # Listen on localhost only.
  sed -i "s/^#listen_addresses.*/listen_addresses = '127.0.0.1'/" "$PGDATA/postgresql.conf"
fi

# Override the systemd unit's PGDATA via a drop-in.
mkdir -p /etc/systemd/system/postgresql.service.d
cat > /etc/systemd/system/postgresql.service.d/override.conf <<EOF
[Service]
Environment=PGDATA=$PGDATA
EOF
systemctl daemon-reload
systemctl enable --now postgresql

# Wait for postgres to accept connections.
for i in $(seq 1 30); do
  if sudo -u postgres /usr/bin/psql -c "SELECT 1" >/dev/null 2>&1; then break; fi
  sleep 2
done

# Create a postgres admin role for ExtendDB to use during init.
PG_ADMIN_PASS="$(openssl rand -hex 16)"
sudo -u postgres /usr/bin/psql <<SQL
DO \$\$ BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'extenddb_admin') THEN
    CREATE ROLE extenddb_admin LOGIN SUPERUSER PASSWORD '$PG_ADMIN_PASS';
  ELSE
    ALTER ROLE extenddb_admin WITH LOGIN SUPERUSER PASSWORD '$PG_ADMIN_PASS';
  END IF;
END \$\$;
SQL

mark_status "postgres-ready"

# 4. Rust toolchain. Install for root since we run extenddb as root for v0.1.
if [ ! -f /root/.cargo/env ]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
fi
. /root/.cargo/env

# 5. Build ExtendDB at the pinned SHA.
if [ ! -d /opt/extenddb ]; then
  git clone --filter=tree:0 https://github.com/ExtendDB/extenddb /opt/extenddb
fi
cd /opt/extenddb
git fetch --quiet origin "$EXTENDDB_SHA" || git fetch --quiet origin
git checkout --quiet "$EXTENDDB_SHA"
git rev-parse HEAD > /etc/extenddb-version

# Build into a stable target dir on the data volume so subsequent rebuilds
# are incremental even if the root volume is small.
export CARGO_TARGET_DIR=/data/extenddb/target
cargo build --release --bin extenddb
install -m 755 "$CARGO_TARGET_DIR/release/extenddb" /usr/local/bin/extenddb

mark_status "extenddb-built"

# 6. Generate admin password and run `extenddb init`.
# `extenddb init` will create the application user and the catalog/data DBs
# using the postgres superuser credentials we just provisioned.
ADMIN_USER=admin
ADMIN_PASS="$(openssl rand -hex 24)"
EXTENDDB_USER=extenddb
EXTENDDB_PASS="extenddb-bench"

mkdir -p /etc/extenddb /var/lib/extenddb
chown root:root /etc/extenddb /var/lib/extenddb

# Bind on the instance's primary private IP so clients on the LG can reach it.
IMDS_TOKEN="$(curl -s -X PUT 'http://169.254.169.254/latest/api/token' -H 'X-aws-ec2-metadata-token-ttl-seconds: 21600')"
SUT_PRIVATE_IP="$(curl -s -H "X-aws-ec2-metadata-token: $IMDS_TOKEN" http://169.254.169.254/latest/meta-data/local-ipv4)"
echo "SUT private IP: $SUT_PRIVATE_IP"

# `extenddb init` provisions DBs + admin user + TLS cert.
EXTENDDB_ADMIN_USER="$ADMIN_USER" EXTENDDB_ADMIN_PASSWORD="$ADMIN_PASS" \
  /usr/local/bin/extenddb init \
    --config /etc/extenddb/extenddb.toml \
    --pg-host 127.0.0.1 --pg-port 5432 \
    --pg-user extenddb_admin --pg-pass "$PG_ADMIN_PASS" \
    --extenddb-user "$EXTENDDB_USER" --extenddb-pass "$EXTENDDB_PASS" \
    --bind-addr "$SUT_PRIVATE_IP" \
    --overwrite

chmod 600 /etc/extenddb/extenddb.toml

# Force the server bind to the SUT's private IP so the LG can connect and
# `extenddb manage` (which reads bind_addr as the connection target) works.
# init leaves bind_addr empty by default; we set it explicitly here.
sed -i "s|^bind_addr.*|bind_addr = \"$SUT_PRIVATE_IP\"|" /etc/extenddb/extenddb.toml
grep -q '^bind_addr' /etc/extenddb/extenddb.toml || \
  sed -i "/^\[server\]/a bind_addr = \"$SUT_PRIVATE_IP\"" /etc/extenddb/extenddb.toml

# 7. Systemd unit for ExtendDB. extenddb forks into the background; use Type=forking.
cat > /etc/systemd/system/extenddb.service <<EOF
[Unit]
Description=ExtendDB server
After=postgresql.service network-online.target
Wants=network-online.target
Requires=postgresql.service

[Service]
Type=forking
PIDFile=/root/.extenddb/run/extenddb-8000.pid
ExecStart=/usr/local/bin/extenddb serve --config /etc/extenddb/extenddb.toml
ExecStop=/usr/local/bin/extenddb stop --config /etc/extenddb/extenddb.toml
Restart=on-failure
RestartSec=5
User=root
LimitNOFILE=1048576
Environment=RUST_LOG=info
Environment=HOME=/root

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable --now extenddb

# Wait for ExtendDB to come up. /health is unauth and returns 200 OK.
TLS_CA=/root/.extenddb/tls/cert.pem
HEALTH_URL="https://127.0.0.1:8000/health"
for i in $(seq 1 60); do
  if curl --cacert "$TLS_CA" -fsS "$HEALTH_URL" >/dev/null 2>&1; then
    echo ">>> ExtendDB healthy after ${i} attempts"
    break
  fi
  sleep 2
done

mark_status "extenddb-running"

# 8. Provision a bench IAM user + access key via the management API.
# Mirrors devtools/provision-test-credentials but focused on the bench user.
ACCOUNT_ID=200000000001
ACCOUNT_NAME=bench
USER_NAME=bench
POLICY_NAME=full-access
TABLE_NAME=bench

cat > /tmp/full-access.json <<'JSON'
{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"dynamodb:*","Resource":"*"}]}
JSON

ee_manage() {
  EXTENDDB_PASSWORD="$ADMIN_PASS" /usr/local/bin/extenddb manage \
    --user "$ADMIN_USER" --config /etc/extenddb/extenddb.toml "$@"
}

ee_manage create-account --account-id "$ACCOUNT_ID" --account-name "$ACCOUNT_NAME" || true
ee_manage create-user --account-id "$ACCOUNT_ID" --user-name "$USER_NAME" || true
KEY_JSON="$(ee_manage create-access-key --account-id "$ACCOUNT_ID" --user-name "$USER_NAME")"
ACCESS_KEY_ID="$(echo "$KEY_JSON" | jq -r '.access_key_id')"
SECRET_ACCESS_KEY="$(echo "$KEY_JSON" | jq -r '.secret_access_key')"
ee_manage put-user-policy --account-id "$ACCOUNT_ID" --user-name "$USER_NAME" \
  --policy-name "$POLICY_NAME" --policy-document "$(cat /tmp/full-access.json)"

# 9. Create the bench table via aws CLI (single hash key `pk` of type S).
# AWS CLI is preinstalled on AL2023.
export AWS_ACCESS_KEY_ID="$ACCESS_KEY_ID"
export AWS_SECRET_ACCESS_KEY="$SECRET_ACCESS_KEY"
export AWS_DEFAULT_REGION="$AWS_REGION"
export AWS_CA_BUNDLE="$TLS_CA"
aws dynamodb create-table \
  --endpoint-url "https://$SUT_PRIVATE_IP:8000" \
  --table-name "$TABLE_NAME" \
  --attribute-definitions AttributeName=pk,AttributeType=S \
  --key-schema AttributeName=pk,KeyType=HASH \
  --billing-mode PAY_PER_REQUEST \
  --no-cli-pager 2>&1 || echo "(table may already exist)"

# 10. Publish bench credentials + connection details to SSM Parameter Store.
unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_CA_BUNDLE
PUT="aws ssm put-parameter --region $AWS_REGION --overwrite"
$PUT --name "${SSM_PREFIX}admin-password"     --type SecureString --value "$ADMIN_PASS"
$PUT --name "${SSM_PREFIX}access-key-id"      --type SecureString --value "$ACCESS_KEY_ID"
$PUT --name "${SSM_PREFIX}secret-access-key"  --type SecureString --value "$SECRET_ACCESS_KEY"
$PUT --name "${SSM_PREFIX}account-id"         --type String       --value "$ACCOUNT_ID"
$PUT --name "${SSM_PREFIX}sut-private-ip"     --type String       --value "$SUT_PRIVATE_IP"
$PUT --name "${SSM_PREFIX}extenddb-sha"       --type String       --value "$EXTENDDB_SHA"
$PUT --name "${SSM_PREFIX}table-name"         --type String       --value "$TABLE_NAME"

# Also dump the TLS cert as a parameter so the LG can trust the self-signed cert.
TLS_CERT_B64="$(base64 -w0 < "$TLS_CA")"
$PUT --name "${SSM_PREFIX}tls-cert-b64"       --type String       --value "$TLS_CERT_B64"

mark_status "ready"
echo ">>> SUT bootstrap complete at $(date -u +%FT%TZ)"
