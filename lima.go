package main

import (
	_ "embed"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"text/template"
)

//go:embed template.yaml.tmpl
var vmTemplate string

type mount struct {
	Location   string
	MountPoint string
	Writable   bool
}

type vmConfig struct {
	CPUs              int
	Memory, Disk      string
	MirrorPort        int
	GitName, GitEmail string
	Mounts            []mount
	Mirrors           []mirrorEntry
}

// instanceMeta は wtx up 時の判断を記録し、sync / rm が参照する。
type instanceMeta struct {
	Workdir  string `json:"workdir"`
	MainRepo string `json:"main_repo,omitempty"` // 隔離git時のホスト側リポジトリ
	Branch   string `json:"branch,omitempty"`
	Isolated bool   `json:"isolated"`
	KeepRefs bool   `json:"keep_refs,omitempty"` // ホストに gc 保護 ref を作ったか
}

func wtxHome() string {
	h, _ := os.UserHomeDir()
	d := filepath.Join(h, ".wtx")
	_ = os.MkdirAll(d, 0o755)
	return d
}

func limactl(args ...string) error {
	cmd := exec.Command("limactl", args...)
	cmd.Stdout, cmd.Stderr, cmd.Stdin = os.Stdout, os.Stderr, os.Stdin
	return cmd.Run()
}

func gitConfigGlobal(key, fallback string) string {
	out, err := exec.Command("git", "config", "--global", key).Output()
	if err != nil || len(strings.TrimSpace(string(out))) == 0 {
		return fallback
	}
	return strings.TrimSpace(string(out))
}

func defaultVMConfig() vmConfig {
	return vmConfig{
		CPUs: 2, Memory: "4GiB", Disk: "20GiB",
		MirrorPort: mirrorPort(),
		GitName:    gitConfigGlobal("user.name", "wtx"),
		GitEmail:   gitConfigGlobal("user.email", "wtx@localhost"),
		Mirrors:    mirrorConfig(),
	}
}

func renderVMYAML(cfg vmConfig, path string) error {
	tmpl := template.Must(template.New("vm").Funcs(template.FuncMap{"shq": shellQuote}).Parse(vmTemplate))
	f, err := os.Create(path)
	if err != nil {
		return err
	}
	defer f.Close()
	return tmpl.Execute(f, cfg)
}

// splitFlags は位置引数の後ろに置かれたフラグも拾えるよう並べ替える。
func splitFlags(args []string, valueFlags map[string]bool) (flags, pos []string) {
	for i := 0; i < len(args); i++ {
		a := args[i]
		if strings.HasPrefix(a, "--") {
			flags = append(flags, a)
			name := strings.TrimPrefix(a, "--")
			if valueFlags[name] && !strings.Contains(a, "=") && i+1 < len(args) {
				i++
				flags = append(flags, args[i])
			}
		} else {
			pos = append(pos, a)
		}
	}
	return
}

