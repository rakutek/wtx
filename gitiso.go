package main

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

// isoBaseGit はホストの .git を ro で参照し続けるためのVM内パス。
// Lima のマウントはすべてホストと同じ絶対パスに置き（limactl clone --mount-only の制約）、
// VM内で bind mount により「ホストの .git を退避 → VMローカルの .git を被せる」を行う。
const isoBaseGit = "/run/wtx/base.git"

type repoKind int

const (
	repoWorktree repoKind = iota // git worktree add で作られた linked worktree
	repoNormal                   // 通常のリポジトリ（.git がディレクトリ）
)

type repoInfo struct {
	Kind         repoKind
	HostGit      string // ホスト側の .git 実体（VM内でも同じパスに見える）
	HostRepo     string // ホスト側リポジトリルート（worktreeならメインリポジトリ）
	WorktreeName string // .git/worktrees/<name>
	Branch       string
}

func headBranch(gitDir string) string {
	b, err := os.ReadFile(filepath.Join(gitDir, "HEAD"))
	if err != nil {
		return ""
	}
	return strings.TrimPrefix(strings.TrimSpace(string(b)), "ref: refs/heads/")
}

// inspectRepo は workdir が linked worktree か通常リポジトリかを判別する。
// gitリポジトリでなければ nil を返す（隔離gitの対象外）。
func inspectRepo(workdir string) (*repoInfo, error) {
	dot := filepath.Join(workdir, ".git")
	st, err := os.Stat(dot)
	if err != nil {
		return nil, nil
	}
	if st.IsDir() {
		return &repoInfo{Kind: repoNormal, HostGit: dot, HostRepo: workdir, Branch: headBranch(dot)}, nil
	}
	b, err := os.ReadFile(dot)
	if err != nil {
		return nil, err
	}
	gd, ok := strings.CutPrefix(strings.TrimSpace(string(b)), "gitdir: ")
	if !ok {
		return nil, fmt.Errorf("unrecognized .git file in %s", workdir)
	}
	if !filepath.IsAbs(gd) {
		gd = filepath.Join(workdir, gd)
	}
	info := &repoInfo{
		Kind:         repoWorktree,
		HostGit:      filepath.Dir(filepath.Dir(gd)), // .git/worktrees/<name> → .git
		WorktreeName: filepath.Base(gd),
		Branch:       headBranch(gd),
	}
	info.HostRepo = filepath.Dir(info.HostGit)
	return info, nil
}

// setupIsolatedGit はVM内にVMローカルの .git を構築し、ホストの .git に被せる。
//
//	(1) ホストの .git を /run/wtx/base.git に ro で bind（この bind は後段の shadow 後も生き続ける）
//	(2) そこから --shared clone してVMローカルの複製を作る（objects は alternates 参照＝コピーゼロ）
//	(3) VMローカルを .git のパスに bind して被せる
//
// 結果、VM内からホストの .git 実体へ書き込む経路が消え、hooks/config 注入による
// ホスト側コード実行（VM脱出）・ref破壊・gc事故が構造的に不可能になる。
func setupIsolatedGit(vm string, repo *repoInfo, workdir string) error {
	worktreeSetup := ""
	if repo.Kind == repoWorktree {
		// linked worktree の gitdir ポインタが指す先をVMローカル側に再現する
		worktreeSetup = `
  mkdir -p "$LOCAL/worktrees/$N"
  echo "$WT/.git" > "$LOCAL/worktrees/$N/gitdir"
  echo "../.." > "$LOCAL/worktrees/$N/commondir"
  if [ -f "$BASE/worktrees/$N/HEAD" ]; then
    cp "$BASE/worktrees/$N/HEAD" "$LOCAL/worktrees/$N/HEAD"
  else
    echo "ref: refs/heads/$BRANCH" > "$LOCAL/worktrees/$N/HEAD"
  fi`
	}
	r := strings.NewReplacer(
		"@WT@", shellQuote(workdir),
		"@GITDIR@", shellQuote(repo.HostGit),
		"@NAME@", shellQuote(vm),
		"@N@", shellQuote(repo.WorktreeName),
		"@BRANCH@", shellQuote(repo.Branch),
		"@WORKTREE_SETUP@", worktreeSetup,
	)
	return vmScript(vm, r.Replace(`set -eu
WT=@WT@
GITDIR=@GITDIR@
NAME=@NAME@
N=@N@
BRANCH=@BRANCH@
LOCAL=/var/lib/wtx/git/$NAME
BASE=`+isoBaseGit+`

# 「VMローカルの .git が被さっているか」はマーカーで判定する。
# worktree モードでは $GITDIR は Lima の ro マウント地点なので mountpoint 判定では区別できない。
if [ -e "$GITDIR/.wtx-local" ]; then exit 0; fi
if [ ! -d "$GITDIR" ]; then
  echo "wtx: $GITDIR がVM内に見つかりません（マウント漏れ）" >&2
  exit 1
fi

sudo mkdir -p "$BASE" /var/lib/wtx/git /usr/local/sbin
sudo chown "$(id -u):$(id -g)" /var/lib/wtx/git
mountpoint -q "$BASE" || {
  sudo mount --bind "$GITDIR" "$BASE"
  # private 化しないと、後段で $GITDIR に被せる bind が peer group 経由で
  # $BASE にも伝播し、退避したはずのホスト .git が隠れてしまう
  sudo mount --make-private "$BASE"
  sudo mount -o remount,ro,bind "$BASE"
}
if [ ! -e "$LOCAL/objects/info/alternates" ]; then
  rm -rf "$LOCAL"
  git clone -q --bare --shared "$BASE" "$LOCAL"
  git -C "$LOCAL" config core.bare false
  git -C "$LOCAL" symbolic-ref HEAD "refs/heads/$BRANCH" 2>/dev/null || true
  touch "$LOCAL/.wtx-local"@WORKTREE_SETUP@
fi
[ -e "$GITDIR/.wtx-local" ] || sudo mount --bind "$LOCAL" "$GITDIR"
git -C "$WT" reset -q

# VM再起動後も同じ状態を再現する（bind mount は永続しないため）
sudo tee /usr/local/sbin/wtx-gitmount >/dev/null <<EOF
#!/bin/sh
set -e
GITDIR=$GITDIR
LOCAL=$LOCAL
BASE=$BASE
i=0; while [ ! -d "\$GITDIR" ] && [ \$i -lt 60 ]; do sleep 1; i=\$((i+1)); done
mkdir -p "\$BASE"
mountpoint -q "\$BASE" || { mount --bind "\$GITDIR" "\$BASE"; mount --make-private "\$BASE"; mount -o remount,ro,bind "\$BASE"; }
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
`), nil)
}

