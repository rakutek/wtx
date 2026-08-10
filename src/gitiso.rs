use crate::sshx::vm_script;
use crate::util::{git_out, git_run, lima_dir, shq};
use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// ホストの .git を ro で参照し続けるためのVM内パス。
/// Lima のマウントはすべてホストと同じ絶対パスに置くため（limactl clone --mount-only の制約）、
/// 退避先は VM 内の bind mount で作る。
pub const ISO_BASE_GIT: &str = "/run/wtx/base.git";

#[derive(Debug, Clone, PartialEq)]
pub enum RepoKind {
    Worktree,
    Normal,
}

#[derive(Debug, Clone)]
pub struct RepoInfo {
    pub kind: RepoKind,
    pub host_git: PathBuf,  // ホスト側の .git 実体（VM内でも同じパスに見える）
    pub host_repo: PathBuf, // リポジトリルート（worktreeならメインリポジトリ）
    pub worktree_name: String,
    pub branch: String,
}

fn head_branch(git_dir: &Path) -> String {
    std::fs::read_to_string(git_dir.join("HEAD"))
        .map(|s| s.trim().trim_start_matches("ref: refs/heads/").to_string())
        .unwrap_or_default()
}

/// workdir が linked worktree か通常リポジトリかを判別する。gitリポジトリでなければ None。
pub fn inspect_repo(workdir: &Path) -> Result<Option<RepoInfo>> {
    let dot = workdir.join(".git");
    let md = match std::fs::metadata(&dot) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    if md.is_dir() {
        return Ok(Some(RepoInfo {
            kind: RepoKind::Normal,
            branch: head_branch(&dot),
            host_git: dot,
            host_repo: workdir.to_path_buf(),
            worktree_name: String::new(),
        }));
    }
    let content = std::fs::read_to_string(&dot)?;
    let gd = content
        .trim()
        .strip_prefix("gitdir: ")
        .ok_or_else(|| anyhow!("unrecognized .git file in {}", workdir.display()))?;
    let gd = if Path::new(gd).is_absolute() {
        PathBuf::from(gd)
    } else {
        workdir.join(gd)
    };
    let host_git = gd
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow!("bad gitdir pointer"))?
        .to_path_buf(); // .git/worktrees/<name> → .git
    Ok(Some(RepoInfo {
        kind: RepoKind::Worktree,
        branch: head_branch(&gd),
        worktree_name: gd.file_name().unwrap_or_default().to_string_lossy().into_owned(),
        host_repo: host_git.parent().unwrap_or(Path::new("/")).to_path_buf(),
        host_git,
    }))
}

