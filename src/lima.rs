use crate::gitiso::{self, RepoKind};
use crate::mirror;
use crate::util::*;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const GOLDEN: &str = "wtx-golden";
const TEMPLATE: &str = include_str!("../templates/vm.yaml.tmpl");

#[derive(Debug, Clone)]
pub struct Mount {
    pub location: String,
    pub writable: bool,
}

/// wtx up 時の判断を記録し、sync / rm / TUI が参照する。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InstanceMeta {
    pub workdir: String,
    #[serde(default)]
    pub main_repo: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub isolated: bool,
    #[serde(default)]
    pub keep_refs: bool,
    /// `wtx up --from` の clone 元（来歴。空なら通常作成）
    #[serde(default)]
    pub seeded_from: String,
}

pub fn meta_path(name: &str) -> PathBuf {
    wtx_home().join(format!("{name}.json"))
}

pub fn load_meta(name: &str) -> Option<InstanceMeta> {
    serde_json::from_str(&std::fs::read_to_string(meta_path(name)).ok()?).ok()
}

pub struct UpOpts {
    pub memory: Option<String>,
    pub cpus: Option<u32>,
    pub disk: String,
    pub from: Option<String>,
    pub share_git: bool,
    pub no_claude: bool,
    pub no_clone: bool,
    pub extra_mounts: Vec<String>,
}

fn render_yaml(mounts: &[Mount], cpus: u32, memory: &str, disk: &str, path: &Path) -> Result<()> {
    let m: String = mounts
        .iter()
        .map(|m| format!("- location: \"{}\"\n  writable: {}\n", m.location, m.writable))
        .collect();
    let yaml = TEMPLATE
        .replace("__CPUS__", &cpus.to_string())
        .replace("__MEMORY__", memory)
        .replace("__DISK__", disk)
        .replace("__MOUNTS__", m.trim_end())
        .replace("__MIRROR_PORT__", &mirror::mirror_port().to_string())
        .replace("__GIT_NAME__", &git_config_global("user.name", "wtx"))
        .replace("__GIT_EMAIL__", &git_config_global("user.email", "wtx@localhost"));
    std::fs::write(path, yaml)?;
    Ok(())
}

pub fn golden_usable() -> bool {
    lima_dir(GOLDEN).join("lima.yaml").exists() && lima_status(GOLDEN) == "Stopped"
}

pub fn image_build() -> Result<()> {
    if lima_dir(GOLDEN).exists() {
        return Err(anyhow!("{GOLDEN} already exists (run `wtx image rm` to rebuild it)"));
    }
    let yaml = wtx_home().join(format!("{GOLDEN}.yaml"));
    render_yaml(&[], 2, "4GiB", "20GiB", &yaml)?;
    println!("Building the golden VM (one-time, 3-4 min)...");
    limactl(&["start", "--name", GOLDEN, "--tty=false", &yaml.to_string_lossy()])?;
    limactl(&["stop", GOLDEN])?; // clone は停止中のインスタンスに対して行う
    println!("Done: `wtx up` now clones {GOLDEN}");
    Ok(())
}

pub fn image_rm() -> Result<()> {
    limactl(&["delete", "-f", GOLDEN])?;
    let _ = std::fs::remove_file(wtx_home().join(format!("{GOLDEN}.yaml")));
    Ok(())
}

pub fn image_status() {
    if golden_usable() {
        println!("{GOLDEN}: ready (wtx up clones it for fast startup)");
    } else {
        let st = lima_status(GOLDEN);
        if st.is_empty() {
            println!("{GOLDEN}: not built - run `wtx image build` to cut VM creation to seconds");
        } else {
            println!("{GOLDEN}: {st} - it must be stopped to be cloned (limactl stop {GOLDEN})");
        }
    }
}

