# Operations and limitations

## Resource sizing

Each VM defaults to 4 GiB RAM, 2 CPUs, and a 20 GiB disk. Clones keep the source disk and
inherit CPU and RAM unless overridden. This is intentionally heavier than Compose project
names; use plain worktrees or a shared daemon when branch-specific runtime state and unchanged
localhost assumptions are not worth that cost.

Cloned VMs keep the clone source's disk size. `--disk` applies only to fresh provisioning,
while `--memory` and `--cpus` can override inherited values.

## Lifecycle and cleanup

- Deleting a worktree does not delete its VM because git provides no hook for wtx. `wtx ls`
  and the TUI mark these VMs as `orphaned`.
- VM preparation through `up`, `new`, or `ensure` runs an automatic orphan sweep at most once
  per hour. The first sweep stops a newly orphaned VM and records when it was observed. If its
  worktree does not return, a later VM preparation deletes it after a seven-day recovery
  window. The current target and a `--from` source are excluded from that sweep.
- Automatic and bulk prune skip legacy isolated-Git VMs because their commits may exist only
  inside the VM. `wtx ls`, the TUI, and JSON inspection flag them for manual inspection and
  explicit `wtx rm` cleanup.
- `wtx prune --yes` still removes all eligible current orphans immediately. Set
  `WTX_NO_AUTO_PRUNE=1` to disable automatic sweeps, for example when worktrees live on an
  external volume that can remain unavailable for more than seven days.
- `wtx rm NAME --with-worktree` removes a VM and its linked worktree together. It deliberately
  leaves a normal repository directory untouched.
- Commits made in a current wtx VM are already stored in the host `.git`, so VM deletion does
  not discard committed work.
- VMs from old isolated-git versions do not propagate commits to the host. Reattaching with
  `wtx up` detects and warns about them; recreate those VMs.

## Registry cache registration

The launchd plist stores the executable path used by `wtx mirror install`. Registering through
a stable PATH symlink survives build-directory moves. If a directly registered build artifact
is moved or removed by `cargo clean`, run `wtx mirror install` again. Re-run it after editing
`~/.wtx/mirrors.json` as well, because the socket list must be regenerated.

## Known limitations

- **Transparent caching is limited to Docker Hub.** Docker Engine 29 applies
  `registry-mirrors` only to Hub. Explicitly configured endpoints still support pulls such as
  `docker pull localhost:5002/<org>/<image>`.
- VMs created before credential sharing became opt-in may retain an old `~/.claude` mount and
  agent-forwarding setting. Recreate them; mount policy cannot be safely changed on reattach.
- `wtx exec` does not interpret shell syntax. Pass pipes and similar expressions through
  `bash -c '...'` or another shell explicitly.
- `--from` changes Compose volume names by matching the `<directory-name>_` prefix. If the
  project name comes from an invisible source such as `COMPOSE_PROJECT_NAME`, rename the
  volumes manually with `docker volume`.
- With `--agent-access`, `git push` inside a VM works only while the host ssh-agent holds the
  required key.

## Verification

[VERIFICATION.md](../VERIFICATION.md) is the full real-VM lab notebook, including rejected
designs and observed failure modes. The main end-to-end checks are:

- `scripts/check-worktree-lifecycle.sh`: create, commit propagation, concurrent commits,
  delete, orphan detection, and prune.
- `scripts/check-seed.sh`: `--from` seeding, volume renaming, Compose adoption, shared-Git
  behavior, and source restart.
- `scripts/check-sim.sh`: simulator lifecycle.

These scripts create and remove real VMs. The lifecycle check refuses to run when unrelated
orphaned VMs already exist so its prune step cannot remove them.
