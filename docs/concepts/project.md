---
status: canonical
type: concept
authority: product
last-reviewed: 2026-07-25
---

# Project

A project groups sessions which operate on the same source repository. For Git
repositories, Brood uses the common Git directory as the stable project
identity. The primary checkout and every linked worktree therefore remain one
project even though their working directories and branches differ.

Each session retains its own working directory, branch, panes, processes,
agents, and plugin state. Project grouping is presentation metadata; it does not
merge session authority or lifecycle.

The session picker presents one collapsible section per project. A Git project
heading can create a branch and linked worktree in Brood's managed worktree
area under the XDG data directory. Successful creation also creates, activates,
and associates a session whose working directory is the new worktree. Branch
names remain Git branch names; an existing local branch is attached rather than
recreated. Filesystem-safe managed directory names are an internal storage
detail. Retrying an interrupted creation reuses the valid managed checkout and
activates its existing session when present.

Directories outside Git still form path-identified project groups, but do not
offer Git worktree creation.

Related:

- [[session]]
- [[workspace]]
- [[session-runner]]
- [[../experiments/session-switcher-ui]]
