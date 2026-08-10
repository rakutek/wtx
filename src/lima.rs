use crate::mirror;
use crate::repo::{self, RepoKind};
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

/// wtx up 時の判断を記録し、rm / TUI が参照する。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InstanceMeta {
    pub workdir: String,
    #[serde(default)]
    pub main_repo: String,
    #[serde(default)]
    pub branch: String,
    /// `wtx up --from` の clone 元（来歴。空なら通常作成）
    #[serde(default)]
    pub seeded_from: String,
    /// worktree 専用シミュレータのUDID（wtx sim。空なら未作成）
    #[serde(default)]
    pub sim_udid: String,
    #[serde(default)]
    pub sim_devicetype: String,
    /// `wtx sim wire` の割り当て（label → host/guest）。`sim env` が再armに使う
    #[serde(default)]
    pub ports: std::collections::BTreeMap<String, PortMap>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortMap {
    pub host: u16,
    pub guest: u16,
}

pub fn meta_path(name: &str) -> PathBuf {
    wtx_home().join(format!("{name}.json"))
}

pub fn load_meta(name: &str) -> Option<InstanceMeta> {
    serde_json::from_str(&std::fs::read_to_string(meta_path(name)).ok()?).ok()
}

pub fn save_meta(name: &str, meta: &InstanceMeta) -> Result<()> {
    std::fs::write(meta_path(name), serde_json::to_string_pretty(meta)?)?;
    Ok(())
}

pub struct UpOpts {
    pub memory: Option<String>,
    pub cpus: Option<u32>,
    pub disk: String,
    pub from: Option<String>,
    pub no_claude: bool,
    pub no_clone: bool,
    pub extra_mounts: Vec<String>,
    pub sim: bool,
    pub sim_device: Option<String>,
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
/// 新しい worktree のプロジェクト名に付け替える。
/// 隔離gitの除去は旧バージョンのwtxが作ったVMからの移行措置:
/// clone 元に .wtx-local オーバーレイが残っていると、新VMの git が clone 元の
/// VMローカル git を黙って使い続けてしまうため、剥がせない場合だけ失敗させる。
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

# 旧バージョンのwtxが作った隔離git状態の除去（レガシーVMからの移行措置）。
# unit の停止・削除はベストエフォートだが、剥がせない overlay を残したまま進むと
# git が黙って clone 元の状態を指すので、その場合だけは失敗させる。
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

    let repo = repo::inspect_repo(&workdir)?;

