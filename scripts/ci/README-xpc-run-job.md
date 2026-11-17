# xpc-run-job.sh

`xpc-run-job.sh` is the job wrapper on the weftos-ci host (aepod-xpc). There it is installed as `~/ci-jobs/run-job.sh`. Lane builds call it through `ci-run.sh` in research/weftos-agent-notes/scratch/ci/.

- **Job slots:** it runs one lane job in a rootless podman container, with up to `CI_SLOTS` jobs at once (slot locks).
- **Logs:** each run appends to `~/ci-jobs/<lane>/logs/<job>.log` and writes `<job>.status`.

Change of 2026-11-17: `.status` records the real result.
- The job runs under `bash -o pipefail`.
- When the container exits 0, the wrapper scans that run's log lines for cargo and test failure markers:
  - `could not compile`
  - `test result: FAILED`
  - cargo's "test failed" / "process didn't exit successfully"
  - `RC <name> <non-zero>`
- On a hit it writes `exit 1 (container exit 0, but the log shows a failure: <line>)`.

Before this change, a job such as `cargo test ...; echo RC $?` wrote "exit 0" on a compile error.

This copy is the source of record. To deploy it, copy it to xpc as a new file and rename it into place, so a running job never reads a half-written script:

    scp scripts/ci/xpc-run-job.sh aepod@aepod-xpc:ci-jobs/run-job.sh.new && ssh aepod@aepod-xpc 'mv ~/ci-jobs/run-job.sh.new ~/ci-jobs/run-job.sh'
