# Coding-agent evaluation

This lane compares Brood's internal repository agent with an uncustomized Codex
CLI control while holding the model, reasoning effort, issue text, base commit,
and grader constant.

The checked-in sample uses seed `20260819` to draw six SWE-bench Verified tasks
estimated below one hour and four tasks estimated at one to four hours. It is a
stratified smoke evaluation, not a statistically precise estimate of full-set
performance.

The completed Luna Medium comparison, including both Brood runs and the vanilla
Codex control, is recorded in `results/2026-08-19-luna-medium.md`.

Run either harness from the repository root:

```sh
node evals/coding-agent/run-swebench.mjs brood baseline
node evals/coding-agent/run-swebench.mjs codex baseline
```

Pass instance IDs after the run ID to run a subset. Reusing a run ID replaces
that harness's workspaces and prediction file. Artifacts are written below
`$BROOD_AGENT_EVAL_CACHE/runs` or `~/.cache/brood-agent-evals/runs` by default.

Grade each prediction file with a distinct grader run ID. The wrapper installs
the official SWE-bench harness in the eval cache, pre-pulls the selected images,
and uses rootful Podman because the published images contain IDs outside this
host's rootless user namespace. Agent inference remains unprivileged. The
official harness caches by run ID and instance ID.

```sh
evals/coding-agent/grade-swebench.sh \
  ~/.cache/brood-agent-evals/runs/baseline/brood-predictions.jsonl \
  brood-baseline 2
```

Keep network access available to the model API and repository setup, but the
task explicitly prohibits using the network to find an upstream solution.
Codex commands run in its `workspace-write` sandbox. Brood's shell does not yet
have a separate network namespace, so this remains a known isolation gap.
