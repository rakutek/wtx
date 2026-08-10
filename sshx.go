package main

import (
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

// sshBase は Lima の control master を迂回する接続引数を返す。
// 迂回する理由: provision の usermod -aG docker は master 確立後の既存セッションに効かない
// （VERIFICATION.md 参照）。新規接続なら docker グループが有効になる。
func sshBase(name string) []string {
	h, _ := os.UserHomeDir()
	return []string{
		"-F", filepath.Join(h, ".lima", name, "ssh.config"),
		"-o", "ControlMaster=no", "-o", "ControlPath=none",
	}
}

func shellQuote(s string) string {
	return "'" + strings.ReplaceAll(s, "'", `'\''`) + "'"
}

// vmScript は script を VM 内の bash に stdin 経由で流す（クォート事故が起きない）。
func vmScript(name, script string, stdin io.Reader) error {
	args := append(sshBase(name), "lima-"+name, "--", "bash", "-s")
	cmd := exec.Command("ssh", args...)
	// bash -s は stdin を行単位で読むため、script の末尾に改行がないと
	// 後続の stdin データがコマンド行に連結されてしまう（資格情報が空になる）。
	if !strings.HasSuffix(script, "\n") {
		script += "\n"
	}
	if stdin != nil {
		cmd.Stdin = io.MultiReader(strings.NewReader(script), stdin)
	} else {
		cmd.Stdin = strings.NewReader(script)
	}
	cmd.Stdout, cmd.Stderr = os.Stdout, os.Stderr
	return cmd.Run()
}

func cmdExec(args []string) error {
	if len(args) < 1 {
		return fmt.Errorf("usage: wtx exec NAME [-w DIR] CMD...")
	}
	name := args[0]
	args = args[1:]
	dir := ""
	if len(args) >= 2 && args[0] == "-w" {
		dir = args[1]
		args = args[2:]
	}
	if len(args) == 0 {
		return fmt.Errorf("usage: wtx exec NAME [-w DIR] CMD...")
	}
	parts := make([]string, len(args))
	for i, a := range args {
		parts[i] = shellQuote(a)
	}
	remote := strings.Join(parts, " ")
	if dir != "" {
		remote = "cd " + shellQuote(dir) + " && " + remote
	}
	sshArgs := append(sshBase(name), "lima-"+name, "--", remote)
	cmd := exec.Command("ssh", sshArgs...)
	cmd.Stdin, cmd.Stdout, cmd.Stderr = os.Stdin, os.Stdout, os.Stderr
	if err := cmd.Run(); err != nil {
		if ee, ok := err.(*exec.ExitError); ok {
			os.Exit(ee.ExitCode()) // 終了コードを素通しする（オーケストレータ連携の契約）
		}
		return err
	}
	return nil
}

func cmdShell(args []string) error {
	if len(args) < 1 {
		return fmt.Errorf("NAME required")
	}
	name := args[0]
	sshArgs := append(sshBase(name), "-t", "lima-"+name)
	cmd := exec.Command("ssh", sshArgs...)
	cmd.Stdin, cmd.Stdout, cmd.Stderr = os.Stdin, os.Stdout, os.Stderr
	return cmd.Run()
}

func cmdForward(args []string, reverse bool) error {
	if len(args) < 2 {
		return fmt.Errorf("usage: wtx %s NAME A:B", map[bool]string{false: "forward", true: "bridge"}[reverse])
	}
	name, spec := args[0], args[1]
	a, b, ok := strings.Cut(spec, ":")
	if !ok {
		return fmt.Errorf("port spec must be A:B")
	}
	sock := filepath.Join(wtxHome(), name+"-"+a+".sock")
	var fwd string
	if reverse {
		fwd = fmt.Sprintf("%s:127.0.0.1:%s", a, b) // -R: VM内のAをホストのBへ
	} else {
		fwd = fmt.Sprintf("%s:localhost:%s", a, b) // -L: ホストのAをVMのBへ
	}
	flag := map[bool]string{false: "-L", true: "-R"}[reverse]
	sshArgs := append(sshBase(name), "-f", "-N", "-M", "-S", sock, flag, fwd, "lima-"+name)
	if err := exec.Command("ssh", sshArgs...).Run(); err != nil {
		return err
	}
	fmt.Printf("%s %s active (stop: wtx unforward %s %s)\n",
		map[bool]string{false: "forward", true: "bridge"}[reverse], spec, name, a)
	return nil
}

func cmdUnforward(args []string) error {
	if len(args) < 2 {
		return fmt.Errorf("usage: wtx unforward NAME PORT")
	}
	name, port := args[0], args[1]
	sock := filepath.Join(wtxHome(), name+"-"+port+".sock")
	_ = exec.Command("ssh", "-S", sock, "-O", "exit", "lima-"+name).Run()
	_ = os.Remove(sock)
	fmt.Println("stopped")
	return nil
}
