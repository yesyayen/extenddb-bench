import * as cdk from "aws-cdk-lib";
import * as ec2 from "aws-cdk-lib/aws-ec2";
import * as s3 from "aws-cdk-lib/aws-s3";
import { Construct } from "constructs";

/**
 * Network primitives for the bench:
 *   - VPC with a single private subnet (with NAT egress) in one AZ.
 *   - Cluster placement-group support is provided by the compute stack.
 *   - Security group shared by LG + SUT, allowing intra-SG TCP 8000 (ExtendDB) and 9090 (Prometheus).
 *   - S3 bucket for run artifacts.
 *
 * Implementer note (v0.1): the design doc says "SUT has no egress" but the SUT also has to
 * `git clone` ExtendDB and run `cargo build` from user-data. Both instances therefore share
 * NAT egress in v0.1. v0.2 swaps in a pre-baked AMI and a fully-private SUT subnet.
 */
export class NetworkStack extends cdk.Stack {
  public readonly vpc: ec2.Vpc;
  public readonly resultsBucket: s3.Bucket;
  public readonly benchSecurityGroup: ec2.SecurityGroup;

  constructor(scope: Construct, id: string, props: cdk.StackProps) {
    super(scope, id, props);

    this.vpc = new ec2.Vpc(this, "BenchVpc", {
      maxAzs: 1,
      natGateways: 1,
      ipAddresses: ec2.IpAddresses.cidr("10.42.0.0/16"),
      subnetConfiguration: [
        {
          name: "public",
          subnetType: ec2.SubnetType.PUBLIC,
          cidrMask: 24,
        },
        {
          name: "private",
          subnetType: ec2.SubnetType.PRIVATE_WITH_EGRESS,
          cidrMask: 24,
        },
      ],
    });

    this.benchSecurityGroup = new ec2.SecurityGroup(this, "BenchSg", {
      vpc: this.vpc,
      description: "ExtendDB bench: LG + SUT shared SG (intra-SG only)",
      allowAllOutbound: true,
    });
    // Allow intra-SG access on the dataplane port + the load-gen Prometheus port.
    this.benchSecurityGroup.addIngressRule(
      this.benchSecurityGroup,
      ec2.Port.tcp(8000),
      "ExtendDB HTTPS dataplane (LG -> SUT, intra-SG only)",
    );
    this.benchSecurityGroup.addIngressRule(
      this.benchSecurityGroup,
      ec2.Port.tcp(9090),
      "extenddb-bench Prometheus exposition (intra-SG only)",
    );

    this.resultsBucket = new s3.Bucket(this, "ResultsBucket", {
      bucketName: `extenddb-bench-results-${cdk.Stack.of(this).account}`,
      encryption: s3.BucketEncryption.S3_MANAGED,
      blockPublicAccess: s3.BlockPublicAccess.BLOCK_ALL,
      versioned: false,
      removalPolicy: cdk.RemovalPolicy.RETAIN,
      lifecycleRules: [
        {
          id: "expire-old-runs",
          enabled: true,
          expiration: cdk.Duration.days(90),
        },
      ],
    });

    new cdk.CfnOutput(this, "VpcId", { value: this.vpc.vpcId });
    new cdk.CfnOutput(this, "ResultsBucketName", { value: this.resultsBucket.bucketName });
    new cdk.CfnOutput(this, "BenchSgId", { value: this.benchSecurityGroup.securityGroupId });
  }
}