/// 停止中のVMを clone する引数列。clone 後の lima.yaml は解決済み形式なので
/// テンプレートで上書きはできず、マウントは --mount-only で差し替える
/// （ゆえに全マウントはホストと同じ絶対パスに置く）。--memory/--cpus は
/// 明示されたときだけ渡し、省略時は clone 元の値を引き継ぐ。
fn clone_args(src: &str, name: &str, o: &UpOpts, mounts: &[Mount]) -> Vec<String> {
    let mut args: Vec<String> = vec!["clone".into(), src.into(), name.into()];
    if let Some(mem) = &o.memory {
        args.push("--memory".into());
        args.push(mem.trim_end_matches("GiB").to_string());
    }
    if let Some(c) = o.cpus {
        args.push("--cpus".into());
        args.push(c.to_string());
    }
    for m in mounts {
        args.push("--mount-only".into());
        args.push(if m.writable {
            format!("{}:w", m.location)
        } else {
            m.location.clone()
        });
    }
    args
}

/// docker compose の既定プロジェクト名（ディレクトリ名を小文字化し、英数と -_ 以外を除去）。
/// compose はこれを volume 名の接頭辞に使うため、worktree のディレクトリ名が変わると
/// 同じ compose ファイルでも volume 名が変わる。
fn compose_project_name(dir: &Path) -> String {
    let base = dir
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let cleaned: String = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        .collect();
    cleaned.trim_start_matches(|c: char| !c.is_ascii_alphanumeric()).to_string()
}

/// `--from` で clone したVMから clone 元由来の状態を取り除き、compose の volume 名を
/// 新しい worktree のプロジェクト名に付け替える。setup_isolated_git より前に呼ぶこと:
/// clone 元の .wtx-local マーカーが残っていると同一メインリポジトリの worktree で
/// マーカー判定が誤爆し、新VMが clone 元のVMローカル git を使い続けてしまう。
fn seed_cleanup(name: &str, src: &str, workdir: &Path) -> Result<()> {
    let src_meta = load_meta(src);
    let old_gitdir = src_meta
        .as_ref()
        .filter(|m| !m.main_repo.is_empty())
        .map(|m| format!("{}/.git", m.main_repo))
        .unwrap_or_default();
    let src_project = src_meta
        .as_ref()
        .map(|m| compose_project_name(Path::new(&m.workdir)))
        .unwrap_or_default();
    let dst_project = compose_project_name(workdir);

    let script = format!(
        r#"set -eu
OLDGIT={oldgit}
SRC={src}
DST={dst}

# clone 元の隔離git状態を除去する。unit の停止・削除はベストエフォートだが、
# 剥がせない overlay を残したまま進むと git が黙って clone 元の状態を指すので、
# その場合だけは失敗させる。
sudo systemctl disable --now wtx-gitmount.service >/dev/null 2>&1 || true
sudo rm -f /etc/systemd/system/wtx-gitmount.service /usr/local/sbin/wtx-gitmount
sudo systemctl daemon-reload >/dev/null 2>&1 || true
if [ -n "$OLDGIT" ]; then
  n=0
  while [ -e "$OLDGIT/.wtx-local" ] && [ $n -lt 5 ]; do
    sudo umount "$OLDGIT" 2>/dev/null || true
    n=$((n+1))
  done
  if [ -e "$OLDGIT/.wtx-local" ]; then
    echo "wtx: could not remove the stale git overlay at $OLDGIT" >&2
    exit 1
  fi
fi
if mountpoint -q /run/wtx/base.git 2>/dev/null; then sudo umount /run/wtx/base.git || true; fi
sudo rm -rf /var/lib/wtx/git

# docker: コンテナは作り直せばよい（compose が再作成する）。引き継ぐ価値があるのは
# volume（DBデータ）とイメージなので、コンテナと不要ネットワークだけ消す。
n=0
until docker info >/dev/null 2>&1; do
  n=$((n+1))
  if [ $n -ge 120 ]; then echo "wtx: dockerd did not come up in the seeded VM" >&2; exit 1; fi
  sleep 1
done
docker ps -aq | xargs -r docker rm -fv >/dev/null
docker network prune -f >/dev/null 2>&1 || true

# compose の volume は <プロジェクト名>_ が接頭辞。worktree のディレクトリ名が変わると
# 参照名も変わるので、clone 元プロジェクトの volume を新しい名前に付け替える。
# （compose ファイルで name: を固定しているプロジェクトは接頭辞が一致せず、単にスキップされる）
if [ -n "$SRC" ] && [ "$SRC" != "$DST" ]; then
  docker volume ls -q | while IFS= read -r v; do
    case "$v" in
    "$SRC"_*)
      suffix="${{v#"$SRC"_}}"
      nv="${{DST}}_$suffix"
      docker volume create \
        --label "com.docker.compose.project=$DST" \
        --label "com.docker.compose.volume=$suffix" \
        "$nv" >/dev/null
      sudo cp -a "/var/lib/docker/volumes/$v/_data/." "/var/lib/docker/volumes/$nv/_data/"
      docker volume rm "$v" >/dev/null
      echo "wtx: volume $v -> $nv"
      ;;
    esac
  done
fi
"#,
        oldgit = shq(&old_gitdir),
        src = shq(&src_project),
        dst = shq(&dst_project),
    );
    crate::sshx::vm_script(name, &script, None)
}

