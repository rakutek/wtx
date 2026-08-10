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
	CPUs               int
	Memory, Disk       string
	MirrorPort         int
	GitName, GitEmail  string
	Mounts             []mount
}

// instanceMeta は wtx up 時の判断を記録し、sync / rm が参照する。
type instanceMeta struct {
	Workdir  string `json:"workdir"`
	MainRepo string `json:"main_repo,omitempty"`
	Branch   string `json:"branch,omitempty"`
	Isolated bool   `json:"isolated"`
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
	shareGit := fs.Bool("share-git", false, "隔離gitを無効化し、メイン.gitをrw共有（旧方式）")
	noClaude := fs.Bool("no-claude", false, "Claude資格情報をコピーしない")
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
		fmt.Fprintln(os.Stderr, "wtx: warning: mirror is down — pull は Docker Hub 直行になります (wtx mirror up)")
	}

	wt, err := parseWorktree(workdir)
	if err != nil {
		return err
	}
	isolated := wt != nil && !*shareGit

	mounts := []mount{{Location: workdir, Writable: true}}
	if wt != nil {
		if isolated {
			mounts = append(mounts, mount{Location: wt.MainGit, MountPoint: isoBaseGit, Writable: false})
		} else {
			mounts = append(mounts, mount{Location: wt.MainGit, Writable: true})
		}
	}
	seen := map[string]bool{}
	for _, m := range mounts {
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
		mounts = append(mounts, mount{Location: abs, Writable: w})
	}

	cfg := vmConfig{
		CPUs: *cpus, Memory: *memory, Disk: *disk,
		MirrorPort: mirrorPort(),
		GitName:    gitConfigGlobal("user.name", "wtx"),
		GitEmail:   gitConfigGlobal("user.email", "wtx@localhost"),
		Mounts:     mounts,
	}
	tmpl := template.Must(template.New("vm").Funcs(template.FuncMap{"shq": shellQuote}).Parse(vmTemplate))
	yamlPath := filepath.Join(wtxHome(), name+".yaml")
	f, err := os.Create(yamlPath)
	if err != nil {
		return err
	}
	if err := tmpl.Execute(f, cfg); err != nil {
		f.Close()
		return err
	}
	f.Close()

	if err := limactl("start", "--name", name, "--tty=false", yamlPath); err != nil {
		return err
	}

	if isolated {
		if err := setupIsolatedGit(name, wt, workdir); err != nil {
			return fmt.Errorf("isolated git setup: %w", err)
		}
	}
	if !*noClaude {
		if err := copyClaudeCreds(name); err != nil {
			fmt.Fprintln(os.Stderr, "wtx: warning: claude credentials not copied:", err)
		}
	}

	meta := instanceMeta{Workdir: workdir, Isolated: isolated}
	if wt != nil {
		meta.MainRepo = wt.MainRepo
		meta.Branch = wt.Branch
	}
	mb, _ := json.MarshalIndent(meta, "", "  ")
	if err := os.WriteFile(filepath.Join(wtxHome(), name+".json"), mb, 0o644); err != nil {
		return err
	}

	fmt.Printf("ready:\n")
	fmt.Printf("  wtx shell %s\n", name)
	if isolated {
		fmt.Printf("  wtx sync %s        # VM内のコミットをホストへ回収\n", name)
	}
	fmt.Printf("  wtx rm %s\n", name)
	return nil
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