func cmdUp(args []string) error {
	fs := flag.NewFlagSet("up", flag.ExitOnError)
	memory := fs.String("memory", "4GiB", "VM memory")
	cpus := fs.Int("cpus", 2, "VM CPUs")
	disk := fs.String("disk", "20GiB", "VM disk")
	shareGit := fs.Bool("share-git", false, "隔離gitを無効化し、ホストの.gitをrw共有（旧方式）")
	noClaude := fs.Bool("no-claude", false, "Claude資格情報をコピーしない")
	noClone := fs.Bool("no-clone", false, "ゴールデンVMのcloneを使わず新規プロビジョニングする")
	flagArgs, pos := splitFlags(args, map[string]bool{"memory": true, "cpus": true, "disk": true})
	if err := fs.Parse(flagArgs); err != nil {
		return err
	}
	if len(pos) < 2 {
		return fmt.Errorf("usage: wtx up NAME WORKDIR [MOUNT[:ro]...]")
	}
	name := pos[0]
	workdir, err := filepath.Abs(pos[1])
	if err != nil {
		return err
	}
	if st, err := os.Stat(workdir); err != nil || !st.IsDir() {
		return fmt.Errorf("workdir not found: %s", workdir)
	}

	if !mirrorAlive() {
		fmt.Fprintln(os.Stderr, "wtx: warning: mirror is down — pull は上流に直行します (wtx mirror up)")
	}

	repo, err := inspectRepo(workdir)
	if err != nil {
		return err
	}
	isolated := repo != nil && !*shareGit

	cfg := defaultVMConfig()
	cfg.CPUs, cfg.Memory, cfg.Disk = *cpus, *memory, *disk
	cfg.Mounts = []mount{{Location: workdir, Writable: true}}
	if repo != nil && repo.Kind == repoWorktree {
		// linked worktree: メインの .git は workdir の外にあるので別マウントする
		// （隔離モードでは ro。VMローカルの .git を bind で被せる）
		cfg.Mounts = append(cfg.Mounts, mount{Location: repo.HostGit, Writable: !isolated})
	}
	seen := map[string]bool{}
	for _, m := range cfg.Mounts {
		seen[m.Location] = true
	}
	for _, m := range pos[2:] {
		loc, w := strings.TrimSuffix(m, ":ro"), !strings.HasSuffix(m, ":ro")
		abs, err := filepath.Abs(loc)
		if err != nil {
			return err
		}
		// メイン .git の重複指定は隔離モードを黙って壊すので拒否する
		if seen[abs] {
			fmt.Fprintf(os.Stderr, "wtx: %s は自動でマウント済みのため無視します\n", abs)
			continue
		}
		seen[abs] = true
		cfg.Mounts = append(cfg.Mounts, mount{Location: abs, Writable: w})
	}

	yamlPath := filepath.Join(wtxHome(), name+".yaml")
	if err := renderVMYAML(cfg, yamlPath); err != nil {
		return err
	}

	if st := limaStatus(name); st != "" {
		// 既存インスタンスへの再アタッチ（マウント構成は作成時のもの）
		if st != "Running" {
			if err := limactl("start", name, "--tty=false"); err != nil {
				return err
			}
		}
	} else if !*noClone && goldenUsable() {
		// プロビジョニング済みVMを clone し、マウントだけ差し替えて起動する。
		// clone 後の lima.yaml は解決済み形式なので、テンプレートで上書きはできない
		// （--mount-only 経由で指定する。ゆえに全マウントはホストと同じ絶対パスに置く）。
		cloneArgs := []string{"clone", goldenName, name,
			"--memory", strings.TrimSuffix(cfg.Memory, "GiB"),
			"--cpus", fmt.Sprint(cfg.CPUs)}
		for _, m := range cfg.Mounts {
			spec := m.Location
			if m.Writable {
				spec += ":w"
			}
			cloneArgs = append(cloneArgs, "--mount-only", spec)
		}
		if err := limactl(cloneArgs...); err != nil {
			return err
		}
		if err := limactl("start", name, "--tty=false"); err != nil {
			return err
		}
	} else {
		if !*noClone {
			fmt.Fprintln(os.Stderr, "wtx: ヒント: `wtx image build` でゴールデンVMを作ると以後の作成が数十秒になります")
		}
		if err := limactl("start", "--name", name, "--tty=false", yamlPath); err != nil {
			return err
		}
	}

	if err := applyMirrorConfig(name); err != nil {
		fmt.Fprintln(os.Stderr, "wtx: warning: mirror config not applied:", err)
	}

	meta := instanceMeta{Workdir: workdir, Isolated: isolated}
	if isolated {
		meta.MainRepo, meta.Branch = repo.HostRepo, repo.Branch
		if err := setupIsolatedGit(name, repo, workdir); err != nil {
			return fmt.Errorf("isolated git setup: %w", err)
		}
		// alternates 参照中の object をホスト側 gc から守る
		if err := pinHostObjects(repo.HostRepo, name); err != nil {
			fmt.Fprintln(os.Stderr, "wtx: warning: gc保護refを作成できませんでした:", err)
		} else {
			meta.KeepRefs = true
		}
	} else if repo != nil {
		meta.MainRepo, meta.Branch = repo.HostRepo, repo.Branch
	}
	if !*noClaude {
		if err := copyClaudeCreds(name); err != nil {
			fmt.Fprintln(os.Stderr, "wtx: warning: claude credentials not copied:", err)
		}
	}

	mb, _ := json.MarshalIndent(meta, "", "  ")
	if err := os.WriteFile(filepath.Join(wtxHome(), name+".json"), mb, 0o644); err != nil {
		return err
	}

	fmt.Printf("ready:\n  wtx shell %s\n", name)
	if isolated {
		fmt.Printf("  wtx sync %s        # VM内のコミットをホストへ回収\n", name)
	}
	fmt.Printf("  wtx rm %s\n", name)
	return nil
}

func copyFile(src, dst string) error {
	b, err := os.ReadFile(src)
	if err != nil {
		return err
	}
	return os.WriteFile(dst, b, 0o644)
}

func loadMeta(name string) (*instanceMeta, error) {
	b, err := os.ReadFile(filepath.Join(wtxHome(), name+".json"))
	if err != nil {
		return nil, fmt.Errorf("no metadata for %q (created by an older wtx?): %w", name, err)
	}
	var m instanceMeta
	if err := json.Unmarshal(b, &m); err != nil {
		return nil, err
	}
	return &m, nil
}

func cmdRm(args []string) error {
	if len(args) < 1 {
		return fmt.Errorf("NAME required")
	}
	name := args[0]
	if meta, err := loadMeta(name); err == nil && meta.KeepRefs && meta.MainRepo != "" {
		if err := unpinHostObjects(meta.MainRepo, name); err != nil {
			fmt.Fprintln(os.Stderr, "wtx: warning:", err)
		}
	}
	socks, _ := filepath.Glob(filepath.Join(wtxHome(), name+"-*.sock"))
	for _, s := range socks {
		_ = exec.Command("ssh", "-S", s, "-O", "exit", "lima-"+name).Run()
		_ = os.Remove(s)
	}
	if err := limactl("delete", "-f", name); err != nil {
		return err
	}
	_ = os.Remove(filepath.Join(wtxHome(), name+".yaml"))
	_ = os.Remove(filepath.Join(wtxHome(), name+".json"))
	return nil
}
