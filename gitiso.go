package main

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

// isoBaseGit は隔離gitモードでホストの .git を ro マウントするVM内パス。
const isoBaseGit = "/run/wtx/base.git"

type worktreeInfo struct {
	MainGit  string // ホストのメイン .git ディレクトリ
	MainRepo string // その親（メインリポジトリ）
	Name     string // .git/worktrees/<Name>
	Branch   string
}

// parseWorktree は workdir が linked worktree なら情報を返す。そうでなければ nil。
func parseWorktree(workdir string) (*worktreeInfo, error) {
	b, err := os.ReadFile(filepath.Join(workdir, ".git"))
	if err != nil {
		return nil, nil // .git がファイルでない（通常リポジトリ or 非リポジトリ）
	}
	line := strings.TrimSpace(string(b))
	gd, ok := strings.CutPrefix(line, "gitdir: ")
	if !ok {
		return nil, fmt.Errorf("unrecognized .git file in %s", workdir)
	}
	if !filepath.IsAbs(gd) {
		gd = filepath.Join(workdir, gd)
	}
	info := &worktreeInfo{
		MainGit:  filepath.Dir(filepath.Dir(gd)),
		Name:     filepath.Base(gd),
	}
	info.MainRepo = filepath.Dir(info.MainGit)
	if hb, err := os.ReadFile(filepath.Join(gd, "HEAD")); err == nil {
		info.Branch = strings.TrimPrefix(strings.TrimSpace(string(hb)), "ref: refs/heads/")
	}
	return info, nil
}

// setupIsolatedGit は VM 内にVMローカルの .git を構築する。
// ホストの .git は isoBaseGit に ro マウント済み。objects は alternates 参照（コピーゼロ）、
// refs はスナップショットコピー。worktree の gitdir ポインタが指す先をVMローカルに再現する。
// ホストの .git は物理的に不変となり、hooks/config 経由のホストコード実行・ref破壊・gc事故が
// 構造的に不可能になる（VERIFICATION.md フェーズ4）。
func setupIsolatedGit(vm string, wt *worktreeInfo, workdir string) error {
	r := strings.NewReplacer(
		"@MAIN@", shellQuote(wt.MainRepo),
		"@WT@", shellQuote(workdir),
		"@N@", shellQuote(wt.Name),
		"@BRANCH@", shellQuote(wt.Branch),
	)
	script := r.Replace(`set -eu
BASE=` + isoBaseGit + `
MAIN=@MAIN@
WT=@WT@
N=@N@
# alternates の有無で「VMローカルgitが構築済みか」を判定する。
# .git が存在するのに alternates がない場合はホストの .git が rw マウントされている
# （隔離が成立しない）ので、黙って共有モードに落ちずに失敗させる。
if [ -e "$MAIN/.git/objects/info/alternates" ]; then exit 0; fi
if [ -e "$MAIN/.git" ]; then
  echo "wtx: $MAIN/.git がVM内に既に存在します（ホストの .git が rw マウントされている可能性）。隔離gitを構築できません" >&2
  exit 1
fi
sudo mkdir -p "$MAIN"
sudo chown "$(id -u):$(id -g)" "$MAIN"
git clone -q --bare --shared "$BASE" "$MAIN/.git"
git -C "$MAIN/.git" config core.bare false
mkdir -p "$MAIN/.git/worktrees/$N"
echo "$WT/.git" > "$MAIN/.git/worktrees/$N/gitdir"
echo "../.." > "$MAIN/.git/worktrees/$N/commondir"
if [ -f "$BASE/worktrees/$N/HEAD" ]; then
  cp "$BASE/worktrees/$N/HEAD" "$MAIN/.git/worktrees/$N/HEAD"
else
  echo "ref: refs/heads/"@BRANCH@ > "$MAIN/.git/worktrees/$N/HEAD"
fi
git -C "$WT" reset -q
`)
	return vmScript(vm, script, nil)
}

// cmdSync は VM 内のブランチ群を refs/wtx/<name>/* としてホストのメインリポジトリへ fetch する。
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
		return fmt.Errorf("%s is not a worktree VM; nothing to sync", name)
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
