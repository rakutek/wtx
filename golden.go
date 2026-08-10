package main

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

// goldenName はプロビジョニング済みのテンプレートVM。
// wtx up はこれを clone するため、VM作成が数分から数十秒になる。
const goldenName = "wtx-golden"

func limaInstanceDir(name string) string {
	h, _ := os.UserHomeDir()
	return filepath.Join(h, ".lima", name)
}

func limaStatus(name string) string {
	out, err := exec.Command("limactl", "list", name, "--format", "{{.Status}}").Output()
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(out))
}

// goldenUsable はゴールデンVMが clone 可能な状態（存在し、停止済み）かを返す。
func goldenUsable() bool {
	if _, err := os.Stat(filepath.Join(limaInstanceDir(goldenName), "lima.yaml")); err != nil {
		return false
	}
	return limaStatus(goldenName) == "Stopped"
}

func cmdImage(args []string) error {
	sub := "status"
	if len(args) > 0 {
		sub = args[0]
	}
	switch sub {
	case "build":
		if _, err := os.Stat(limaInstanceDir(goldenName)); err == nil {
			return fmt.Errorf("%s は既に存在します（作り直すなら wtx image rm）", goldenName)
		}
		cfg := defaultVMConfig()
		yamlPath := filepath.Join(wtxHome(), goldenName+".yaml")
		if err := renderVMYAML(cfg, yamlPath); err != nil {
			return err
		}
		fmt.Println("ゴールデンVMをビルドしています（初回のみ、3〜4分）...")
		if err := limactl("start", "--name", goldenName, "--tty=false", yamlPath); err != nil {
			return err
		}
		// clone は停止中のインスタンスに対して行う
		if err := limactl("stop", goldenName); err != nil {
			return err
		}
		fmt.Printf("完了: 以後の wtx up は %s を clone します\n", goldenName)
		return nil
	case "rm":
		if err := limactl("delete", "-f", goldenName); err != nil {
			return err
		}
		_ = os.Remove(filepath.Join(wtxHome(), goldenName+".yaml"))
		return nil
	case "status":
		if goldenUsable() {
			fmt.Printf("%s: ready (wtx up は clone で高速起動)\n", goldenName)
		} else if st := limaStatus(goldenName); st != "" {
			fmt.Printf("%s: %s — clone には停止が必要です (limactl stop %s)\n", goldenName, st, goldenName)
		} else {
			fmt.Printf("%s: 未ビルド — `wtx image build` で作成すると VM 作成が数十秒になります\n", goldenName)
		}
		return nil
	default:
		return fmt.Errorf("usage: wtx image build|rm|status")
	}
}
