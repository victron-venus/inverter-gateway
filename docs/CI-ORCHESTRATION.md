# CI orchestration

Quality gate owns pull request and merge queue validation. The release pipeline
owns default branch and nightly checks for applications; validation-only projects
run those events through Quality gate. Callable validators do not launch duplicate
runs. Superseded PR runs are cancelled, and validation jobs have explicit timeouts.

The required CI configuration contracts check CodeQL pin compatibility, validation
triggers, timeouts, and complete gate dependencies. Dependency Review, where present,
blocks the same gate for pull requests and reports other events as not applicable.
CodeQL action updates are grouped where Dependabot Actions updates are configured.

Auto-merge retains the check-waiting mode because the conditional Gitar review
check does not appear on every PR. It waits for all observed checks, including Gitar,
without making absent conditional checks a permanent merge blocker. BOT_PAT remains
the merger token; approval token policies are unchanged. The gate and external
security checks retain strict branch protection and verified source GitHub Apps.
