use crate::mirror;
use crate::repo::{self, RepoKind};
use crate::util::*;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    /// task 状態は持たず、外部オーケストレータがcleanupに使う所有来歴だけを記録する。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<OwnerMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OwnerMeta {
    pub kind: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortMap {
    pub host: u16,
    pub guest: u16,
}

pub const RECEIPT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize)]
struct WorktreeInspection {
    path: String,
    repo: String,
    branch: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    head: String,
    orphaned: bool,
}

#[derive(Debug, Serialize)]
struct RuntimeInspection {
    docker: &'static str,
}

#[derive(Debug, Serialize)]
struct PortInspection {
    host: u16,
    guest: u16,
    forward_alive: bool,
}

#[derive(Debug, Serialize)]
struct SimulatorInspection {
    udid: String,
    device_type: String,
    state: String,
}

#[derive(Debug, Serialize)]
struct InstanceInspection {
    name: String,
    status: String,
    ready: bool,
    runtime: RuntimeInspection,
    worktree: WorktreeInspection,
    #[serde(skip_serializing_if = "String::is_empty")]
    seeded_from: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<OwnerMeta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    simulator: Option<SimulatorInspection>,
    ports: BTreeMap<String, PortInspection>,
}

#[derive(Debug, Serialize)]
struct InspectReceipt {
    schema_version: u32,
    instance: InstanceInspection,
}

#[derive(Debug, Serialize)]
struct EnsureReceipt {
    schema_version: u32,
    action: &'static str,
    instance: InstanceInspection,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum RemoveAction {
    Deleted,
    NotFound,
}

#[derive(Debug, Serialize)]
struct RemoveReceipt<'a> {
    schema_version: u32,
    action: RemoveAction,
    name: &'a str,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RemoveOpts {
    pub with_worktree: bool,
    pub if_exists: bool,
    pub json: bool,
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

fn up_limactl(args: &[&str], quiet: bool) -> Result<()> {
    if quiet {
        limactl_capture(args).map_err(|e| anyhow!("limactl {}: {e}", args.join(" ")))
    } else {
        limactl(args)
    }
}

fn render_yaml(mounts: &[Mount], cpus: u32, memory: &str, disk: &str, path: &Path) -> Result<()> {
    let m: String = mounts
        .iter()
        .map(|m| {
            format!(
                "- location: \"{}\"\n  writable: {}\n",
                m.location, m.writable
            )
        })
        .collect();
    let yaml = TEMPLATE
        .replace("__CPUS__", &cpus.to_string())
        .replace("__MEMORY__", memory)
        .replace("__DISK__", disk)
        .replace("__MOUNTS__", m.trim_end())
        .replace("__MIRROR_PORT__", &mirror::mirror_port().to_string())
        .replace("__GIT_NAME__", &git_config_global("user.name", "wtx"))
        .replace(
            "__GIT_EMAIL__",
            &git_config_global("user.email", "wtx@localhost"),
        );
    std::fs::write(path, yaml)?;
    Ok(())
}

pub fn golden_usable() -> bool {
    lima_dir(GOLDEN).join("lima.yaml").exists() && lima_status(GOLDEN) == "Stopped"
}

pub fn image_build() -> Result<()> {
    if lima_dir(GOLDEN).exists() {
        return Err(anyhow!(
            "{GOLDEN} already exists (run `wtx image rm` to rebuild it)"
        ));
    }
    let yaml = wtx_home().join(format!("{GOLDEN}.yaml"));
    render_yaml(&[], 2, "4GiB", "20GiB", &yaml)?;
    println!("Building the golden VM (one-time, 3-4 min)...");
    limactl(&[
        "start",
        "--name",
        GOLDEN,
        "--tty=false",
        &yaml.to_string_lossy(),
    ])?;
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
    cleaned
        .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_string()
}

/// `--from` で clone したVMから clone 元由来の状態を取り除き、compose の volume 名を
/// 新しい worktree のプロジェクト名に付け替える。
/// 隔離gitの除去は旧バージョンのwtxが作ったVMからの移行措置:
/// clone 元に .wtx-local オーバーレイが残っていると、新VMの git が clone 元の
/// VMローカル git を黙って使い続けてしまうため、剥がせない場合だけ失敗させる。
fn seed_cleanup(name: &str, src: &str, workdir: &Path, quiet: bool) -> Result<()> {
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
    crate::sshx::vm_script_with_output(name, &script, None, !quiet)
}

pub fn up(name: &str, workdir: &str, o: UpOpts) -> Result<()> {
    up_inner(name, workdir, o, false)?;
    println!("ready:\n  wtx shell {name}\n  wtx rm {name}");
    Ok(())
}

fn up_inner(name: &str, workdir: &str, o: UpOpts, quiet: bool) -> Result<()> {
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
            eprintln!(
                "wtx: warning: {} not found; claude runs unauthenticated",
                host_claude.display()
            );
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
        mounts.push(Mount {
            location: abs,
            writable: w,
        });
    }