    let mut mounts = vec![Mount {
        location: workdir.to_string_lossy().into_owned(),
        writable: true,
    }];
    if let Some(r) = &repo {
        if r.kind == RepoKind::Worktree {
            // メインの .git は workdir の外にあるので別マウントする（rw共有。
            // VM内コミットはホストのブランチをそのまま動かす）
            mounts.push(Mount {
                location: r.host_git.to_string_lossy().into_owned(),
                writable: true,
            });
        }
    }
    // ~/.claude はマウントで共有する。資格情報・settings・skills がホストとライブで
    // 一致し、VM側でのトークンリフレッシュもホストとずれない。
    let host_claude = dirs::home_dir().unwrap_or_default().join(".claude");
    if !o.no_claude {
        if host_claude.is_dir() {
            mounts.push(Mount {
                location: host_claude.to_string_lossy().into_owned(),
                writable: true,
            });
        } else {
            eprintln!("wtx: warning: {} not found; claude runs unauthenticated", host_claude.display());
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
    let existed = !status.is_empty();
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

    // 再アタッチではシミュレータ・ポート割り当てを引き継ぐ（メタは毎回書き直すため）。
    let prev = load_meta(name).unwrap_or_default();
    let mut meta = InstanceMeta {
        workdir: workdir.to_string_lossy().into_owned(),
        seeded_from: o.from.clone().unwrap_or(prev.seeded_from),
        sim_udid: prev.sim_udid,
        sim_devicetype: prev.sim_devicetype,
        ports: prev.ports,
        ..Default::default()
    };
    if let Some(r) = &repo {
        meta.main_repo = r.host_repo.to_string_lossy().into_owned();
        meta.branch = r.branch.clone();
        // 旧バージョンのwtxが作ったVMは隔離gitオーバーレイが生きていて、
        // コミットがVMローカルに落ちたままホストに現れない。作り直しを促す。
        if existed && o.from.is_none() {
            let marker = format!("test -e {}/.wtx-local && echo legacy || true", shq(&r.host_git.to_string_lossy()));
            if let Ok(out) = crate::sshx::capture(name, &marker) {
                if out.contains("legacy") {
                    eprintln!(
                        "wtx: warning: {name} was created by an older wtx with isolated git; \
                         commits made inside it do NOT reach the host. Recreate it: wtx rm {name} && wtx up ..."
                    );
                }
            }
        }
    }
    if let Err(e) = mirror::apply_to_vm(name) {
        eprintln!("wtx: warning: mirror config not applied: {e}");
    }
    // マウントは作成時に固定される。--no-claude で作られた既存VMではマウント先が
    // 存在しないので、スクリプト側の -d ガードが symlink 作成をスキップする。
    if !o.no_claude && host_claude.is_dir() {
        let script = format!(
            r#"set -eu
H={h}
[ -d "$H" ] || exit 0
if [ ! -L "$HOME/.claude" ]; then rm -rf "$HOME/.claude"; ln -s "$H" "$HOME/.claude"; fi"#,
            h = shq(&host_claude.to_string_lossy()),
        );
        if let Err(e) = crate::sshx::vm_script(name, &script, None) {
            eprintln!("wtx: warning: could not link ~/.claude in the VM: {e}");
        }
    }
    // --from: clone 元にシミュレータがあれば、アプリ・データごと複製して引き継ぐ。
    // ポートは label:guest の定義だけ引き継ぎ、ホスト側は新規に払い出す
    // （clone 元と同じホストポートは共存できない）。
    if meta.sim_udid.is_empty() {
        if let Some(src_meta) = o.from.as_deref().and_then(load_meta) {
            if !src_meta.sim_udid.is_empty() {
                match crate::sim::clone_device(&src_meta.sim_udid, name) {
                    Ok(udid) => {
                        meta.sim_udid = udid;
                        meta.sim_devicetype = src_meta.sim_devicetype.clone();
                        meta.ports = crate::sim::inherit_ports(&src_meta.ports);
                    }
                    Err(e) => eprintln!("wtx: warning: simulator not cloned: {e}"),
                }
            }
        }
    }
    if o.sim || o.sim_device.is_some() {
        if let Err(e) = crate::sim::ensure_device(name, &mut meta, o.sim_device.as_deref()) {
            eprintln!("wtx: warning: simulator not created: {e}");
        }
    }
    save_meta(name, &meta)?;

    println!("ready:\n  wtx shell {name}\n  wtx rm {name}");
    Ok(())
}

/// 旧バージョンのwtxが作った gc保護 ref（refs/wtx/keep/<name>/*）の後始末（ベストエフォート）。
fn unpin_legacy_keep_refs(host_repo: &Path, name: &str) {
    let prefix = format!("refs/wtx/keep/{name}/");
    let out = git_out(host_repo, &["for-each-ref", "--format=%(refname)", &prefix]);
    for r in out.split_whitespace() {
        let _ = git_run(host_repo, &["update-ref", "-d", r]);
    }
}

pub fn rm(name: &str, with_worktree: bool) -> Result<()> {
    let meta = load_meta(name);
    if let Some(m) = &meta {
        if !m.main_repo.is_empty() {
            unpin_legacy_keep_refs(Path::new(&m.main_repo), name);
        }
    }
    crate::sshx::close_all_forwards(name);
    if let Some(m) = &meta {
        if !m.sim_udid.is_empty() {
            crate::sim::delete_device(&m.sim_udid);
        }
    }
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

/// worktree が消えた（孤児）VM を掃除する。コミットはホストの .git に直接刻まれている
/// ので、VMを消しても作業が失われることはない。
pub fn prune(yes: bool) -> Result<()> {
    let orphans: Vec<Instance> = list_instances().into_iter().filter(|i| i.orphaned).collect();
    if orphans.is_empty() {
        println!("no orphaned VMs");
        return Ok(());
    }
    for i in &orphans {
        if !yes {
            println!("  would delete {} (workdir gone: {})", i.name, i.workdir);
            continue;
        }
        match rm(&i.name, false) {
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
        "{}{}{}",
        pad("NAME", 24),
        pad("STATUS", 10),
        pad("BRANCH", 16)
    );
    // シミュレータ状態は sim を使うVMがあるときだけ問い合わせる（xcrun 無し環境を巻き込まない）
    let sim_states = crate::sim::states_for(
        &rows.iter().filter(|i| !i.sim_udid.is_empty()).map(|i| i.sim_udid.clone()).collect::<Vec<_>>(),
    );
    for i in &rows {
        let orphan = if i.orphaned { "  (orphaned: workdir gone)" } else { "" };
        let sim = if i.sim_udid.is_empty() {
            String::new()
        } else {
            let st = sim_states.get(&i.sim_udid).map(String::as_str).unwrap_or("missing");
            format!("  sim:{st}")
        };
        println!(
            "{}{}{}{}{}{}",
            pad(&i.name, 24),
            pad(&i.status, 10),
            pad(&i.branch, 16),
            i.workdir,
            sim,
            orphan
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
    /// プロジェクト（ホスト側リポジトリルート）。TUI のグループ化キー。
    pub repo: String,
    /// worktree が消えているVM。コミットはホストにあるので消しても作業は失われない。
    pub orphaned: bool,
    /// worktree 専用シミュレータのUDID（空なら未作成）。
    pub sim_udid: String,
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
                sim_udid: meta.as_ref().map(|m| m.sim_udid.clone()).unwrap_or_default(),
                repo: meta.map(|m| m.main_repo).unwrap_or_default(),
            })
        })
        .collect()
}