/// VM内にVMローカルの .git を構築し、ホストの .git に被せる。
///
///   (1) ホストの .git を /run/wtx/base.git に ro bind して退避
///       （private 化しないと後段の bind が peer group 経由で伝播し、退避先まで隠れる）
///   (2) そこから --shared clone（objects は alternates 参照＝コピーゼロ）
///   (3) VMローカルを .git のパスに bind で被せる
///
/// 結果、VM内からホストの .git 実体へ書き込む経路が消え、hooks/config 注入による
/// ホスト側コード実行（VM脱出）・ref破壊・gc事故が構造的に不可能になる。
pub fn setup_isolated_git(vm: &str, repo: &RepoInfo, workdir: &Path) -> Result<()> {
    let worktree_setup = if repo.kind == RepoKind::Worktree {
        r#"
  mkdir -p "$LOCAL/worktrees/$N"
  echo "$WT/.git" > "$LOCAL/worktrees/$N/gitdir"
  echo "../.." > "$LOCAL/worktrees/$N/commondir"
  if [ -f "$BASE/worktrees/$N/HEAD" ]; then
    cp "$BASE/worktrees/$N/HEAD" "$LOCAL/worktrees/$N/HEAD"
  else
    echo "ref: refs/heads/$BRANCH" > "$LOCAL/worktrees/$N/HEAD"
  fi"#
    } else {
        ""
    };

    let script = format!(
        r#"set -eu
WT={wt}
GITDIR={gitdir}
NAME={name}
N={n}
BRANCH={branch}
LOCAL=/var/lib/wtx/git/$NAME
BASE={base}

# A marker file tells us whether the VM-local .git is already bind-mounted on top.
# In worktree mode $GITDIR is itself a Lima mount point, so `mountpoint` cannot tell them apart.
if [ -e "$GITDIR/.wtx-local" ]; then exit 0; fi
if [ ! -d "$GITDIR" ]; then
  echo "wtx: $GITDIR is missing inside the VM (mount not set up)" >&2
  exit 1
fi

sudo mkdir -p "$BASE" /var/lib/wtx/git /usr/local/sbin
sudo chown "$(id -u):$(id -g)" /var/lib/wtx/git
mountpoint -q "$BASE" || {{
  sudo mount --bind "$GITDIR" "$BASE"
  sudo mount --make-private "$BASE"
  sudo mount -o remount,ro,bind "$BASE"
}}
if [ ! -e "$LOCAL/objects/info/alternates" ]; then
  rm -rf "$LOCAL"
  git clone -q --bare --shared "$BASE" "$LOCAL"
  git -C "$LOCAL" config core.bare false
  git -C "$LOCAL" symbolic-ref HEAD "refs/heads/$BRANCH" 2>/dev/null || true
  touch "$LOCAL/.wtx-local"{worktree_setup}
fi
[ -e "$GITDIR/.wtx-local" ] || sudo mount --bind "$LOCAL" "$GITDIR"
git -C "$WT" reset -q

# Recreate the same state after a VM reboot (bind mounts are not persistent)
sudo tee /usr/local/sbin/wtx-gitmount >/dev/null <<EOF
#!/bin/sh
set -e
GITDIR=$GITDIR
LOCAL=$LOCAL
BASE=$BASE
i=0; while [ ! -d "\$GITDIR" ] && [ \$i -lt 60 ]; do sleep 1; i=\$((i+1)); done
mkdir -p "\$BASE"
mountpoint -q "\$BASE" || {{ mount --bind "\$GITDIR" "\$BASE"; mount --make-private "\$BASE"; mount -o remount,ro,bind "\$BASE"; }}
[ -e "\$GITDIR/.wtx-local" ] || mount --bind "\$LOCAL" "\$GITDIR"
EOF
sudo chmod +x /usr/local/sbin/wtx-gitmount
sudo tee /etc/systemd/system/wtx-gitmount.service >/dev/null <<'EOF'
[Unit]
Description=wtx isolated git bind mounts
After=local-fs.target
[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/sbin/wtx-gitmount
[Install]
WantedBy=multi-user.target
EOF
sudo systemctl daemon-reload
sudo systemctl enable -q wtx-gitmount.service
"#,
        wt = shq(&workdir.to_string_lossy()),
        gitdir = shq(&repo.host_git.to_string_lossy()),
        name = shq(vm),
        n = shq(&repo.worktree_name),
        branch = shq(&repo.branch),
        base = ISO_BASE_GIT,
        worktree_setup = worktree_setup,
    );
    vm_script(vm, &script, None)
}

/// ホスト側の現在のブランチ先端を refs/wtx/keep/<name>/* に固定し、
/// VM が alternates 経由で参照中の object がホストの gc で刈られるのを防ぐ。
pub fn pin_host_objects(host_repo: &Path, name: &str) -> Result<()> {
    let out = git_out(
        host_repo,
        &["for-each-ref", "--format=%(objectname) %(refname:short)", "refs/heads/"],
    );
    for line in out.lines() {
        if let Some((sha, branch)) = line.split_once(' ') {
            git_run(host_repo, &["update-ref", &format!("refs/wtx/keep/{name}/{branch}"), sha])?;
        }
    }
    Ok(())
}

pub fn unpin_host_objects(host_repo: &Path, name: &str) {
    let prefix = format!("refs/wtx/keep/{name}/");
    let out = git_out(host_repo, &["for-each-ref", "--format=%(refname)", &prefix]);
    for r in out.split_whitespace() {
        let _ = git_run(host_repo, &["update-ref", "-d", r]);
    }
}

/// VM 内のVMローカル git の絶対パス。worktree が消えても残るので、
/// 孤児VMからのコミット回収はここを起点にする。
pub fn vm_git_path(vm: &str) -> String {
    format!("/var/lib/wtx/git/{vm}")
}

/// まだホストに取り込まれていないVM内ブランチを返す（`wtx rm` / `prune` の安全弁）。
/// VMローカル git のブランチ先端が、ホスト側リポジトリに object として存在するかで判定する。
pub fn unfetched_branches(vm: &str, host_repo: &Path) -> Result<Vec<String>> {
    let out = crate::sshx::capture(
        vm,
        &format!(
            "git -C {} for-each-ref --format='%(objectname) %(refname:short)' refs/heads/",
            shq(&vm_git_path(vm))
        ),
    )?;
    let mut pending = vec![];
    for line in out.lines() {
        let Some((sha, branch)) = line.split_once(' ') else { continue };
        let known = Command::new("git")
            .arg("-C")
            .arg(host_repo)
            .args(["cat-file", "-e", &format!("{sha}^{{commit}}")])
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !known {
            pending.push(branch.to_string());
        }
    }
    Ok(pending)
}

/// VM 内のブランチ群を refs/wtx/<name>/* としてホストのリポジトリへ fetch する。
pub fn sync(name: &str, host_repo: &Path, workdir: &str, branch: &str, isolated: bool) -> Result<()> {
    let ssh_cmd = format!(
        "ssh -F {} -o ControlMaster=no -o ControlPath=none",
        shq(&lima_dir(name).join("ssh.config").to_string_lossy())
    );
    // 隔離モードではVMローカル git を直接指す。worktree が消えた（孤児）VMからでも回収できる。
    let remote = if isolated { vm_git_path(name) } else { workdir.to_string() };
    let st = Command::new("git")
        .arg("-C")
        .arg(host_repo)
        .args([
            "fetch",
            &format!("lima-{name}:{remote}"),
            &format!("+refs/heads/*:refs/wtx/{name}/*"),
        ])
        .env("GIT_SSH_COMMAND", ssh_cmd)
        .status()?;
    if !st.success() {
        return Err(anyhow!("git fetch failed"));
    }
    if !branch.is_empty() {
        if Path::new(workdir).exists() {
            println!("to merge: git -C {workdir} merge --ff-only refs/wtx/{name}/{branch}");
        } else {
            // 孤児VM: 元の worktree が無いので、回収先の ref だけ伝える
            println!("fetched into refs/wtx/{name}/{branch} (the worktree is gone; check it out where you need it)");
        }
    }
    Ok(())
}