    let yaml = wtx_home().join(format!("{name}.yaml"));
    render_yaml(
        &mounts,
        o.cpus.unwrap_or(2),
        o.memory.as_deref().unwrap_or("4GiB"),
        &o.disk,
        &yaml,
    )?;

    let status = lima_status(name);
    let existed = !status.is_empty();
    if let Some(src) = o.from.as_deref() {
        // 既存VMを clone して DB（volume）・イメージ・導入済みツールごと引き継ぐ。
        // 停止中のディスクを複製するので at-rest の一貫したコピーになる。
        if !status.is_empty() {
            return Err(anyhow!(
                "{name} already exists; --from can only seed a new VM"
            ));
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
            if !quiet {
                println!("stopping {src} for a consistent copy (it restarts in the background)...");
            }
            up_limactl(&["stop", src], quiet)?;
        }
        let args = clone_args(src, name, &o, &mounts);
        up_limactl(&args.iter().map(|s| s.as_str()).collect::<Vec<_>>(), quiet)?;
        if was_running {
            // 新VMの起動と並行して clone 元を復帰させ、ダウンタイムを clone の間だけにする
            let _ = std::process::Command::new("limactl")
                .args(["start", "--tty=false", src])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
        up_limactl(&["start", name, "--tty=false"], quiet)?;
        seed_cleanup(name, src, &workdir, quiet)?;
    } else if !status.is_empty() {
        // 既存インスタンスへの再アタッチ（マウント構成は作成時のもの）
        if status != "Running" {
            up_limactl(&["start", name, "--tty=false"], quiet)?;
        }
    } else if !o.no_clone && golden_usable() {
        let args = clone_args(GOLDEN, name, &o, &mounts);
        up_limactl(&args.iter().map(|s| s.as_str()).collect::<Vec<_>>(), quiet)?;
        up_limactl(&["start", name, "--tty=false"], quiet)?;
    } else {
        if !o.no_clone {
            eprintln!("wtx: hint: `wtx image build` makes later VM creation take seconds");
        }
        up_limactl(
            &[
                "start",
                "--name",
                name,
                "--tty=false",
                &yaml.to_string_lossy(),
            ],
            quiet,
        )?;
    }

    // 再アタッチではシミュレータ・ポート割り当てを引き継ぐ（メタは毎回書き直すため）。
    let prev = load_meta(name).unwrap_or_default();
    let mut meta = InstanceMeta {
        workdir: workdir.to_string_lossy().into_owned(),
        seeded_from: o.from.clone().unwrap_or(prev.seeded_from),
        sim_udid: prev.sim_udid,
        sim_devicetype: prev.sim_devicetype,
        ports: prev.ports,
        owner: prev.owner,
        ..Default::default()
    };
    if let Some(r) = &repo {
        meta.main_repo = r.host_repo.to_string_lossy().into_owned();
        meta.branch = r.branch.clone();
        // 旧バージョンのwtxが作ったVMは隔離gitオーバーレイが生きていて、
        // コミットがVMローカルに落ちたままホストに現れない。作り直しを促す。
        if existed && o.from.is_none() {
            let marker = format!(
                "test -e {}/.wtx-local && echo legacy || true",
                shq(&r.host_git.to_string_lossy())
            );
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
                match crate::sim::clone_device_with_output(&src_meta.sim_udid, name, !quiet) {
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
        if let Err(e) =
            crate::sim::ensure_device_with_output(name, &mut meta, o.sim_device.as_deref(), !quiet)
        {
            eprintln!("wtx: warning: simulator not created: {e}");
        }
    }
    save_meta(name, &meta)?;
    Ok(())
}

/// オーケストレータ向けの冪等なVM準備。既存VMでは作成時専用の --from を再適用せず、
/// 記録済みのseedと要求が一致することだけを検証する。
pub fn ensure(
    name: &str,
    workdir: &str,
    mut o: UpOpts,
    owner: Option<OwnerMeta>,
    timeout_seconds: u64,
    json: bool,
) -> Result<()> {
    if timeout_seconds == 0 {
        return Err(anyhow!("--timeout-seconds must be greater than zero"));
    }
    let workdir = std::fs::canonicalize(workdir)?;
    ensure_name_matches(name, &workdir)?;

    let before = lima_status(name);
    let action = if before.is_empty() {
        "created"
    } else if before == "Running" {
        "reused"
    } else {
        "started"
    };

    if !before.is_empty() {
        let existing = load_meta(name).ok_or_else(|| {
            anyhow!("VM {name} exists but has no wtx metadata; refusing to adopt it implicitly")
        })?;
        if let Some(requested) = o.from.as_deref() {
            if existing.seeded_from != requested {
                let actual = if existing.seeded_from.is_empty() {
                    "<none>"
                } else {
                    &existing.seeded_from
                };
                return Err(anyhow!(
                    "VM {name} already exists with seeded_from={actual}; requested {requested}"
                ));
            }
        }
        // --from は新規作成専用。検証後は通常の再アタッチ経路へ流す。
        o.from = None;
    }

    up_inner(name, &workdir.to_string_lossy(), o, json)?;

    if let Some(owner) = owner {
        let mut meta = load_meta(name).ok_or_else(|| anyhow!("metadata disappeared for {name}"))?;
        meta.owner = Some(owner);
        save_meta(name, &meta)?;
    }

    crate::sshx::wait_docker_ready(name, Duration::from_secs(timeout_seconds))?;
    let instance = inspect_instance(name)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&EnsureReceipt {
                schema_version: RECEIPT_SCHEMA_VERSION,
                action,
                instance,
            })?
        );
    } else {
        println!("{action}: {name} is ready");
        println!("  worktree: {}", instance.worktree.path);
        println!("  run: wtx shell {name}");
        println!("  remove: wtx rm {name}");
    }
    Ok(())
}

