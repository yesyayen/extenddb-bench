import * as cdk from "aws-cdk-lib";
import * as ec2 from "aws-cdk-lib/aws-ec2";
import * as iam from "aws-cdk-lib/aws-iam";
import { Construct } from "constructs";
import * as crypto from "node:crypto";
import * as fs from "node:fs";
import * as path from "node:path";

export interface MonitorStackProps extends cdk.StackProps {
  vpc: ec2.IVpc;
  benchSecurityGroup: ec2.ISecurityGroup;
}

const SSM_PREFIX = "/extenddb-bench/";

/**
 * Monitor stack: a single t4g.medium running Prometheus + Grafana with
 * persistent storage on a 50 GB gp3 volume. Independent of ComputeStack so
 * the dashboard survives `cdk destroy ExtendDbBenchCompute`.
 *
 * The monitor reads SSM parameters (`/extenddb-bench/sut-private-ip`,
 * `/extenddb-bench/lg-private-ip`) every 30s and rewrites Prometheus
 * `file_sd` targets, so it auto-discovers fresh LG/SUT replacements.
 */
export class MonitorStack extends cdk.Stack {
  public readonly instance: ec2.Instance;

  constructor(scope: Construct, id: string, props: MonitorStackProps) {
    super(scope, id, props);

    const ami = ec2.MachineImage.fromSsmParameter(
      "/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-arm64",
      { os: ec2.OperatingSystemType.LINUX },
    );

    const role = new iam.Role(this, "MonitorRole", {
      assumedBy: new iam.ServicePrincipal("ec2.amazonaws.com"),
      description: "extenddb-bench monitor: SSM + Parameter Store read/write",
      managedPolicies: [
        iam.ManagedPolicy.fromAwsManagedPolicyName("AmazonSSMManagedInstanceCore"),
      ],
    });
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
    role.addToPolicy(
      new iam.PolicyStatement({
        actions: ["kms:Encrypt", "kms:Decrypt", "kms:GenerateDataKey"],
        resources: ["*"],
        conditions: {
          StringEquals: { "kms:ViaService": `ssm.${this.region}.amazonaws.com` },
        },
      }),
    );

    // Random Grafana admin password generated at synth. Persists across
    // re-deploys because we only re-roll if context `monitorPasswordRoll=1`.
    const grafanaPassword = grafanaAdminPassword(this);

    const userData = renderUserData("monitor.sh", {
      __SSM_PREFIX__: SSM_PREFIX,
      __AWS_REGION__: this.region,
      __DATA_DEVICE__: "/dev/nvme1n1",
      __GRAFANA_PASSWORD__: grafanaPassword,
    });

    this.instance = new ec2.Instance(this, "Monitor", {
      vpc: props.vpc,
      vpcSubnets: { subnetType: ec2.SubnetType.PRIVATE_WITH_EGRESS },
      instanceType: ec2.InstanceType.of(
        ec2.InstanceClass.T4G,
        ec2.InstanceSize.MEDIUM,
      ),
      machineImage: ami,
      role,
      securityGroup: props.benchSecurityGroup,
      blockDevices: [
        {
          deviceName: "/dev/xvda",
          volume: ec2.BlockDeviceVolume.ebs(20, {
            volumeType: ec2.EbsDeviceVolumeType.GP3,
            deleteOnTermination: true,
            encrypted: true,
          }),
        },
      ],
      requireImdsv2: true,
      userDataCausesReplacement: true,
    });
    this.instance.addUserData(userData);
    cdk.Tags.of(this.instance).add("Name", "extenddb-bench-monitor");
    cdk.Tags.of(this.instance).add("role", "monitor");

    // Persistent metrics + Grafana state on a separate volume retained on stack delete.
    const dataVolume = new ec2.Volume(this, "MonitorDataVolume", {
      availabilityZone: cdk.Fn.select(0, props.vpc.availabilityZones),
      size: cdk.Size.gibibytes(50),
      volumeType: ec2.EbsDeviceVolumeType.GP3,
      encrypted: true,
      removalPolicy: cdk.RemovalPolicy.RETAIN,
    });
    new ec2.CfnVolumeAttachment(this, "MonitorDataAttach", {
      instanceId: this.instance.instanceId,
      volumeId: dataVolume.volumeId,
      device: "/dev/sdb",
    });

    new cdk.CfnOutput(this, "MonitorInstanceId", {
      value: this.instance.instanceId,
      description: "Monitor EC2 ID; SSM port-forward 3000 to reach Grafana",
    });
    new cdk.CfnOutput(this, "MonitorPrivateIp", {
      value: this.instance.instancePrivateIp,
    });
    new cdk.CfnOutput(this, "GrafanaPortForwardCmd", {
      value:
        `aws ssm start-session --region ${this.region} --target ${this.instance.instanceId} ` +
        `--document-name AWS-StartPortForwardingSession ` +
        `--parameters '{"portNumber":["3000"],"localPortNumber":["3000"]}'`,
      description: "Run, then open http://localhost:3000",
    });
    new cdk.CfnOutput(this, "GrafanaPasswordSsmCmd", {
      value:
        `aws ssm get-parameter --region ${this.region} --with-decryption ` +
        `--name ${SSM_PREFIX}grafana-admin-password --query Parameter.Value --output text`,
      description: "Fetch the Grafana admin password (user: admin)",
    });
  }
}

function renderUserData(filename: string, vars: Record<string, string>): string {
  const filePath = path.join(__dirname, "user-data", filename);
  let body = fs.readFileSync(filePath, "utf-8");
  for (const [key, value] of Object.entries(vars)) {
    body = body.replaceAll(key, value);
  }
  // Strip a leading shebang since CDK's UserData.forLinux already prepends one.
  return body.replace(/^#![^\n]*\n/, "");
}

/**
 * Random Grafana admin password generated fresh per `cdk synth`.
 * The actual value is stashed in SSM Parameter Store by user-data so
 * the operator can always retrieve it via the printed command.
 */
function grafanaAdminPassword(_scope: Construct): string {
  return crypto.randomBytes(18).toString("base64").replace(/[+/=]/g, "").slice(0, 24);
}
