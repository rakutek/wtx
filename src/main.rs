//! wtx is a parallel development tool for coding agents using git worktrees.
//! It gives each worktree an isolated VM (Lima/vz) with its own `dockerd`, while
//! `up --from` can carry over the environment, including database volumes and images.
//! Git is mounted read-write from the host, so commits made in the VM appear directly
//! on the host. See VERIFICATION.md for the design and verification record.
mod context;
mod launchd;
mod lima;
mod mirror;
mod port;
mod repo;
mod sim;
mod sshx;
mod tui;
mod update;
mod util;

use anyhow::{anyhow, Result};
use clap::{CommandFactory, Parser, Subcommand};
use std::path::Path;

#[derive(Parser)]
#[command(
    name = "wtx",
    version,
    about = "wtx: per-worktree VMs with in-VM dockerd and a built-in registry cache"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

/// VM creation flags shared by `wtx up` and `wtx new`.
#[derive(clap::Args)]
struct UpFlags {
    /// Memory (default 4GiB; cloned VMs inherit their source unless set)
    #[arg(long)]
    memory: Option<String>,
    /// CPUs (default 2; cloned VMs inherit their source unless set)
    #[arg(long)]
    cpus: Option<u32>,
    /// Disk size for freshly provisioned VMs (cloned VMs keep their source's disk)
    #[arg(long, default_value = "20GiB")]
    disk: String,
    /// Seed from an existing VM: clone its disk so docker volumes (DB data),
    /// images and installed tools carry over (the source is stopped briefly)
    #[arg(long, conflicts_with = "no_clone")]
    from: Option<String>,
    /// Explicitly share ~/.claude and the host ssh-agent with the VM (trusted agents only)
    #[arg(long)]
    agent_access: bool,
    /// Deprecated compatibility flag; credentials are no longer shared by default
    #[arg(long, hide = true, conflicts_with = "agent_access")]
    no_claude: bool,
    /// Provision from scratch instead of cloning the golden VM
    #[arg(long)]
    no_clone: bool,
    /// Also create a per-worktree iOS simulator device (see `wtx sim`)
    #[arg(long)]
    sim: bool,
    /// Device type for --sim, e.g. "iPhone 16 Pro" (implies --sim)
    #[arg(long)]
    sim_device: Option<String>,
}