fn inspect_instance(name: &str) -> Result<InstanceInspection> {
    let meta =
        load_meta(name).ok_or_else(|| anyhow!("no metadata for {name} (is it a wtx VM?)"))?;
    let status = lima_status(name);
    let docker_ready = status == "Running" && crate::sshx::docker_ready(name);
    let orphaned = meta.workdir.is_empty() || !Path::new(&meta.workdir).exists();
    let head = if orphaned {
        String::new()
    } else {
        git_out(Path::new(&meta.workdir), &["rev-parse", "HEAD"])
    };

    let sim_states = if meta.sim_udid.is_empty() {
        BTreeMap::new()
    } else {
        crate::sim::states_for(std::slice::from_ref(&meta.sim_udid))
    };
    let simulator = if meta.sim_udid.is_empty() {
        None
    } else {
        Some(SimulatorInspection {
            state: sim_states
                .get(&meta.sim_udid)
                .cloned()
                .unwrap_or_else(|| "missing".to_string()),
            udid: meta.sim_udid.clone(),
            device_type: meta.sim_devicetype.clone(),
        })
    };
    let ports = meta
        .ports
        .iter()
        .map(|(label, port)| {
            (
                label.clone(),
                PortInspection {
                    host: port.host,
                    guest: port.guest,
                    forward_alive: crate::sshx::master_alive(name, port.host),
                },
            )
        })
        .collect();

    Ok(InstanceInspection {
        name: name.to_string(),
        status,
        ready: docker_ready,
        runtime: RuntimeInspection {
            docker: if docker_ready { "ready" } else { "unavailable" },
        },
        worktree: WorktreeInspection {
            path: meta.workdir,
            repo: meta.main_repo,
            branch: meta.branch,
            head,
            orphaned,
        },
        seeded_from: meta.seeded_from,
        owner: meta.owner,
        simulator,
        ports,
    })
}

pub fn inspect(name: Option<&str>, json: bool) -> Result<()> {
    let (name, _) = crate::sim::resolve(name)?;
    let instance = inspect_instance(&name)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&InspectReceipt {
                schema_version: RECEIPT_SCHEMA_VERSION,
                instance,
            })?
        );
        return Ok(());
    }

    println!(
        "{}: {} ({})",
        instance.name,
        instance.status,
        if instance.ready { "ready" } else { "not ready" }
    );
    println!("  worktree: {}", instance.worktree.path);
    if !instance.worktree.branch.is_empty() {
        println!("  branch: {}", instance.worktree.branch);
    }
    println!("  docker: {}", instance.runtime.docker);
    if !instance.seeded_from.is_empty() {
        println!("  seeded from: {}", instance.seeded_from);
    }
    if let Some(owner) = &instance.owner {
        println!("  owner: {}", owner.kind);
        for (key, value) in &owner.labels {
            println!("    {key}={value}");
        }
    }
    if let Some(sim) = &instance.simulator {
        println!(
            "  simulator: {} ({}) [{}]",
            sim.udid, sim.device_type, sim.state
        );
    }
    for (label, port) in &instance.ports {
        println!(
            "  port {label}: host {} -> guest {} [{}]",
            port.host,
            port.guest,
            if port.forward_alive { "armed" } else { "down" }
        );
    }
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

