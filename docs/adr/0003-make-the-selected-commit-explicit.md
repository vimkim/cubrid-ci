# Make the selected commit explicit

An implicit worktree collection selects local `HEAD` and requires it to equal the published PR head. An explicit pull-request collection requires `--commit <SHA>` even when that SHA is currently the PR head, preventing a moving remote head from silently changing the evidence identity.
