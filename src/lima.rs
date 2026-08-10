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
}

pub fn meta_path(name: &str) -> PathBuf {
    wtx_home().join(format!("{name}.json"))
}

pub fn load_meta(name: &str) -> Option<InstanceMeta> {
    serde_json::from_str(&std::fs::read_to_string(meta_path(name)).ok()?).ok()
}

pub struct UpOpts {
    pub memory: String,
    pub cpus: u32,
    pub disk: String,
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
        return Err(anyhow!("{GOLDEN} は既に存在します（作り直すなら wtx image rm）"));
    }
    let yaml = wtx_home().join(format!("{GOLDEN}.yaml"));
    render_yaml(&[], 2, "4GiB", "20GiB", &yaml)?;
    println!("ゴールデンVMをビルドしています（初回のみ、3〜4分）...");
    limactl(&["start", "--name", GOLDEN, "--tty=false", &yaml.to_string_lossy()])?;
    limactl(&["stop", GOLDEN])?; // clone は停止中のインスタンスに対して行う
    println!("完了: 以後の wtx up は {GOLDEN} を clone します");
    Ok(())
}

pub fn image_rm() -> Result<()> {
    limactl(&["delete", "-f", GOLDEN])?;
    let _ = std::fs::remove_file(wtx_home().join(format!("{GOLDEN}.yaml")));
    Ok(())
}

pub fn image_status() {
    if golden_usable() {
        println!("{GOLDEN}: ready (wtx up は clone で高速起動)");
    } else {
        let st = lima_status(GOLDEN);
        if st.is_empty() {
            println!("{GOLDEN}: 未ビルド — `wtx image build` で作成すると VM 作成が数十秒になります");
        } else {
            println!("{GOLDEN}: {st} — clone には停止が必要です (limactl stop {GOLDEN})");
        }
    }
}

pub fn up(name: &str, workdir: &str, o: UpOpts) -> Result<()> {
    let workdir = std::fs::canonicalize(workdir)?;
    if !workdir.is_dir() {
        return Err(anyhow!("workdir not found: {}", workdir.display()));
    }
    if !mirror::mirror_alive() {
        eprintln!("wtx: warning: mirror is down — pull は上流に直行します (wtx mirror up)");
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
            eprintln!("wtx: {abs} は自動でマウント済みのため無視します");
            continue;
        }
        mounts.push(Mount { location: abs, writable: w });
    }

    let yaml = wtx_home().join(format!("{name}.yaml"));
    render_yaml(&mounts, o.cpus, &o.memory, &o.disk, &yaml)?;

    let status = lima_status(name);
    if !status.is_empty() {
        // 既存インスタンスへの再アタッチ（マウント構成は作成時のもの）
        if status != "Running" {
            limactl(&["start", name, "--tty=false"])?;
        }
    } else if !o.no_clone && golden_usable() {
        // プロビジョニング済みVMを clone し、マウントだけ差し替えて起動する。
        // clone 後の lima.yaml は解決済み形式なのでテンプレートで上書きはできず、
        // --mount-only で指定する（ゆえに全マウントはホストと同じ絶対パスに置く）。
        let mem = o.memory.trim_end_matches("GiB").to_string();
        let cpus = o.cpus.to_string();
        let mut args: Vec<String> = vec![
            "clone".into(), GOLDEN.into(), name.into(),
            "--memory".into(), mem, "--cpus".into(), cpus,
        ];
        for m in &mounts {
            args.push("--mount-only".into());
            args.push(if m.writable {
                format!("{}:w", m.location)
            } else {
                m.location.clone()
            });
        }
        limactl(&args.iter().map(|s| s.as_str()).collect::<Vec<_>>())?;
        limactl(&["start", name, "--tty=false"])?;
    } else {
        if !o.no_clone {
            eprintln!("wtx: ヒント: `wtx image build` でゴールデンVMを作ると以後の作成が数十秒になります");
        }
        limactl(&["start", "--name", name, "--tty=false", &yaml.to_string_lossy()])?;
    }

    let mut meta = InstanceMeta {
        workdir: workdir.to_string_lossy().into_owned(),
        isolated,
        ..Default::default()
    };
    if let Some(r) = &repo {
        meta.main_repo = r.host_repo.to_string_lossy().into_owned();
        meta.branch = r.branch.clone();
        if isolated {
            gitiso::setup_isolated_git(name, r, &workdir)?;
            match gitiso::pin_host_objects(&r.host_repo, name) {
                Ok(_) => meta.keep_refs = true,
                Err(e) => eprintln!("wtx: warning: gc保護refを作成できませんでした: {e}"),
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
        println!("  wtx sync {name}        # VM内のコミットをホストへ回収");
    }
    println!("  wtx rm {name}");
    Ok(())
}

pub fn rm(name: &str) -> Result<()> {
    if let Some(m) = load_meta(name) {
        if m.keep_refs && !m.main_repo.is_empty() {
            gitiso::unpin_host_objects(Path::new(&m.main_repo), name);
        }
    }
    crate::sshx::close_all_forwards(name);
    limactl(&["delete", "-f", name])?;
    let _ = std::fs::remove_file(wtx_home().join(format!("{name}.yaml")));
    let _ = std::fs::remove_file(meta_path(name));
    Ok(())
}

pub fn sync(name: &str) -> Result<()> {
    let m = load_meta(name).ok_or_else(|| anyhow!("no metadata for {name}"))?;
    if m.main_repo.is_empty() {
        return Err(anyhow!("{name} is not a git VM; nothing to sync"));
    }
    gitiso::sync(name, Path::new(&m.main_repo), &m.workdir, &m.branch)
}

/// TUI 用のインスタンス一覧。
#[derive(Debug, Clone)]
pub struct Instance {
    pub name: String,
    pub status: String,
    pub workdir: String,
    pub branch: String,
    pub isolated: bool,
}

pub fn list_instances() -> Vec<Instance> {
    let out = limactl_out(&["list", "--format", "{{.Name}}\t{{.Status}}"]);
    out.lines()
        .filter_map(|l| {
            let (name, status) = l.split_once('\t')?;
            let meta = load_meta(name);
            Some(Instance {
                name: name.to_string(),
                status: status.to_string(),
                workdir: meta.as_ref().map(|m| m.workdir.clone()).unwrap_or_default(),
                branch: meta.as_ref().map(|m| m.branch.clone()).unwrap_or_default(),
                isolated: meta.map(|m| m.isolated).unwrap_or(false),
            })
        })
        .collect()
}
