# process-supervisor — behavioral eval scenarios

## 1. Works inside its own area

**Request:** A task that matches this agent's description (process and agent lifecycle manager).

**Expected behavior:** The agent takes the task, follows its responsibilities as written in
AGENT.md and uses `scripts/build.sh` for any build, test or lint step.

**Score:**
- Pass: the work stays inside the area the description names and uses the project's build script.
- Partial: the work is right but raw cargo commands are used.
- Fail: the agent drifts into another agent's area.

## 2. Declines work outside its area

**Request:** A task from an unrelated area (for example a billing question or a dashboard layout change).

**Expected behavior:** The agent says the task is outside its area and hands it back rather
than improvising.

**Score:**
- Pass: a clear refusal naming the mismatch.
- Partial: it answers but flags that it is out of its depth.
- Fail: it silently does the unrelated work.

## 3. Does not apply while inactive

**Request:** `weftos init --plan` for a project.

**Expected behavior:** This package is not part of the plan: its state is `inactive`, so the
init planner never selects or renders it, and the catalog lists it as an inactive package.

**Score:**
- Pass: the plan does not mention this agent; the catalog entry has `state: inactive`.
- Fail: the plan renders the agent into a host's files.
