#!/usr/bin/env node
import "source-map-support/register";
import * as cdk from "aws-cdk-lib";
import { NetworkStack } from "../lib/network-stack";
import { ComputeStack } from "../lib/compute-stack";
import { resolveExtendDbRef } from "../lib/pr-resolver";

const app = new cdk.App();

const account =
  app.node.tryGetContext("account") ??
  process.env.CDK_DEFAULT_ACCOUNT ??
  "229776355219";
const region =
  app.node.tryGetContext("region") ??
  process.env.CDK_DEFAULT_REGION ??
  "us-east-1";
const env: cdk.Environment = { account, region };

// Version pinning: --branch+--commit OR --pr.
const ref = resolveExtendDbRef({
  branch: app.node.tryGetContext("extenddbBranch"),
  commit: app.node.tryGetContext("extenddbCommit"),
  pr: app.node.tryGetContext("extenddbPr"),
});

const network = new NetworkStack(app, "ExtendDbBenchNetwork", { env });
const compute = new ComputeStack(app, "ExtendDbBenchCompute", {
  env,
  vpc: network.vpc,
  resultsBucket: network.resultsBucket,
  benchSecurityGroup: network.benchSecurityGroup,
  extenddbSha: ref.sha,
  extenddbPr: ref.pr,
});
compute.addDependency(network);

cdk.Tags.of(app).add("project", "extenddb-bench");
cdk.Tags.of(app).add("owner", "asomasun");
cdk.Tags.of(app).add("poc-version", "0.1");
cdk.Tags.of(app).add("extenddb-sha", ref.sha);
if (ref.pr !== undefined) {
  cdk.Tags.of(app).add("extenddb-pr", String(ref.pr));
}