pub fn rm(name: &str, opts: RemoveOpts) -> Result<()> {
    let action = remove_instance(name, opts)?;
    print_remove_result(name, action, opts.json)
}

fn remove_instance(name: &str, opts: RemoveOpts) -> Result<RemoveAction> {
    let meta = load_meta(name);
    let vm_exists = !lima_status_checked(name)?.is_empty();
    if !vm_exists && meta.is_none() {
        if !opts.if_exists {
            return Err(anyhow!("no such wtx VM: {name}"));
        }
        return Ok(RemoveAction::NotFound);
    }
    if let Some(m) = &meta {
        if !m.main_repo.is_empty() {
            unpin_legacy_keep_refs(Path::new(&m.main_repo), name);
        }
    }
    crate::sshx::close_all_forwards(name);
    if let Some(m) = &meta {
        if !m.sim_udid.is_empty() {
            if opts.json {
                crate::sim::delete_device_quietly(&m.sim_udid);
            } else {
                crate::sim::delete_device(&m.sim_udid);
            }
        }
    }
    if vm_exists {
        limactl_capture(&["delete", "-f", name]).map_err(|e| anyhow!("limactl delete: {e}"))?;
    }
    remove_managed_file(&wtx_home().join(format!("{name}.yaml")))?;
    remove_managed_file(&meta_path(name))?;

    if opts.with_worktree {
        remove_linked_worktree(name, meta.as_ref(), opts.json)?;
    }
    Ok(RemoveAction::Deleted)
}

fn remove_linked_worktree(name: &str, meta: Option<&InstanceMeta>, quiet: bool) -> Result<()> {
    let m = meta.ok_or_else(|| anyhow!("no metadata for {name}; cannot locate the worktree"))?;

    // linked worktree のときだけ畳む。通常リポジトリで消すと本体を消してしまう。
    if m.main_repo.is_empty() || m.main_repo == m.workdir {
        eprintln!(
            "wtx: {name} is not a linked worktree; left {} in place",
            m.workdir
        );
    } else if !Path::new(&m.workdir).exists() {
        eprintln!("wtx: worktree {} is already gone", m.workdir);
    } else {
        let st = std::process::Command::new("git")
            .arg("-C")
            .arg(&m.main_repo)
            .args(["worktree", "remove", "--force", &m.workdir])
            .status()?;
        if st.success() {
            if !quiet {
                println!("removed worktree {}", m.workdir);
            }
        } else {
            eprintln!(
                "wtx: could not remove worktree {} (remove it manually)",
                m.workdir
            );
        }
    }
    Ok(())
}

fn print_remove_result(name: &str, action: RemoveAction, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&RemoveReceipt {
                schema_version: RECEIPT_SCHEMA_VERSION,
                action,
                name,
            })?
        );
    } else if matches!(action, RemoveAction::NotFound) {
        println!("not_found: {name}");
    }
    Ok(())
}

fn remove_managed_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(anyhow!("remove {}: {err}", path.display())),
    }
}

