# GPT-5.6 Luna Medium smoke evaluation

## Verdict

This sample is consistent with Brood's internal agent being near vanilla Codex,
but it is too small and too variable to establish stable parity. Brood resolved
9 of 10 tasks in its baseline run, compared with 8 of 10 for Codex. A second
Brood run after the reliability change resolved 7 of 10. The mean of the two
Brood runs is therefore 8 of 10, equal to the single Codex run, but a mean over
two trials is not a statistically reliable parity claim.

The code change addresses a demonstrated operational weakness: before it,
transient model-service errors terminated two Brood attempts. The agent now
retries a narrow set of transient failures and reports aggregate token usage.
This improved request reliability, but the second correctness score does not
show an eval-score improvement.

## Method

- Dataset: SWE-bench Verified at dataset commit
  `78f471bf655a3137b2e8a75af1501690ec009ec3`.
- Sample: seed `20260819`; six random tasks estimated below one hour and four
  random tasks estimated at one to four hours.
- Control: Codex CLI 0.148.0 with user configuration disabled.
- Both agents: `gpt-5.6-luna`, medium reasoning, the same issue text and base
  commit, no access to the gold patch, and official SWE-bench 5.0.2 grading at
  commit `7a21e05772954cc81471ae19d56f436cecf43c54`.
- The sample and model are fixed between runs. No task-specific prompt or tool
  changes were made after examining grader outcomes.

## Scores

| Stratum | Brood baseline | Codex baseline | Brood after retry change |
| --- | ---: | ---: | ---: |
| Below one hour | 6/6 | 6/6 | 6/6 |
| One to four hours | 3/4 | 2/4 | 1/4 |
| Total | **9/10** | **8/10** | **7/10** |

All numbers are official SWE-bench resolved counts. The fixed sample is a smoke
test, not a representative estimate of either product's full benchmark score.
The five-point swing in Brood's hard stratum across only four tasks is direct
evidence that repeated trials or a larger sample are needed.

## Reliability and usage

The original Brood run needed 11 attempts to produce the 10-patch set. One
attempt stopped on HTTP 503 before making a useful patch; another stopped on
HTTP 503 after leaving a patch that passed grading. Nine attempts reached a
clean agent result. The replacement Matplotlib attempt is the one included in
the baseline score and usage totals.

The second Brood run reached a clean agent result on 9 of 10 tasks. The remaining
task stopped after producing a passing patch when the service returned a
sporadic HTTP 400 claiming that `prompt_cache_retention` was unsupported; Brood
does not send that field. The retry policy was extended to that exact provider
error before the remaining tasks ran. Broad HTTP 400 errors are still not
retried.

| Run | Clean completions | Input tokens | Cached input | Output tokens | Wall time | Estimated list price |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Brood baseline | 9/10 | 1,422,241 | 474,368 | 24,199 | 12m 05s | at least $0.2281 |
| Codex baseline | 10/10 | 4,472,791 | 4,052,992 | 42,078 | 16m 42s | $0.2155 |
| Brood after retry change | 9/10 | 1,177,017 | 290,816 | 23,387 | 11m 22s | at least $0.2111 |

Brood totals omit the terminated request in each Brood run, so its price is a
lower bound. Estimates use the published Luna rates of $0.20 per million
uncached input tokens, $0.02 per million cached input tokens, and $1.20 per
million output tokens. Brood processed fewer tokens, while Codex received much
more cached-input billing; this sample does not establish a Brood cost advantage.

## Reproduction and artifacts

The checked-in runner, sample, grader wrapper, and commands are documented in
`evals/coding-agent/README.md`. Raw run artifacts remain under
`~/.cache/brood-agent-evals/runs`. Official reports are:

- `~/.cache/brood-agent-evals/SWE-bench/brood-gpt-5.6-luna-medium.brood-baseline-full.json`
- `~/.cache/brood-agent-evals/SWE-bench/brood-gpt-5.6-luna-medium.brood-baseline-hard.json`
- `~/.cache/brood-agent-evals/SWE-bench/codex-gpt-5.6-luna-medium.codex-baseline-full.json`
- `~/.cache/brood-agent-evals/SWE-bench/codex-gpt-5.6-luna-medium.codex-baseline-hard.json`
- `~/.cache/brood-agent-evals/SWE-bench/brood-gpt-5.6-luna-medium.brood-tuned-full.json`

The downloaded rootful Podman evaluation images occupy about 27 GB and are
retained to make reruns cheap. They are fully reclaimable according to
`sudo podman system df`.

The next confidence-building step is to run at least three repetitions of a
larger fixed sample and compare paired task outcomes. The next efficiency target
is Brood's low cached-input ratio, measured separately from correctness changes.
