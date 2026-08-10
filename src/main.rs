//! wtx — worktree × コーディングエージェントの並列開発ツール。
//! worktree ごとに独立VM（Lima/vz）+ VM内dockerd を与え、`up --from` で
//! DB（volume）・イメージごと環境を引き継げる。git はホストと rw 共有
//! （VM内コミット＝ホストに直接反映）。設計と検証記録は VERIFICATION.md を参照。
mod launchd;
mod lima;
mod mirror;
mod repo;
mod sshx;
mod tui;
mod util;

use anyhow::Result;
use clap::{Parser, Subcommand};

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

#[derive(Subcommand)]
enum Cmd {
    /// Create and start a VM (the host git is shared: commits in the VM land on the host)
    Up {
        name: String,
        workdir: String,
        /// Extra mounts (append :ro for read-only)
        mounts: Vec<String>,
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
    Ls,
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
        Some(Cmd::Up { name, workdir, mounts, memory, cpus, disk, from, no_claude, no_clone }) => {
            lima::up(
                &name,
                &workdir,
                lima::UpOpts { memory, cpus, disk, from, no_claude, no_clone, extra_mounts: mounts },
            )
        }
        Some(Cmd::Exec { name, workdir, cmd }) => sshx::exec(&name, workdir.as_deref(), &cmd),
        Some(Cmd::Shell { name }) => sshx::shell(&name),
        Some(Cmd::Ls) => {
            lima::ls();
            Ok(())
        }
        Some(Cmd::Forward { name, spec }) => sshx::forward(&name, &spec, false),
        Some(Cmd::Bridge { name, spec }) => sshx::forward(&name, &spec, true),
        Some(Cmd::Unforward { name, port }) => sshx::unforward(&name, &port),
        Some(Cmd::Stop { name }) => util::limactl(&["stop", &name]),
        Some(Cmd::Rm { name, with_worktree }) => lima::rm(&name, with_worktree),
        Some(Cmd::Prune { yes }) => lima::prune(yes),
        Some(Cmd::Image { action }) => match action.as_str() {
            "build" => lima::image_build(),
            "rm" => lima::image_rm(),
            _ => {
                lima::image_status();
                Ok(())
            }
        },
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
