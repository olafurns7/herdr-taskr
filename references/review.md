# taskr for a reviewer lane

A reviewer lane needs only this file and the core [SKILL.md](../SKILL.md); it does not load the orchestration skill. Output codes: [format.md](format.md).

1. Run `taskr got ATTEMPT` first, from the prompt's `First taskr got N`. A hook may have recorded it: `g1 ... dup` is fine.
2. Stay read-only. Write only the report path and the journal path that the brief names (a temporary sibling is allowed for an atomic journal replace). No source edits, commits, pushes, PRs, issue-tracker writes, agent starts, installs or daemon restarts.
3. Run probes and checks in a scratch directory: a scratch clone or copy, `HOME` and an explicit `TASKR_DB` under `mktemp -d`. Never touch the live ledger or another lane's worktree files.
4. Missing decision: `taskr ask "question" --blocking`, then `taskr wait --for answer` and `taskr ack EVENT --as $TASKR_TASK` after you handle it. On exit 5, rerun the printed `--request-key` line before you wait.
5. Finish: write the report, then `taskr ready "rev-SLUG" --report PATH`, then `taskr done "VERDICT: one line"` (or `taskr fail "reason"` when the review cannot finish). Reply with the report path and three lines.

The report states: the verdict; each finding with file:line, a failure case and a fix; the checks that hold; every command run with its exit code; the source identity reviewed (commit or hashes); and each probe path with the exact command to rerun it. Keep probes outside the repository at the evidence paths named by the brief; list every artifact left there. Ask for an external evidence path if one is missing.
