#!/usr/bin/env bash
# Print operator commands for the running v0.15 stack.
# Reads SSM Parameter Store + EC2 to fill in instance IDs and IPs.

set -euo pipefail

PROFILE="${AWS_PROFILE:-asomasun-admin}"
REGION="${AWS_REGION:-us-east-1}"
PREFIX="/extenddb-bench/"

ec2_id() {
  aws ec2 describe-instances --profile "$PROFILE" --region "$REGION" \
    --filters "Name=tag:project,Values=extenddb-bench" "Name=tag:role,Values=$1" \
              "Name=instance-state-name,Values=running" \
    --query 'Reservations[*].Instances[*].InstanceId' --output text | head -n1
}

ssm_get() {
  aws ssm get-parameter --profile "$PROFILE" --region "$REGION" \
    --name "${PREFIX}$1" --with-decryption \
    --query 'Parameter.Value' --output text 2>/dev/null || echo "<unset>"
}

LG_ID=$(ec2_id lg)
SUT_ID=$(ec2_id sut)
MON_ID=$(ec2_id monitor)
LG_IP=$(ssm_get lg-private-ip)
SUT_IP=$(ssm_get sut-private-ip)
MON_IP=$(ssm_get monitor-private-ip)
GRAFANA_PW=$(ssm_get grafana-admin-password)

cat <<EOF
extenddb-bench v0.15 operator cheatsheet
=========================================

instances
  LG      $LG_ID  ($LG_IP)
  SUT     $SUT_ID ($SUT_IP)
  MONITOR $MON_ID ($MON_IP)

shells (SSM Session Manager)
  LG      aws ssm start-session --profile $PROFILE --region $REGION --target $LG_ID
  SUT     aws ssm start-session --profile $PROFILE --region $REGION --target $SUT_ID
  MONITOR aws ssm start-session --profile $PROFILE --region $REGION --target $MON_ID

grafana (port-forward to localhost:3000)
  aws ssm start-session --profile $PROFILE --region $REGION --target $MON_ID \\
    --document-name AWS-StartPortForwardingSession \\
    --parameters '{"portNumber":["3000"],"localPortNumber":["3000"]}'
  user: admin / pw: $GRAFANA_PW
  dashboards: bench-live, bench-hosts, bench-storage

prometheus (optional, same idea on port 9090)
  aws ssm start-session --profile $PROFILE --region $REGION --target $MON_ID \\
    --document-name AWS-StartPortForwardingSession \\
    --parameters '{"portNumber":["9090"],"localPortNumber":["9091"]}'
  open http://localhost:9091/targets

run a sweep on the LG (one of these forms)
  aws ssm start-session --profile $PROFILE --region $REGION --target $LG_ID
  # then on the host:
  bench-run --rps-sweep 1000,5000,25000 --warmup 5s --duration 30s --iterations 3

tail bench logs from your laptop
  aws ssm start-session --profile $PROFILE --region $REGION --target $LG_ID
  # then: tail -f /var/log/extenddb-bench-bootstrap.log  (bootstrap)
  #       journalctl -f                                  (anything else)

teardown (REQUIRES YOUR ACK)
  cd ~/projects/extenddb-bench/infra
  npx cdk destroy ExtendDbBenchCompute  # leaves monitor + dashboard alive
  npx cdk destroy --all                 # nukes everything; data on retained EBS volume
EOF