impl UpFlags {
    fn into_opts(self, extra_mounts: Vec<String>) -> lima::UpOpts {
        lima::UpOpts {
            memory: self.memory,
            cpus: self.cpus,
            disk: self.disk,
            from: self.from,
            agent_access: self.agent_access && !self.no_claude,
            no_clone: self.no_clone,
            extra_mounts,
            sim: self.sim,
            sim_device: self.sim_device,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Create and start a VM (the host git is shared: commits in the VM land on the host)
    Up {
        /// VM name, or a directory (a single argument containing `/` counts as DIR).
        /// Omit both to resolve from the current directory
        name: Option<String>,
        /// Worktree directory (default: the VM's recorded workdir, else the current directory)
        workdir: Option<String>,
        /// Extra mounts (append :ro for read-only)
        mounts: Vec<String>,
        #[command(flatten)]
        flags: UpFlags,
    },
    /// Idempotently create or start a VM and wait until dockerd is ready
    Ensure {
        /// VM name, or a directory (a single argument containing `/` counts as DIR).
        /// Omit both to resolve from the current directory
        name: Option<String>,
        /// Worktree directory (default: the VM's recorded workdir, else the current directory)
        workdir: Option<String>,
        /// Extra mounts used only when the VM is first created (append :ro for read-only)
        mounts: Vec<String>,
        #[command(flatten)]
        flags: UpFlags,
        /// Seconds to wait for dockerd after the VM starts
        #[arg(long, default_value_t = 120)]
        timeout_seconds: u64,
        /// Machine-readable versioned receipt
        #[arg(long)]
        json: bool,
    },
    /// Inspect one wtx VM, its worktree, runtime readiness, ports, and simulator
    Inspect {
        /// VM name (omit inside a worktree covered by a wtx VM)
        name: Option<String>,
        /// Machine-readable versioned receipt
        #[arg(long)]
        json: bool,
    },
    /// Create a git worktree and its VM in one step (the branch is created if missing)
    New {
        /// Branch to check out in the new worktree
        branch: String,
        /// Worktree directory (default: sibling of the main repo, <repo>-<branch>)
        #[arg(long)]
        dir: Option<String>,
        #[command(flatten)]
        flags: UpFlags,
    },
    /// Run a command inside a VM (exit code is passed through; use bash -c '...' for shell syntax)
    Exec {
        /// VM name. Omit inside a worktree covered by a wtx VM
        #[arg(short = 'n', long)]
        name: Option<String>,
        #[arg(short = 'w', long)]
        workdir: Option<String>,
        /// Allocate a remote PTY for interactive agent CLIs
        #[arg(short = 't', long)]
        tty: bool,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
    /// Open an interactive shell inside a VM
    Shell { name: Option<String> },
    /// List VMs
    Ls {
        /// Machine-readable output for scripts and agents
        #[arg(long)]
        json: bool,
    },
    /// Open the ratatui console
    Tui {
        /// Render a single frame without a tty and exit (for smoke tests)
        #[arg(long)]
        snapshot: bool,
    },
    /// Allocate and manage named host ports for VM services
    Port {
        #[command(subcommand)]
        action: PortCmd,
    },
    /// Print eval-able VM and port environment variables; re-arm recorded forwards
    Env {
        /// VM name. Omit inside a worktree covered by a wtx VM
        name: Option<String>,
        /// Machine-readable output for scripts and agents
        #[arg(long)]
        json: bool,
    },
    /// Publish a VM port on the host (ssh -L). SPEC is HOST:GUEST
    Forward {
        /// VM name. Omit inside a worktree covered by a wtx VM
        #[arg(short = 'n', long)]
        name: Option<String>,
        /// HOST:GUEST, or legacy NAME HOST:GUEST
        #[arg(required = true, num_args = 1..=2)]
        args: Vec<String>,
    },
    /// Expose a host port inside the VM (ssh -R). SPEC is also HOST:GUEST
    Bridge {
        /// VM name. Omit inside a worktree covered by a wtx VM
        #[arg(short = 'n', long)]
        name: Option<String>,
        /// HOST:GUEST, or legacy NAME HOST:GUEST
        #[arg(required = true, num_args = 1..=2)]
        args: Vec<String>,
    },
    /// Tear down a forward/bridge
    Unforward {
        /// VM name. Omit inside a worktree covered by a wtx VM
        #[arg(short = 'n', long)]
        name: Option<String>,
        /// Bound port, or legacy NAME PORT
        #[arg(required = true, num_args = 1..=2)]
        args: Vec<String>,
    },
    /// Stop a VM
    Stop { name: Option<String> },
    /// Delete a VM (its databases and images go with it; commits are already on the host)
    Rm {
        name: String,
        /// Also remove the linked git worktree the VM was created from
        #[arg(long)]
        with_worktree: bool,
        /// Succeed when the VM is already absent
        #[arg(long)]
        if_exists: bool,
        /// Machine-readable versioned receipt
        #[arg(long)]
        json: bool,
    },
    /// Delete VMs whose worktree no longer exists
    Prune {
        /// Actually delete them (without this, only report what would be deleted)
        #[arg(long)]
        yes: bool,
    },
    /// Inspect or manually manage the automatically prepared shared base VM
    Image {
        #[command(subcommand)]
        action: Option<ImageCmd>,
    },
    /// Manage the pull-through registry cache
    Mirror {
        #[command(subcommand)]
        action: Option<MirrorCmd>,
    },
    /// Print the VM name for the current worktree
    Which,
    /// Print shell completions (bash|zsh|fish|elvish|powershell)
    Completions { shell: clap_complete::Shell },
    /// Per-worktree iOS simulator lifecycle
    Sim {
        #[command(subcommand)]
        action: Option<SimCmd>,
    },
    /// Refresh Homebrew metadata and upgrade wtx
    Upgrade,
    /// Check GitHub Releases for a newer wtx version
    Update {
        #[command(subcommand)]
        action: UpdateCmd,
    },
}

#[derive(Subcommand)]
enum PortCmd {
    /// Allocate a host port for LABEL:GUESTPORT and start the forward (idempotent)
    Add {
        /// Service label and port inside the VM, e.g. api:3000
        spec: String,
        /// VM name. Omit inside a worktree covered by a wtx VM
        #[arg(short = 'n', long)]
        name: Option<String>,
    },
}

/// NAME is optional throughout and is resolved from the current worktree when omitted.
#[derive(Subcommand)]
enum SimCmd {
    /// Create the worktree's simulator device (idempotent; also heals a deleted device)
    Up {
        name: Option<String>,
        /// Device type, e.g. "iPhone 16 Pro" (default: newest iPhone of the newest runtime)
        #[arg(long)]
        device: Option<String>,
    },
    /// Show device state and forward liveness
    Status {
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Compatibility alias for `wtx port add LABEL:GUESTPORT`
    Wire { spec: String, name: Option<String> },
    /// Compatibility alias for `wtx env`
    Env {
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Delete the simulator device (the VM stays)
    Rm { name: Option<String> },
}

#[derive(Subcommand)]
enum UpdateCmd {
    /// Check for a newer release (never installs it)
    Check {
        /// Machine-readable versioned result
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ImageCmd {
    /// Prepare the shared base VM before it is first needed
    Build,
    /// Delete the shared base VM
    Rm,
    /// Show shared base VM status
    Status,
}

#[derive(Subcommand)]
enum MirrorCmd {
    /// Start the mirror in the background
    Up,
    /// Stop a manually started mirror
    Down,
    /// Show mirror and cache status
    Status,
    /// Install launchd socket activation
    Install,
    /// Remove launchd socket activation
    Uninstall,
    /// Run cache GC now; optionally persist a new size limit
    Gc {
        /// Cache limit in GiB
        #[arg(long)]
        max_gib: Option<u64>,
    },
    /// Serve registry requests (internal launchd entrypoint)
    #[command(hide = true)]
    Serve,
}

fn main() {
    // Restore standard Unix behavior so writes to a closed pipe, such as in
    // `wtx ls | head`, terminate via SIGPIPE instead of panicking on stdout.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    if let Err(e) = run() {
        eprintln!("wtx: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        None => tui::run(),
        Some(Cmd::Tui { snapshot }) => {
            if snapshot {
                tui::snapshot()
            } else {
                tui::run()
            }
        }
        Some(Cmd::Up {
            name,
            workdir,
            mounts,
            flags,
        }) => {
            let (name, workdir) = up_target(name, workdir)?;
            let opts = flags.into_opts(mounts);
            lima::up(&name, &workdir, &opts)
        }
        Some(Cmd::Ensure {
            name,
            workdir,
            mounts,
            flags,
            timeout_seconds,
            json,
        }) => {
            let (name, workdir) = up_target(name, workdir)?;
            lima::ensure(
                &name,
                &workdir,
                flags.into_opts(mounts),
                timeout_seconds,
                json,
            )
        }
        Some(Cmd::Inspect { name, json }) => lima::inspect(name.as_deref(), json),
        Some(Cmd::New { branch, dir, flags }) => {
            let opts = flags.into_opts(vec![]);
            lima::new(&branch, dir.as_deref(), &opts)
        }
        Some(Cmd::Exec {
            name,
            workdir,
            tty,
            mut cmd,
        }) => {
            let (name, meta) = exec_target(name.as_deref(), &mut cmd)?;
            let workdir = default_guest_workdir(workdir, &meta);
            sshx::exec(&name, Some(&workdir), &cmd, tty)
        }
        Some(Cmd::Shell { name }) => {
            let (name, _) = context::resolve(name.as_deref())?;
            sshx::shell(&name)
        }
        Some(Cmd::Ls { json }) => {
            if json {
                lima::ls_json()
            } else {
                lima::ls();
                Ok(())
            }
        }
        Some(Cmd::Port { action }) => match action {
            PortCmd::Add { spec, name } => port::add(name.as_deref(), &spec),
        },
        Some(Cmd::Env { name, json }) => port::env(name.as_deref(), json),
        Some(Cmd::Forward { name, args }) => {
            let (name, spec) = named_value(name.as_deref(), &args)?;
            sshx::forward(&name, &spec, false)
        }
        Some(Cmd::Bridge { name, args }) => {
            let (name, spec) = named_value(name.as_deref(), &args)?;
            sshx::forward(&name, &spec, true)
        }
        Some(Cmd::Unforward { name, args }) => {
            let (name, port) = named_value(name.as_deref(), &args)?;
            sshx::unforward(&name, &port)
        }
        Some(Cmd::Stop { name }) => {
            let (name, _) = context::resolve(name.as_deref())?;
            lima::stop(&name, false).map_err(anyhow::Error::msg)
        }
        Some(Cmd::Rm {
            name,
            with_worktree,
            if_exists,
            json,
        }) => lima::rm(
            &name,
            lima::RemoveOpts {
                with_worktree,
                if_exists,
                json,
            },
        ),
        Some(Cmd::Prune { yes }) => {
            lima::prune(yes);
            Ok(())
        }
        Some(Cmd::Image { action }) => match action.unwrap_or(ImageCmd::Status) {
            ImageCmd::Build => lima::image_build(),
            ImageCmd::Rm => lima::image_rm(),
            ImageCmd::Status => {
                lima::image_status();
                Ok(())
            }
        },
        Some(Cmd::Which) => context::which(),
        Some(Cmd::Completions { shell }) => {
            clap_complete::generate(shell, &mut Cli::command(), "wtx", &mut std::io::stdout());
            Ok(())
        }
        Some(Cmd::Sim { action }) => {
            match action.unwrap_or(SimCmd::Status {
                name: None,
                json: false,
            }) {
                SimCmd::Up { name, device } => sim::up(name.as_deref(), device.as_deref()),
                SimCmd::Status { name, json } => sim::status(name.as_deref(), json),
                SimCmd::Wire { spec, name } => port::add(name.as_deref(), &spec),
                SimCmd::Env { name, json } => port::env(name.as_deref(), json),
                SimCmd::Rm { name } => sim::rm(name.as_deref()),
            }
        }
        Some(Cmd::Mirror { action }) => match action.unwrap_or(MirrorCmd::Status) {
            MirrorCmd::Serve => mirror::serve(),
            MirrorCmd::Up => mirror::up(),
            MirrorCmd::Down => mirror::down(),
            MirrorCmd::Install => launchd::install(),
            MirrorCmd::Uninstall => launchd::uninstall(),
            MirrorCmd::Gc { max_gib } => mirror::gc(max_gib),
            MirrorCmd::Status => {
                mirror::status();
                Ok(())
            }
        },
        Some(Cmd::Upgrade) => update::upgrade(),
        Some(Cmd::Update { action }) => match action {
            UpdateCmd::Check { json } => update::check_and_print(json),
        },
    }
}

/// `exec` prioritizes cwd-based resolution. The legacy `wtx exec NAME CMD...` form is
/// recognized only when its first argument names an existing wtx VM. Use `--name` to
/// disambiguate.
fn exec_target(
    explicit: Option<&str>,
    cmd: &mut Vec<String>,
) -> Result<(String, lima::InstanceMeta)> {
    if let Some(name) = explicit {
        return context::resolve(Some(name));
    }
    if cmd.len() >= 2 && lima::load_meta(&cmd[0]).is_some() {
        let name = cmd.remove(0);
        return context::resolve(Some(&name));
    }
    context::resolve(None)
}

fn default_guest_workdir(requested: Option<String>, meta: &lima::InstanceMeta) -> String {
    if let Some(path) = requested {
        return path;
    }
    std::env::current_dir()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok())
        .filter(|p| {
            let wd = Path::new(&meta.workdir);
            *p == wd || p.starts_with(wd)
        })
        .map_or_else(
            || meta.workdir.clone(),
            |p| p.to_string_lossy().into_owned(),
        )
}

/// Resolve an omitted NAME from the cwd, or accept the legacy `NAME VALUE` two-argument form.
fn named_value(explicit: Option<&str>, args: &[String]) -> Result<(String, String)> {
    match (explicit, args) {
        (Some(name), [value]) => {
            let (name, _) = context::resolve(Some(name))?;
            Ok((name, value.clone()))
        }
        (None, [value]) => {
            let (name, _) = context::resolve(None)?;
            Ok((name, value.clone()))
        }
        (None, [name, value]) => {
            let (name, _) = context::resolve(Some(name))?;
            Ok((name, value.clone()))
        }
        (Some(_), _) => Err(anyhow!("pass one value when --name is used")),
        _ => Err(anyhow!("expected VALUE or legacy NAME VALUE")),
    }
}

/// Resolve omitted NAME and DIR arguments for `wtx up`.
/// - NAME only: reattach an existing VM to its recorded workdir, or create a VM for the cwd.
///   A single argument containing `/`, or naming an existing directory not registered to a
///   VM, is treated as DIR instead.
/// - Both omitted: use the same longest workdir-prefix rule as `wtx which`. If no VM covers
///   the cwd, derive a name from the current directory and create a new VM.
fn up_target(name: Option<String>, workdir: Option<String>) -> Result<(String, String)> {
    if let (Some(n), None) = (&name, &workdir) {
        let looks_like_dir =
            n.contains('/') || (Path::new(n).is_dir() && lima::load_meta(n).is_none());
        if looks_like_dir {
            let dir = std::fs::canonicalize(n)?;
            let vm = lima::derive_name(&dir)?;
            lima::ensure_name_matches(&vm, &dir)?;
            return Ok((vm, dir.to_string_lossy().into_owned()));
        }
    }
    match (name, workdir) {
        (Some(n), Some(w)) => Ok((n, w)),
        (Some(n), None) => {
            let w = lima::load_meta(&n)
                .map(|m| m.workdir)
                .filter(|w| !w.is_empty())
                .unwrap_or_else(|| ".".to_string());
            Ok((n, w))
        }
        (None, _) => {
            let hits = context::covering_cwd()?;
            match hits.len() {
                1 => {
                    let (n, m) = hits.into_iter().next().unwrap();
                    Ok((n, m.workdir))
                }
                0 => {
                    let dir = std::fs::canonicalize(std::env::current_dir()?)?;
                    let vm = lima::derive_name(&dir)?;
                    lima::ensure_name_matches(&vm, &dir)?;
                    Ok((vm, dir.to_string_lossy().into_owned()))
                }
                _ => Err(anyhow!(
                    "multiple VMs cover this directory: {} (pass NAME)",
                    hits.iter()
                        .map(|(n, _)| n.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_tty_is_parsed_without_becoming_remote_argv() {
        let cli =
            Cli::try_parse_from(["wtx", "exec", "--tty", "--name", "vm-a", "--", "codex"]).unwrap();
        match cli.cmd.unwrap() {
            Cmd::Exec { name, tty, cmd, .. } => {
                assert_eq!(name.as_deref(), Some("vm-a"));
                assert!(tty);
                assert_eq!(cmd, ["codex"]);
            }
            _ => panic!("expected exec"),
        }
    }

    #[test]
    fn exec_accepts_cwd_resolved_command_without_name() {
        let cli =
            Cli::try_parse_from(["wtx", "exec", "--", "docker", "compose", "up", "-d"]).unwrap();
        match cli.cmd.unwrap() {
            Cmd::Exec { name, cmd, .. } => {
                assert!(name.is_none());
                assert_eq!(cmd, ["docker", "compose", "up", "-d"]);
            }
            _ => panic!("expected exec"),
        }
    }

    #[test]
    fn image_and_mirror_reject_unknown_actions() {
        assert!(Cli::try_parse_from(["wtx", "image", "bulid"]).is_err());
        assert!(Cli::try_parse_from(["wtx", "mirror", "statsu"]).is_err());
    }

    #[test]
    fn bridge_uses_one_cwd_resolved_host_guest_value() {
        let cli = Cli::try_parse_from(["wtx", "bridge", "5432:5432"]).unwrap();
        match cli.cmd.unwrap() {
            Cmd::Bridge { name, args } => {
                assert!(name.is_none());
                assert_eq!(args, ["5432:5432"]);
            }
            _ => panic!("expected bridge"),
        }
    }

    #[test]
    fn named_port_and_environment_commands_are_top_level() {
        let cli =
            Cli::try_parse_from(["wtx", "port", "add", "web-api:3000", "--name", "vm-a"]).unwrap();
        match cli.cmd.unwrap() {
            Cmd::Port {
                action: PortCmd::Add { spec, name },
            } => {
                assert_eq!(spec, "web-api:3000");
                assert_eq!(name.as_deref(), Some("vm-a"));
            }
            _ => panic!("expected port add"),
        }

        let cli = Cli::try_parse_from(["wtx", "env", "vm-a", "--json"]).unwrap();
        match cli.cmd.unwrap() {
            Cmd::Env { name, json } => {
                assert_eq!(name.as_deref(), Some("vm-a"));
                assert!(json);
            }
            _ => panic!("expected env"),
        }
    }

    #[test]
    fn simulator_port_commands_remain_compatible_aliases() {
        assert!(Cli::try_parse_from(["wtx", "sim", "wire", "api:3000", "vm-a"]).is_ok());
        assert!(Cli::try_parse_from(["wtx", "sim", "env", "vm-a", "--json"]).is_ok());
    }

    #[test]
    fn ensure_machine_contract_is_parsed() {
        let cli =
            Cli::try_parse_from(["wtx", "ensure", "vm-a", "/tmp/worktree-a", "--json"]).unwrap();
        match cli.cmd.unwrap() {
            Cmd::Ensure {
                name,
                workdir,
                json,
                ..
            } => {
                assert_eq!(name.as_deref(), Some("vm-a"));
                assert_eq!(workdir.as_deref(), Some("/tmp/worktree-a"));
                assert!(json);
            }
            _ => panic!("expected ensure"),
        }
    }

    #[test]
    fn idempotent_remove_contract_is_parsed() {
        let cli = Cli::try_parse_from(["wtx", "rm", "vm-a", "--if-exists", "--json"]).unwrap();
        match cli.cmd.unwrap() {
            Cmd::Rm {
                name,
                with_worktree,
                if_exists,
                json,
            } => {
                assert_eq!(name, "vm-a");
                assert!(!with_worktree);
                assert!(if_exists);
                assert!(json);
            }
            _ => panic!("expected rm"),
        }
    }

    #[test]
    fn update_check_machine_contract_is_parsed() {
        let cli = Cli::try_parse_from(["wtx", "update", "check", "--json"]).unwrap();
        match cli.cmd.unwrap() {
            Cmd::Update {
                action: UpdateCmd::Check { json },
            } => assert!(json),
            _ => panic!("expected update check"),
        }
    }

    #[test]
    fn upgrade_is_a_top_level_command() {
        let cli = Cli::try_parse_from(["wtx", "upgrade"]).unwrap();
        assert!(matches!(cli.cmd, Some(Cmd::Upgrade)));
    }
}