/// worktree が消えた（孤児）VM を掃除する。コミットはホストの .git に直接刻まれている
/// ので、VMを消しても作業が失われることはない。
pub fn prune(yes: bool) -> Result<()> {
    let orphans: Vec<Instance> = list_instances()
        .into_iter()
        .filter(|i| i.orphaned)
        .collect();
    if orphans.is_empty() {
        println!("no orphaned VMs");
        return Ok(());
    }
    for i in &orphans {
        if !yes {
            println!("  would delete {} (workdir gone: {})", i.name, i.workdir);
            continue;
        }
        match rm(&i.name, RemoveOpts::default()) {
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
        &rows
            .iter()
            .filter(|i| !i.sim_udid.is_empty())
            .map(|i| i.sim_udid.clone())
            .collect::<Vec<_>>(),
    );
    for i in &rows {
        let orphan = if i.orphaned {
            "  (orphaned: workdir gone)"
        } else {
            ""
        };
        let sim = if i.sim_udid.is_empty() {
            String::new()
        } else {
            let st = sim_states
                .get(&i.sim_udid)
                .map(String::as_str)
                .unwrap_or("missing");
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

/// `wtx ls --json`: エージェント・スクリプト向けの機械可読出力。
pub fn ls_json() -> Result<()> {
    #[derive(Serialize)]
    struct Row {
        name: String,
        status: String,
        branch: String,
        workdir: String,
        repo: String,
        orphaned: bool,
        #[serde(skip_serializing_if = "String::is_empty")]
        sim_udid: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        sim_state: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        owner: Option<OwnerMeta>,
    }
    let rows = list_instances();
    let sim_states = crate::sim::states_for(
        &rows
            .iter()
            .filter(|i| !i.sim_udid.is_empty())
            .map(|i| i.sim_udid.clone())
            .collect::<Vec<_>>(),
    );
    let out: Vec<Row> = rows
        .into_iter()
        .map(|i| Row {
            sim_state: if i.sim_udid.is_empty() {
                None
            } else {
                Some(
                    sim_states
                        .get(&i.sim_udid)
                        .cloned()
                        .unwrap_or_else(|| "missing".to_string()),
                )
            },
            name: i.name,
            status: i.status,
            branch: i.branch,
            workdir: i.workdir,
            repo: i.repo,
            orphaned: i.orphaned,
            sim_udid: i.sim_udid,
            owner: i.owner,
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
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
    /// 外部オーケストレータの所有来歴。
    pub owner: Option<OwnerMeta>,
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
                sim_udid: meta
                    .as_ref()
                    .map(|m| m.sim_udid.clone())
                    .unwrap_or_default(),
                owner: meta.as_ref().and_then(|m| m.owner.clone()),
                repo: meta.map(|m| m.main_repo).unwrap_or_default(),
            })
        })
        .collect()
}

/// VM名をディレクトリ名から作る（Limaのインスタンス名に使える文字だけ残す）。
pub fn derive_name(dir: &Path) -> Result<String> {
    let base = dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_string();
    if cleaned.is_empty() {
        return Err(anyhow!(
            "cannot derive a VM name from {}; pass NAME explicitly",
            dir.display()
        ));
    }
    Ok(cleaned)
}

/// 導出した名前が別の workdir のVMに既に使われていたら失敗させる。
/// 黙って再アタッチすると、そのVMの workdir 記録を書き換えた上に
/// マウントは旧worktreeのままという壊れた状態を作ってしまう。
pub fn ensure_name_matches(name: &str, dir: &Path) -> Result<()> {
    if let Some(m) = load_meta(name) {
        if !m.workdir.is_empty() && Path::new(&m.workdir) != dir {
            return Err(anyhow!(
                "VM {name} already exists for {}; pass a different NAME",
                m.workdir
            ));
        }
    }
    Ok(())
}

/// `wtx new BRANCH`: worktree とVMを一度に作る。ブランチが無ければ現在の HEAD から作る。
pub fn new(branch: &str, dir: Option<&str>, o: UpOpts) -> Result<()> {
    let cwd = std::fs::canonicalize(std::env::current_dir()?)?;
    let repo = repo::inspect_repo(&cwd)?.ok_or_else(|| anyhow!("not inside a git repository"))?;
    let main_repo = repo.host_repo;
    let dirpath = match dir {
        // git は相対パスをリポジトリ基準で解決するので、先にカレント基準で絶対化する
        Some(d) => {
            let p = Path::new(d);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                std::env::current_dir()?.join(p)
            }
        }
        None => {
            let repo_name = main_repo
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            main_repo
                .parent()
                .unwrap_or(Path::new("."))
                .join(format!("{repo_name}-{}", branch.replace('/', "-")))
        }
    };
    if dirpath.exists() {
        return Err(anyhow!("{} already exists", dirpath.display()));
    }
    // VM名の衝突は worktree を作る前に検査する（作った後に失敗すると片付けが残る）
    let name = derive_name(&dirpath)?;
    ensure_name_matches(&name, &dirpath)?;

    let branch_ref = format!("refs/heads/{branch}");
    let exists = git_run(
        &main_repo,
        &["show-ref", "--verify", "--quiet", &branch_ref],
    )
    .is_ok();
    let dirstr = dirpath.to_string_lossy().into_owned();
    let mut args: Vec<&str> = vec!["worktree", "add"];
    if exists {
        args.push(&dirstr);
        args.push(branch);
    } else {
        args.push("-b");
        args.push(branch);
        args.push(&dirstr);
    }
    // git の出力はそのまま見せる（ブランチが他の worktree で checkout 済み等のエラーのため）
    let st = std::process::Command::new("git")
        .arg("-C")
        .arg(&main_repo)
        .args(&args)
        .status()?;
    if !st.success() {
        return Err(anyhow!("git worktree add failed"));
    }
    up(&name, &dirstr, o)
}
