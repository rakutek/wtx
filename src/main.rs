//! wtx — git worktree ごとの隔離VM（Lima/vz）+ VM内dockerd + 内蔵レジストリキャッシュ。
//! Docker Sandboxes のOSS代替。設計と検証記録は ../myapp/VERIFICATION.md を参照。
mod creds;
mod gitiso;
mod launchd;
mod lima;
mod mirror;
mod sshx;
mod tui;
mod util;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "wtx",
    version,
    about = "Per-worktree isolated VMs with in-VM dockerd and a built-in registry cache"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create and start a VM (worktrees are detected automatically and get isolated git)
    Up {
        name: String,
        workdir: String,
        /// Extra mounts (append :ro for read-only)
        mounts: Vec<String>,
        #[arg(long, default_value = "4GiB")]
        memory: String,
        #[arg(long, default_value_t = 2)]
        cpus: u32,
        #[arg(long, default_value = "20GiB")]
        disk: String,
        /// Disable isolated git and share the host .git read-write (legacy mode)
        #[arg(long)]
        share_git: bool,
        /// Do not copy Claude credentials into the VM
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
    /// Fetch commits made inside the VM to the host as refs/wtx/<name>/*
    Sync { name: String },
    /// Publish a VM port on the host (ssh -L) HOST:GUEST
    Forward { name: String, spec: String },
    /// Expose a host port inside the VM (ssh -R) GUEST:HOST
    Bridge { name: String, spec: String },
    /// Tear down a forward/bridge
    Unforward { name: String, port: String },
    /// Stop a VM
    Stop { name: String },
    /// Delete a VM (its databases and images go with it)
    Rm { name: String },
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
        Some(Cmd::Up { name, workdir, mounts, memory, cpus, disk, share_git, no_claude, no_clone }) => {
            lima::up(
                &name,
                &workdir,
                lima::UpOpts { memory, cpus, disk, share_git, no_claude, no_clone, extra_mounts: mounts },
            )
        }
        Some(Cmd::Exec { name, workdir, cmd }) => sshx::exec(&name, workdir.as_deref(), &cmd),
        Some(Cmd::Shell { name }) => sshx::shell(&name),
        Some(Cmd::Ls) => util::limactl(&["list"]),
        Some(Cmd::Sync { name }) => lima::sync(&name),
        Some(Cmd::Forward { name, spec }) => sshx::forward(&name, &spec, false),
        Some(Cmd::Bridge { name, spec }) => sshx::forward(&name, &spec, true),
        Some(Cmd::Unforward { name, port }) => sshx::unforward(&name, &port),
        Some(Cmd::Stop { name }) => util::limactl(&["stop", &name]),
        Some(Cmd::Rm { name }) => lima::rm(&name),
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