// pinHostObjects はホスト側の現在のブランチ先端を refs/wtx/keep/<name>/* に固定し、
// VM が alternates 経由で参照中の object がホストの gc で刈られるのを防ぐ。
func pinHostObjects(hostRepo, name string) error {
	out, err := exec.Command("git", "-C", hostRepo, "for-each-ref",
		"--format=%(objectname) %(refname:short)", "refs/heads/").Output()
	if err != nil {
		return err
	}
	for _, line := range strings.Split(strings.TrimSpace(string(out)), "\n") {
		sha, branch, ok := strings.Cut(line, " ")
		if !ok {
			continue
		}
		if err := exec.Command("git", "-C", hostRepo, "update-ref",
			"refs/wtx/keep/"+name+"/"+branch, sha).Run(); err != nil {
			return err
		}
	}
	return nil
}

func unpinHostObjects(hostRepo, name string) error {
	prefix := "refs/wtx/keep/" + name + "/"
	out, err := exec.Command("git", "-C", hostRepo, "for-each-ref", "--format=%(refname)", prefix).Output()
	if err != nil {
		return fmt.Errorf("gc保護refを削除できませんでした: %w", err)
	}
	for _, ref := range strings.Fields(string(out)) {
		_ = exec.Command("git", "-C", hostRepo, "update-ref", "-d", ref).Run()
	}
	return nil
}

// cmdSync は VM 内のブランチ群を refs/wtx/<name>/* としてホストのリポジトリへ fetch する。
func cmdSync(args []string) error {
	if len(args) < 1 {
		return fmt.Errorf("NAME required")
	}
	name := args[0]
	meta, err := loadMeta(name)
	if err != nil {
		return err
	}
	if meta.MainRepo == "" {
		return fmt.Errorf("%s is not a git VM; nothing to sync", name)
	}
	h, _ := os.UserHomeDir()
	sshCmd := fmt.Sprintf("ssh -F %s -o ControlMaster=no -o ControlPath=none",
		shellQuote(filepath.Join(h, ".lima", name, "ssh.config")))
	cmd := exec.Command("git", "-C", meta.MainRepo, "fetch",
		"lima-"+name+":"+meta.Workdir,
		"+refs/heads/*:refs/wtx/"+name+"/*")
	cmd.Env = append(os.Environ(), "GIT_SSH_COMMAND="+sshCmd)
	cmd.Stdout, cmd.Stderr = os.Stdout, os.Stderr
	if err := cmd.Run(); err != nil {
		return err
	}
	if meta.Branch != "" {
		fmt.Printf("取り込み: git -C %s merge --ff-only refs/wtx/%s/%s\n", meta.Workdir, name, meta.Branch)
	}
	return nil
}
