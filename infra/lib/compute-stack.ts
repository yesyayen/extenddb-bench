import * as cdk from "aws-cdk-lib";
import * as ec2 from "aws-cdk-lib/aws-ec2";
import * as iam from "aws-cdk-lib/aws-iam";
import * as s3 from "aws-cdk-lib/aws-s3";
import * as ssm from "aws-cdk-lib/aws-ssm";
import { Construct } from "constructs";
import * as fs from "node:fs";
import * as path from "node:path";

export interface ComputeStackProps extends cdk.StackProps {
  vpc: ec2.IVpc;
  resultsBucket: s3.IBucket;
  benchSecurityGroup: ec2.ISecurityGroup;
  /** 40-char ExtendDB commit SHA (locked at synth time). */
  extenddbSha: string;
  /** Optional PR number, embedded in tags. */
  extenddbPr?: number;
  /** Bench harness repo URL. */
  benchRepoUrl?: string;
  /** Bench harness git ref. */
  benchRepoRef?: string;
}

const SSM_PREFIX = "/extenddb-bench/";

export class ComputeStack extends cdk.Stack {
  public readonly sutInstance: ec2.Instance;
  public readonly lgInstance: ec2.Instance;

  constructor(scope: Construct, id: string, props: ComputeStackProps) {
    super(scope, id, props);

    const benchRepoUrl =
      props.benchRepoUrl ?? "https://github.com/yesyayen/extenddb-bench";
    const benchRepoRef = props.benchRepoRef ?? "main";

    // Cluster placement group: lowest-latency intra-AZ networking.
    const placementGroup = new ec2.PlacementGroup(this, "BenchPg", {
      strategy: ec2.PlacementGroupStrategy.CLUSTER,
    });

    // AL2023 ARM64 AMI lookup (latest).
    const ami = ec2.MachineImage.fromSsmParameter(
      "/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-arm64",
      { os: ec2.OperatingSystemType.LINUX },
    );

    // IAM role shared by both instances.
    // SSM Session Manager + S3 results write + SSM Parameter Store read+write
    // (scoped to the bench prefix).
    const role = new iam.Role(this, "BenchInstanceRole", {
      assumedBy: new iam.ServicePrincipal("ec2.amazonaws.com"),
      description: "extenddb-bench: SSM + S3 results + SSM Parameter Store",
      managedPolicies: [
        iam.ManagedPolicy.fromAwsManagedPolicyName("AmazonSSMManagedInstanceCore"),
      ],
    });
    props.resultsBucket.grantReadWrite(role);
    role.addToPolicy(
      new iam.PolicyStatement({
        actions: [
          "ssm:GetParameter",
          "ssm:GetParameters",
          "ssm:PutParameter",
          "ssm:DeleteParameter",
        ],
        resources: [
          `arn:aws:ssm:${this.region}:${this.account}:parameter${SSM_PREFIX}*`,
        ],
      }),
    );
    // SSM Parameter Store SecureString uses the AWS-managed default KMS key.
    role.addToPolicy(
      new iam.PolicyStatement({
        actions: ["kms:Encrypt", "kms:Decrypt", "kms:GenerateDataKey"],
        resources: ["*"],
        conditions: {
          StringEquals: { "kms:ViaService": `ssm.${this.region}.amazonaws.com` },
        },
      }),
    );

    // Note: SSM Parameter Store values are populated by the SUT/LG/monitor
    // user-data via `aws ssm put-parameter --overwrite`. We do NOT create
    // them as CDK resources because they need to survive Compute-stack
    // destroy (the monitor stack reads them after compute is gone).
    // The IAM policy below grants both put and delete on the prefix.

    // Render user-data scripts with placeholders substituted.
    const sutUserData = renderUserData("sut.sh", {
      __EXTENDDB_SHA__: props.extenddbSha,
      __SSM_PREFIX__: SSM_PREFIX,
      __AWS_REGION__: this.region,
      __DATA_DEVICE__: "/dev/nvme1n1",
    });
    const lgUserData = renderUserData("lg.sh", {
      __BENCH_REPO__: benchRepoUrl,
      __BENCH_REF__: benchRepoRef,
      __SSM_PREFIX__: SSM_PREFIX,
      __AWS_REGION__: this.region,
    });

    // SUT: c7g.4xlarge with a 1 TB gp3 data volume attached separately.
    // The data volume is created as a standalone Volume + VolumeAttachment so we
    // can specify iops and throughput (the L2 Instance construct's blockDevices
    // does not propagate `throughput` to the underlying CFN resource: see
    // https://github.com/aws/aws-cdk/issues/34033).
    this.sutInstance = new ec2.Instance(this, "Sut", {
      vpc: props.vpc,
      vpcSubnets: { subnetType: ec2.SubnetType.PRIVATE_WITH_EGRESS },
      instanceType: ec2.InstanceType.of(
        ec2.InstanceClass.C7G,
        ec2.InstanceSize.XLARGE4,
      ),
      machineImage: ami,
      role,
      securityGroup: props.benchSecurityGroup,
      placementGroup,
      blockDevices: [
        {
          deviceName: "/dev/xvda",
          volume: ec2.BlockDeviceVolume.ebs(30, {
            volumeType: ec2.EbsDeviceVolumeType.GP3,
            deleteOnTermination: true,
            encrypted: true,
          }),
        },
      ],
      requireImdsv2: true,
      userDataCausesReplacement: true,
    });
    const sutDataVolume = new ec2.Volume(this, "SutDataVolume", {
      availabilityZone: cdk.Fn.select(0, props.vpc.availabilityZones),
      size: cdk.Size.gibibytes(1024),
      volumeType: ec2.EbsDeviceVolumeType.GP3,
      iops: 16000,
      throughput: 1000,
      encrypted: true,
      removalPolicy: cdk.RemovalPolicy.DESTROY,
    });
    new ec2.CfnVolumeAttachment(this, "SutDataAttach", {
      instanceId: this.sutInstance.instanceId,
      volumeId: sutDataVolume.volumeId,
      device: "/dev/sdb",
    });
    this.sutInstance.addUserData(sutUserData);
    cdk.Tags.of(this.sutInstance).add("Name", "extenddb-bench-sut");
    cdk.Tags.of(this.sutInstance).add("role", "sut");

    // LG: c7g.8xlarge, root volume only.
    this.lgInstance = new ec2.Instance(this, "Lg", {
      vpc: props.vpc,
      vpcSubnets: { subnetType: ec2.SubnetType.PRIVATE_WITH_EGRESS },
      instanceType: ec2.InstanceType.of(
        ec2.InstanceClass.C7G,
        ec2.InstanceSize.XLARGE8,
      ),
      machineImage: ami,
      role,
      securityGroup: props.benchSecurityGroup,
      placementGroup,
      blockDevices: [
        {
          deviceName: "/dev/xvda",
          volume: ec2.BlockDeviceVolume.ebs(30, {
            volumeType: ec2.EbsDeviceVolumeType.GP3,
            deleteOnTermination: true,
            encrypted: true,
          }),
        },
      ],
      requireImdsv2: true,
      userDataCausesReplacement: true,
    });
    this.lgInstance.addUserData(lgUserData);
    cdk.Tags.of(this.lgInstance).add("Name", "extenddb-bench-lg");
    cdk.Tags.of(this.lgInstance).add("role", "lg");

    // Outputs.
    new cdk.CfnOutput(this, "SutInstanceId", {
      value: this.sutInstance.instanceId,
      description: "SUT EC2 instance ID (use with `aws ssm start-session`)",
    });
    new cdk.CfnOutput(this, "SutPrivateIp", {
      value: this.sutInstance.instancePrivateIp,
      description: "SUT private IP — pass as --target https://<ip>:8000",
    });
    new cdk.CfnOutput(this, "LgInstanceId", {
      value: this.lgInstance.instanceId,
      description: "LG EC2 instance ID (use with `aws ssm start-session`)",
    });
    new cdk.CfnOutput(this, "LgPrivateIp", {
      value: this.lgInstance.instancePrivateIp,
    });
    new cdk.CfnOutput(this, "ExtendDbSha", {
      value: props.extenddbSha,
      description: "Pinned ExtendDB commit SHA",
    });
    new cdk.CfnOutput(this, "SsmPrefix", {
      value: SSM_PREFIX,
      description: "SSM Parameter Store prefix for bench credentials",
    });

    // SSM document: extenddb-bench-swap-sha.
    // Operator-driven: sequential same-SUT SHA swap for compare runs.
    // Steps: stop unit -> fetch+checkout -> cargo build --release -> install -> start -> health-poll.
    const swapDoc = new ssm.CfnDocument(this, "SwapShaDoc", {
      name: "extenddb-bench-swap-sha",
      documentType: "Command",
      documentFormat: "YAML",
      updateMethod: "NewVersion",
      content: {
        schemaVersion: "2.2",
        description:
          "extenddb-bench: swap the SUT's ExtendDB binary to a target SHA. Same Postgres, same instance, no CDK redeploy.",
        parameters: {
          sha: {
            type: "String",
            description: "Target ExtendDB commit SHA (40-char).",
            allowedPattern: "^[0-9a-fA-F]{7,40}$",
          },
        },
        mainSteps: [
          {
            action: "aws:runShellScript",
            name: "swapExtendDb",
            inputs: {
              runCommand: [
                "#!/bin/bash",
                "set -euxo pipefail",
                "SHA={{ sha }}",
                "export HOME=/root",
                ". /root/.cargo/env",
                "export CARGO_TARGET_DIR=/data/extenddb/target",
                "systemctl stop extenddb || true",
                "cd /opt/extenddb",
                "git fetch --quiet --all",
                "git checkout --quiet \"$SHA\"",
                "git rev-parse HEAD > /etc/extenddb-version",
                "cargo build --release --bin extenddb",
                "install -m 755 \"$CARGO_TARGET_DIR/release/extenddb\" /usr/local/bin/extenddb",
                "systemctl start extenddb",
                "TLS_CA=/root/.extenddb/tls/cert.pem",
                "BIND_ADDR=$(awk -F'\"' '/^bind_addr/ {print $2; exit}' /etc/extenddb/extenddb.toml)",
                "HEALTH_URL=https://${BIND_ADDR}:8000/health",
                "for i in $(seq 1 60); do",
                "  if curl --cacert \"$TLS_CA\" -fsS \"$HEALTH_URL\" >/dev/null 2>&1; then",
                "    echo \"healthy after $i attempts on $HEALTH_URL; sha=$(cat /etc/extenddb-version)\"",
                "    exit 0",
                "  fi",
                "  sleep 2",
                "done",
                "echo 'ExtendDB never became healthy after swap; checked '\"$HEALTH_URL\"",
                "systemctl status extenddb --no-pager || true",
                "exit 1",
              ],
            },
          },
        ],
      },
    });
    new cdk.CfnOutput(this, "SwapShaDocName", {
      value: swapDoc.name!,
      description: "SSM document for sequential SHA swap (compare-shas.sh)",
    });
  }
}

function renderUserData(filename: string, vars: Record<string, string>): string {
  const filePath = path.join(__dirname, "user-data", filename);
  let body = fs.readFileSync(filePath, "utf-8");
  for (const [key, value] of Object.entries(vars)) {
    body = body.replaceAll(key, value);
  }
  // CDK's `UserData.forLinux()` prepends `#!/bin/bash`. Strip a leading
  // shebang from our script body so the rendered cloud-init script has
  // exactly one shebang line.
  return body.replace(/^#![^\n]*\n/, "");
}
