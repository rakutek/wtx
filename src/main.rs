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
#[command(name = "wtx", version, about = "worktreeごとの隔離VM + VM内dockerd + 内蔵レジストリキャッシュ")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// VMを作成・起動する（worktreeは自動判別され、隔離gitが適用される）
    Up {
        name: String,
        workdir: String,
        /// 追加マウント（:ro で読み取り専用）
        mounts: Vec<String>,
        #[arg(long, default_value = "4GiB")]
        memory: String,
        #[arg(long, default_value_t = 2)]
        cpus: u32,
        #[arg(long, default_value = "20GiB")]
        disk: String,
        /// 隔離gitを無効化し、ホストの.gitをrw共有する（旧方式）
        #[arg(long)]
        share_git: bool,
        /// Claude資格情報をコピーしない
        #[arg(long)]
        no_claude: bool,
        /// ゴールデンVMのcloneを使わず新規プロビジョニングする
        #[arg(long)]
        no_clone: bool,
    },
    /// VM内でコマンドを実行する（終了コードは素通し。シェル構文は bash -c '...'）
    Exec {
        name: String,
        #[arg(short = 'w', long)]
        workdir: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
    /// VM内の対話シェルを開く
    Shell { name: String },
    /// VM一覧
    Ls,
    /// ratatui コンソールを開く
    Tui {
        /// tty を使わず1フレームだけ描画して終了する（動作確認用）
        #[arg(long)]
        snapshot: bool,
    },
    /// VM内のコミットを refs/wtx/<name>/* としてホストへ回収する
    Sync { name: String },
    /// VMのポートをホストに公開する (ssh -L) HOST:GUEST
    Forward { name: String, spec: String },
    /// ホストのポートをVM内に露出する (ssh -R) GUEST:HOST
    Bridge { name: String, spec: String },
    /// forward/bridge を解除する
    Unforward { name: String, port: String },
    /// VMを停止する
    Stop { name: String },
    /// VMを削除する（DB・イメージごと消える）
    Rm { name: String },
    /// プロビジョニング済みゴールデンVM（cloneで高速起動）
    Image {
        #[arg(default_value = "status")]
        action: String,
    },
    /// pull-throughレジストリキャッシュ
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
