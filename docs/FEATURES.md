# Feature guide

This page covers wtx behavior that is useful after the first run. For installation and a
minimal workflow, start with the [README](../README.md).

## Runtime model

- **One Lima/vz microVM per worktree.** Each VM has a private rootful Docker Engine,
  volumes, images, and localhost namespace. The VM is a runtime boundary, not a security
  boundary.
- **Same-path mounts.** virtiofs mounts the worktree at the same absolute path as the host,
  so host-side editors and tools continue to work normally.
- **Shared Git metadata.** The main `.git` directory is mounted read-write. Commits made in
  a VM update the host branch directly; worktrees still retain independent indexes and HEADs.
- **Host-side credentials by default.** `~/.claude` is not mounted and ssh-agent forwarding
  is disabled. `--agent-access` opts trusted, VM-resident agents into both at VM creation.
- **Explicit networking.** Lima's automatic port forwarding is disabled. `wtx forward`
  publishes a VM port to the host; `wtx bridge` exposes a host service inside the VM.
- **Pinned runtime.** Docker Engine and its plugins are version-pinned. Git identity is
  reapplied from the host after fresh provisioning and cloning instead of being baked into
  the golden image.

The exact trust and credential boundary is documented in [TRUST-MODEL.md](TRUST-MODEL.md).

## Automatic orphan cleanup

Git has no worktree-removal hook that wtx can use, so deleting a worktree cannot synchronously
delete its VM. Instead, `up`, `new`, and `ensure` scan for orphaned VMs at most once per hour.
The first observation stops the orphan and starts a seven-day recovery window. If the
worktree returns, its marker is cleared; otherwise a later VM setup deletes the VM, its
recorded forwards, and its dedicated simulator.

`wtx ls` and the TUI show whether the recovery window has started. `wtx prune --yes` remains
the immediate cleanup path. Set `WTX_NO_AUTO_PRUNE=1` when a worktree path can intentionally
disappear for longer, such as on a detached external volume.

VMs from the old isolated-Git design may contain commits that never reached the host. Automatic
cleanup stops but does not delete those VMs, and bulk `prune` skips them. Inspect them and use
explicit `wtx rm` only after deciding that their VM-local Git state is no longer needed.

## Seed an environment (`wtx up --from`)

`wtx up NAME DIR --from SRC` clones an existing VM instead of the golden image. Docker
volumes, including database data, pulled images, and installed tools carry over. This is
useful for creating a feature environment from an already migrated and populated main VM.

The source VM stops only while its disk is copied at rest, then restarts in the background.
wtx automatically changes Compose volume prefixes from the source directory name to the new
one. A project with a fixed Compose `name:` keeps that name, and inherited containers are
removed from the new VM.

## Built-in registry cache (`wtx mirror`)

wtx includes a pull-through registry cache that does not require Docker to run. Blob hits
and misses stream without buffering whole layers, HEAD and Range requests are supported, and
a blob is stored only after its SHA-256 digest verifies. Manifests are always refreshed from
upstream because tags can move.

`wtx mirror install` registers launchd socket activation. The cache starts on the first pull
and exits when idle. Its default 20 GiB limit is enforced by evicting the oldest blobs after
writes; `wtx mirror gc --max-gib N` changes the persistent limit and collects immediately.

Docker Engine applies transparent `registry-mirrors` only to Docker Hub, so the default
installation enables that endpoint alone. Extra registries can be configured in
`~/.wtx/mirrors.json` for explicit localhost pulls.

## Named host ports (`wtx port` / `wtx env`)

`wtx port add api:3000` allocates a free host port, records it under the `api` label, and
forwards it to port 3000 in the current worktree's VM. This works for any VM service and does
not require an iOS simulator. Use `--name NAME` when running outside the worktree.

`eval "$(wtx env)"` exports the allocation as `WTX_PORT_API` together with `WTX_VM_NAME` and
`WTX_WORKDIR`. `wtx env --json` returns the same data for agents and scripts. Both forms
re-arm recorded SSH forwards after a VM restart. Seeded VMs retain label-to-guest definitions
but receive new host ports, so source and destination can run concurrently.

Use `wtx forward HOST:GUEST` when a specific host port is required. The older `wtx sim wire`
and `wtx sim env` forms remain compatibility aliases for `wtx port add` and `wtx env`.

## Per-worktree iOS simulators (`wtx sim`)

CoreSimulator runs on the macOS host, so wtx creates a host-side device named `wtx-NAME` and
ties its lifecycle to the worktree VM. `wtx up --sim` creates it, `rm` and `prune` remove it,
and `--from` clones its apps and data.

`wtx port add api:3000` allocates and records a host port for a VM port. From the worktree,
`eval "$(wtx env)"` exports `WTX_SIM_UDID` and named port variables such as
`WTX_PORT_API`. External tools should resolve `sim_udid` immediately before use and bind that
exact device rather than choosing the first booted or focused simulator.

See [DESIGN-sim.md](DESIGN-sim.md) for the design and verified usage contract.

## TUI console (`wtx` / `wtx tui`)

The TUI groups VMs by their main repository and shows VM state and mirror health together.
Project headings fold with `Space`, arrow keys, or `Enter`. Pressing `Enter` on a VM suspends
the TUI, opens a shell in that VM, and restores the interface when the shell exits.

Start, stop, delete, and polling operations run in the background. The affected row shows a
spinner and elapsed time while navigation remains responsive. `wtx tui --snapshot` renders
one frame without a tty for smoke tests.

## Updates

`wtx upgrade` refreshes the Homebrew tap metadata and upgrades wtx. `wtx update check` checks
without installing; `--json` returns a versioned machine-readable result.

Only the interactive TUI performs a background update check. It checks at most once every 24
hours, stays silent on failures, and can be disabled with `WTX_NO_UPDATE_CHECK=1`.
