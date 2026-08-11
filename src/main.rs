//! wtx — worktree × コーディングエージェントの並列開発ツール。
//! worktree ごとに独立VM（Lima/vz）+ VM内dockerd を与え、`up --from` で
//! DB（volume）・イメージごと環境を引き継げる。git はホストと rw 共有
//! （VM内コミット＝ホストに直接反映）。設計と検証記録は VERIFICATION.md を参照。
mod launchd;
mod lima;
mod mirror;
mod repo;
mod sim;
mod sshx;
mod tui;
mod util;

use anyhow::{anyhow, Result};
use clap::{CommandFactory, Parser, Subcommand};
use std::path::Path;

#[derive(Parser)]
#[command(
    name = "wtx",
    version,
    about = "Per-worktree VMs with in-VM dockerd and a built-in registry cache"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

/// `wtx up` / `wtx new` 共通のVM作成フラグ。
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
    /// Do not mount the host ~/.claude into the VM
    #[arg(long)]
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
            no_claude: self.no_claude,
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
        name: String,
        #[arg(short = 'w', long)]
        workdir: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
    /// Open an interactive shell inside a VM
    Shell { name: String },
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
    /// Publish a VM port on the host (ssh -L) HOST:GUEST
    Forward { name: String, spec: String },
    /// Expose a host port inside the VM (ssh -R) GUEST:HOST
    Bridge { name: String, spec: String },
    /// Tear down a forward/bridge
    Unforward { name: String, port: String },
    /// Stop a VM
    Stop { name: String },
    /// Delete a VM (its databases and images go with it; commits are already on the host)
    Rm {
        name: String,
        /// Also remove the linked git worktree the VM was created from
        #[arg(long)]
        with_worktree: bool,
    },
    /// Delete VMs whose worktree no longer exists
    Prune {
        /// Actually delete them (without this, only report what would be deleted)
        #[arg(long)]
        yes: bool,
    },
    /// Pre-provisioned golden VM (build|rm|status); wtx up clones it for fast startup
    Image {
        #[arg(default_value = "status")]
        action: String,
    },
    /// Pull-through registry cache (up|down|status|install|uninstall|serve)
    Mirror {
        #[arg(default_value = "status")]
        action: String,
    },
    /// Print the VM name for the current worktree (composable: wtx exec "$(wtx which)" ...)
    Which,
    /// Print shell completions (bash|zsh|fish|elvish|powershell)
    Completions { shell: clap_complete::Shell },
    /// Per-worktree iOS simulator: device lifecycle, port wiring, agent env vars
    Sim {
        #[command(subcommand)]
        action: Option<SimCmd>,
    },
}

/// NAME はすべて省略可能で、省略時はカレントディレクトリの worktree から解決する。
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
    /// Allocate a host port for a guest port and start the forward (idempotent): LABEL:GUESTPORT
    Wire { spec: String, name: Option<String> },
    /// Print eval-able env (WTX_SIM_UDID, WTX_PORT_*...); re-arms dead forwards
    Env {
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Delete the simulator device (the VM stays)
    Rm { name: Option<String> },
}

fn main() {
    // `wtx ls | head` のようにパイプ先が先に閉じたとき、Rust 既定の SIGPIPE 無視のままだと
    // 標準出力への書き込みが panic するので、通常の Unix コマンドと同じ挙動に戻す。
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
            lima::up(&name, &workdir, flags.into_opts(mounts))
        }
        Some(Cmd::New { branch, dir, flags }) => {
            lima::new(&branch, dir.as_deref(), flags.into_opts(vec![]))
        }
        Some(Cmd::Exec { name, workdir, cmd }) => sshx::exec(&name, workdir.as_deref(), &cmd),
        Some(Cmd::Shell { name }) => sshx::shell(&name),
        Some(Cmd::Ls { json }) => {
            if json {
                lima::ls_json()
            } else {
                lima::ls();
                Ok(())
            }
        }
        Some(Cmd::Forward { name, spec }) => sshx::forward(&name, &spec, false),
        Some(Cmd::Bridge { name, spec }) => sshx::forward(&name, &spec, true),
        Some(Cmd::Unforward { name, port }) => sshx::unforward(&name, &port),
        Some(Cmd::Stop { name }) => util::limactl(&["stop", &name]),
        Some(Cmd::Rm {
            name,
            with_worktree,
        }) => lima::rm(&name, with_worktree),
        Some(Cmd::Prune { yes }) => lima::prune(yes),
        Some(Cmd::Image { action }) => match action.as_str() {
            "build" => lima::image_build(),
            "rm" => lima::image_rm(),
            _ => {
                lima::image_status();
                Ok(())
            }
        },
        Some(Cmd::Which) => sim::which(),
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
                SimCmd::Wire { spec, name } => sim::wire(name.as_deref(), &spec),
                SimCmd::Env { name, json } => sim::env(name.as_deref(), json),
                SimCmd::Rm { name } => sim::rm(name.as_deref()),
            }
        }
        Some(Cmd::Mirror { action }) => match action.as_str() {
            "serve" => mirror::serve(),
            "up" => mirror::up(),
            "down" => mirror::down(),
            "install" => launchd::install(),
            "uninstall" => launchd::uninstall(),
            _ => {
                mirror::status();
                Ok(())
            }
        },
    }
}

/// `wtx up` の NAME/DIR 省略時の解決。
/// - NAME だけ: 既存VMなら記録済み workdir へ再アタッチ、無ければカレントディレクトリに新規作成。
///   ただし `/` を含む（またはVM未登録の既存ディレクトリを指す）1引数は DIR とみなす。
/// - 両方省略: `wtx which` と同じ規則（メタデータ workdir の最長前方一致）で解決し、
///   どのVMにも覆われていなければカレントディレクトリから名前を導出して新規作成する。
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
            let hits = sim::covering_cwd()?;
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
