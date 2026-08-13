# Command reference

Run `wtx --help` or `wtx <command> --help` for the authoritative option list.

| Command | What it does |
|---|---|
| `wtx new BRANCH [--dir DIR]` | Create a git worktree and its VM in one step. The branch is created if missing; `--from`, `--sim`, and other VM options apply. |
| `wtx up [NAME] [DIR]` | Create or start a VM for an existing worktree. With no arguments, resolve it from the current directory. Also triggers the rate-limited automatic orphan sweep. |
| `wtx up NAME DIR --from SRC` | Create a VM seeded with another VM's volumes, images, and tools. |
| `wtx ensure [NAME] [DIR] [--json]` | Idempotently create or start a VM, run the rate-limited automatic orphan sweep, and wait for dockerd. |
| `wtx inspect [NAME] [--json]` | Report VM/worktree readiness, seed, ports, and simulator state. |
| `wtx exec [--name NAME] [-w DIR] [--tty] -- CMD…` | Run a command in the cwd-resolved VM; the current directory is also the default guest directory. |
| `wtx shell [NAME]` | Open an interactive VM shell. NAME resolves from the current directory when omitted. |
| `wtx ls [--json]` | List VMs and flag VMs whose worktrees are gone. |
| `wtx` / `wtx tui` | Open the TUI console; `--snapshot` renders one frame without a tty. |
| `wtx port add [--name NAME] LABEL:GUEST` | Allocate, record, and arm a collision-free host port for a VM service. |
| `wtx env [NAME] [--json]` | Print `WTX_VM_NAME`, `WTX_WORKDIR`, and `WTX_PORT_*`; re-arm recorded forwards. |
| `wtx forward [--name NAME] HOST:GUEST` | Publish a VM port on the host with SSH local forwarding. |
| `wtx bridge [--name NAME] HOST:GUEST` | Expose a host port at a guest port with SSH remote forwarding. |
| `wtx unforward [--name NAME] PORT` | Stop a forward or bridge. |
| `wtx stop [NAME]` | Stop a VM and its booted worktree simulator. |
| `wtx rm NAME [--if-exists] [--json] [--with-worktree]` | Delete a VM, optionally with an idempotent receipt or its linked worktree. |
| `wtx prune [--yes]` | Report VMs whose worktrees no longer exist, or delete eligible ones immediately with `--yes`. |
| `wtx image build\|rm\|status` | Inspect, prewarm, or reset the automatically managed shared base VM. |
| `wtx mirror install\|uninstall\|up\|down\|status\|gc` | Manage the bounded registry cache. |
| `wtx which` | Print the VM name for the current worktree. |
| `wtx completions SHELL` | Print completions for bash, zsh, fish, elvish, or PowerShell. |
| `wtx sim up\|status\|rm` | Manage a per-worktree iOS simulator. `wire` and `env` remain as compatibility aliases. |
| `wtx upgrade` | Refresh Homebrew tap metadata and upgrade wtx. |
| `wtx update check [--json]` | Check GitHub Releases for a newer version without installing it. |

## Shell syntax

`wtx exec` passes an argument vector through unchanged and does not interpret pipes,
redirections, or other shell syntax. Wrap those expressions explicitly:

```bash
wtx exec -- bash -c 'command-a | command-b'
```

## Automation

`ensure`, `inspect`, `ls`, `rm`, `env`, and update checks expose JSON where scripts need
stable machine-readable output. The orchestrator readiness contract and receipt schema are
documented in [DESIGN-orchestration.md](DESIGN-orchestration.md).

Automatic orphan cleanup is opportunistic and runs only while VMs are being prepared. It
stops a newly observed orphan, then deletes it after seven days on a later preparation. Set
`WTX_NO_AUTO_PRUNE=1` to disable this behavior; manual `prune` remains available. Once the
recovery window starts, `ls --json` and `inspect --json` expose its Unix timestamp as
`orphaned_since`. Legacy isolated-Git VMs are excluded and reported as
`legacy_isolated_git: true`; inspect their VM-local commits before using explicit `rm`.