pub fn up(name: &str, workdir: &str, o: UpOpts) -> Result<()> {
    let workdir = std::fs::canonicalize(workdir)?;
    if !workdir.is_dir() {
        return Err(anyhow!("workdir not found: {}", workdir.display()));
    }
    if !mirror::mirror_alive() {
        eprintln!("wtx: warning: mirror is down - pulls go straight upstream (wtx mirror up)");
    }

    let repo = gitiso::inspect_repo(&workdir)?;
    let isolated = repo.is_some() && !o.share_git;

    let mut mounts = vec![Mount {
        location: workdir.to_string_lossy().into_owned(),
        writable: true,
    }];
    if let Some(r) = &repo {
        if r.kind == RepoKind::Worktree {
            // メインの .git は workdir の外にあるので別マウントする
            // （隔離モードでは ro。VMローカルの .git を bind で被せる）
            mounts.push(Mount {
                location: r.host_git.to_string_lossy().into_owned(),
                writable: !isolated,
            });
        }
    }
    for m in &o.extra_mounts {
        let (loc, w) = match m.strip_suffix(":ro") {
            Some(l) => (l, false),
            None => (m.as_str(), true),
        };
        let abs = std::fs::canonicalize(loc)?.to_string_lossy().into_owned();
        if mounts.iter().any(|x| x.location == abs) {
            eprintln!("wtx: ignoring {abs}: already mounted automatically");
            continue;
        }
        mounts.push(Mount { location: abs, writable: w });
    }

    let yaml = wtx_home().join(format!("{name}.yaml"));
    render_yaml(&mounts, o.cpus.unwrap_or(2), o.memory.as_deref().unwrap_or("4GiB"), &o.disk, &yaml)?;

    let status = lima_status(name);
    if let Some(src) = o.from.as_deref() {
        // 既存VMを clone して DB（volume）・イメージ・導入済みツールごと引き継ぐ。
        // 停止中のディスクを複製するので at-rest の一貫したコピーになる。
        if !status.is_empty() {
            return Err(anyhow!("{name} already exists; --from can only seed a new VM"));
        }
        if src == name {
            return Err(anyhow!("--from {src}: cannot seed a VM from itself"));
        }
        let src_status = lima_status(src);
        if src_status.is_empty() {
            return Err(anyhow!("--from {src}: no such VM"));
        }
        let was_running = src_status == "Running";
        if was_running {
            println!("stopping {src} for a consistent copy (it restarts in the background)...");
            limactl(&["stop", src])?;
        }
        let args = clone_args(src, name, &o, &mounts);
        limactl(&args.iter().map(|s| s.as_str()).collect::<Vec<_>>())?;
        if was_running {
            // 新VMの起動と並行して clone 元を復帰させ、ダウンタイムを clone の間だけにする
            let _ = std::process::Command::new("limactl")
                .args(["start", "--tty=false", src])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
        limactl(&["start", name, "--tty=false"])?;
        seed_cleanup(name, src, &workdir)?;
    } else if !status.is_empty() {
        // 既存インスタンスへの再アタッチ（マウント構成は作成時のもの）
        if status != "Running" {
            limactl(&["start", name, "--tty=false"])?;
        }
    } else if !o.no_clone && golden_usable() {
        let args = clone_args(GOLDEN, name, &o, &mounts);
        limactl(&args.iter().map(|s| s.as_str()).collect::<Vec<_>>())?;
        limactl(&["start", name, "--tty=false"])?;
    } else {
        if !o.no_clone {
            eprintln!("wtx: hint: `wtx image build` makes later VM creation take seconds");
        }
        limactl(&["start", "--name", name, "--tty=false", &yaml.to_string_lossy()])?;
    }

    let mut meta = InstanceMeta {
        workdir: workdir.to_string_lossy().into_owned(),
        isolated,
        seeded_from: o.from.clone().unwrap_or_default(),
        ..Default::default()
    };
    if let Some(r) = &repo {
        meta.main_repo = r.host_repo.to_string_lossy().into_owned();
        meta.branch = r.branch.clone();
        if isolated {
            gitiso::setup_isolated_git(name, r, &workdir)?;
            match gitiso::pin_host_objects(&r.host_repo, name) {
                Ok(_) => meta.keep_refs = true,
                Err(e) => eprintln!("wtx: warning: could not create gc-protection refs: {e}"),
            }
        }
    }
    if let Err(e) = mirror::apply_to_vm(name) {
        eprintln!("wtx: warning: mirror config not applied: {e}");
    }
    if !o.no_claude {
        if let Err(e) = crate::creds::copy_claude_creds(name) {
            eprintln!("wtx: warning: claude credentials not copied: {e}");
        }
    }
    std::fs::write(meta_path(name), serde_json::to_string_pretty(&meta)?)?;

    println!("ready:\n  wtx shell {name}");
    if isolated {
        println!("  wtx sync {name}        # fetch commits made in the VM back to the host");
    }
    println!("  wtx rm {name}");
    Ok(())
}

/// 回収されていないVM内コミットがあれば、そのブランチ名を返す。
/// 停止中のVMは問い合わせられないので空（判定不能）を返す。
fn pending_commits(name: &str, meta: &InstanceMeta) -> Vec<String> {
    if !meta.isolated || meta.main_repo.is_empty() || lima_status(name) != "Running" {
        return vec![];
    }
    let repo = Path::new(&meta.main_repo);
    if !repo.exists() {
        return vec![];
    }
    gitiso::unfetched_branches(name, repo).unwrap_or_default()
}

pub fn rm(name: &str, with_worktree: bool, force: bool) -> Result<()> {
    let meta = load_meta(name);
    if let Some(m) = &meta {
        if !force {
            let pending = pending_commits(name, m);
            if !pending.is_empty() {
                return Err(anyhow!(
                    "{name} has commits not yet fetched to the host ({}). \
                     Run `wtx sync {name}` first, or pass --force to discard them",
                    pending.join(", ")
                ));
            }
        }
        if m.keep_refs && !m.main_repo.is_empty() {
            gitiso::unpin_host_objects(Path::new(&m.main_repo), name);
        }
    }
    crate::sshx::close_all_forwards(name);
    limactl(&["delete", "-f", name])?;
    let _ = std::fs::remove_file(wtx_home().join(format!("{name}.yaml")));
    let _ = std::fs::remove_file(meta_path(name));

    if with_worktree {
        let Some(m) = meta else {
            return Err(anyhow!("no metadata for {name}; cannot locate the worktree"));
        };
        // linked worktree のときだけ畳む。通常リポジトリで消すと本体を消してしまう。
        if m.main_repo.is_empty() || m.main_repo == m.workdir {
            eprintln!("wtx: {name} is not a linked worktree; left {} in place", m.workdir);
        } else if !Path::new(&m.workdir).exists() {
            eprintln!("wtx: worktree {} is already gone", m.workdir);
        } else {
            let st = std::process::Command::new("git")
                .arg("-C")
                .arg(&m.main_repo)
                .args(["worktree", "remove", "--force", &m.workdir])
                .status()?;
            if st.success() {
                println!("removed worktree {}", m.workdir);
            } else {
                eprintln!("wtx: could not remove worktree {} (remove it manually)", m.workdir);
            }
        }
    }
    Ok(())
}

pub fn sync(name: &str) -> Result<()> {
    let m = load_meta(name).ok_or_else(|| anyhow!("no metadata for {name}"))?;
    if m.main_repo.is_empty() {
        return Err(anyhow!("{name} is not a git VM; nothing to sync"));
    }
    gitiso::sync(name, Path::new(&m.main_repo), &m.workdir, &m.branch, m.isolated)
}

/// worktree が消えた（孤児）VM を掃除する。
/// 未回収コミットが残っているVMは既定でスキップする。
pub fn prune(force: bool, yes: bool) -> Result<()> {
    let orphans: Vec<Instance> = list_instances().into_iter().filter(|i| i.orphaned).collect();
    if orphans.is_empty() {
        println!("no orphaned VMs");
        return Ok(());
    }
    for i in &orphans {
        let Some(meta) = load_meta(&i.name) else { continue };
        if !force {
            if meta.isolated && lima_status(&i.name) != "Running" {
                println!("starting {} to check for unfetched commits...", i.name);
                if let Err(e) = limactl(&["start", &i.name, "--tty=false"]) {
                    println!("  skip {}: could not start it to verify ({e})", i.name);
                    continue;
                }
            }
            let pending = pending_commits(&i.name, &meta);
            if !pending.is_empty() {
                println!(
                    "  skip {}: unfetched commits on {} (run `wtx sync {}`)",
                    i.name,
                    pending.join(", "),
                    i.name
                );
                continue;
            }
        }
        if !yes {
            println!("  would delete {} (workdir gone: {})", i.name, i.workdir);
            continue;
        }
        match rm(&i.name, false, true) {
            Ok(_) => println!("  deleted {}", i.name),
            Err(e) => println!("  failed to delete {}: {e}", i.name),
        }
    }
    if !yes {
        println!("re-run with --yes to delete them");
    }
    Ok(())
}

/// wtx が把握しているVM一覧（孤児かどうかを含む）。
pub fn ls() {
    let rows = list_instances();
    if rows.is_empty() {
        println!("no VMs. Create one with `wtx up NAME WORKDIR`");
        return;
    }
    println!(
        "{}{}{}{}",
        pad("NAME", 24),
        pad("STATUS", 10),
        pad("GIT", 10),
        pad("BRANCH", 16)
    );
    for i in &rows {
        let git = if i.isolated {
            "isolated"
        } else if i.workdir.is_empty() {
            "-"
        } else {
            "shared"
        };
        let suffix = if i.orphaned { "  (orphaned: workdir gone)" } else { "" };
        println!(
            "{}{}{}{}{}{}",
            pad(&i.name, 24),
            pad(&i.status, 10),
            pad(git, 10),
            pad(&i.branch, 16),
            i.workdir,
            suffix
        );
    }
    if rows.iter().any(|i| i.orphaned) {
        println!("\norphaned VMs can be cleaned up with `wtx prune`");
    }
}

/// TUI 用のインスタンス一覧。
#[derive(Debug, Clone)]
pub struct Instance {
    pub name: String,
    pub status: String,
    pub workdir: String,
    pub branch: String,
    pub isolated: bool,
    /// プロジェクト（ホスト側リポジトリルート）。TUI のグループ化キー。
    pub repo: String,
    /// worktree が消えているVM。VM内のコミットが取り残されている可能性がある。
    pub orphaned: bool,
}

pub fn list_instances() -> Vec<Instance> {
    let out = limactl_out(&["list", "--format", "{{.Name}}\t{{.Status}}"]);
    out.lines()
        .filter_map(|l| {
            let (name, status) = l.split_once('\t')?;
            let meta = load_meta(name);
            let workdir = meta.as_ref().map(|m| m.workdir.clone()).unwrap_or_default();
            Some(Instance {
                name: name.to_string(),
                status: status.to_string(),
                orphaned: !workdir.is_empty() && !Path::new(&workdir).exists(),
                workdir: meta.as_ref().map(|m| m.workdir.clone()).unwrap_or_default(),
                branch: meta.as_ref().map(|m| m.branch.clone()).unwrap_or_default(),
                repo: meta.as_ref().map(|m| m.main_repo.clone()).unwrap_or_default(),
                isolated: meta.map(|m| m.isolated).unwrap_or(false),
            })
        })
        .collect()
}
