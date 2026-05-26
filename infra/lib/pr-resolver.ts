import { execSync } from "node:child_process";

export interface ResolvedRef {
  /** 40-char commit SHA, locked at synth time. */
  sha: string;
  /** PR number, if resolved via PR mode. */
  pr?: number;
}

export interface RefInputs {
  branch?: string;
  commit?: string;
  pr?: string | number;
}

const SHA40 = /^[0-9a-f]{40}$/;
const UPSTREAM_REPO = "ExtendDB/extenddb";

/**
 * Locks an ExtendDB ref to a 40-char commit SHA at synth time.
 *
 * Two mutually-exclusive modes:
 *
 *   --branch <name> --commit <sha>   pinned, no network call
 *   --pr <id>                        resolves via `gh api` to current PR head SHA
 *
 * The resolved SHA is written into a CloudFormation tag and into
 * /etc/extenddb-version on the SUT, and is embedded in every result file.
 */
export function resolveExtendDbRef(inputs: RefInputs): ResolvedRef {
  const hasBranchCommit = inputs.branch !== undefined || inputs.commit !== undefined;
  const hasPr = inputs.pr !== undefined && inputs.pr !== "";

  if (hasBranchCommit && hasPr) {
    throw new Error(
      "Specify either --branch+--commit OR --pr, not both. " +
        `Got branch=${inputs.branch} commit=${inputs.commit} pr=${inputs.pr}.`,
    );
  }

  if (hasBranchCommit) {
    if (!inputs.commit) {
      throw new Error(
        "extenddbBranch was set without extenddbCommit. " +
          "v0.1 requires both. Pass -c extenddbCommit=<40-char-sha>.",
      );
    }
    if (!SHA40.test(inputs.commit)) {
      throw new Error(
        `extenddbCommit must be a 40-char SHA. Got: ${inputs.commit}`,
      );
    }
    return { sha: inputs.commit };
  }

  if (hasPr) {
    const prNum = Number(inputs.pr);
    if (!Number.isInteger(prNum) || prNum <= 0) {
      throw new Error(`extenddbPr must be a positive integer. Got: ${inputs.pr}`);
    }
    const sha = resolvePrToSha(prNum);
    return { sha, pr: prNum };
  }

  throw new Error(
    "No ExtendDB ref pinned. Pass either:\n" +
      "  -c extenddbBranch=main -c extenddbCommit=<40-char-sha>\n" +
      "  -c extenddbPr=<pr-id>",
  );
}

function resolvePrToSha(prNumber: number): string {
  const cmd = `gh api repos/${UPSTREAM_REPO}/pulls/${prNumber} --jq .head.sha`;
  let out: string;
  try {
    out = execSync(cmd, { encoding: "utf-8", stdio: ["ignore", "pipe", "pipe"] }).trim();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    throw new Error(
      `Failed to resolve PR #${prNumber} via gh CLI.\n` +
        `Command: ${cmd}\n` +
        `Error: ${msg}\n\n` +
        `Confirm: gh auth status; gh repo view ${UPSTREAM_REPO}.`,
    );
  }
  if (!SHA40.test(out)) {
    throw new Error(
      `gh api returned an unexpected SHA for PR #${prNumber}: ${JSON.stringify(out)}`,
    );
  }
  return out;
}
