package main

import (
	"context"
	"fmt"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"

	"github.com/distribution/distribution/v3/configuration"
	"github.com/distribution/distribution/v3/registry"
	_ "github.com/distribution/distribution/v3/registry/storage/driver/filesystem"
)

// 内蔵 pull-through レジストリキャッシュ（docker.io のみ）。
// ホストの 127.0.0.1 にバインドし、VM からは host.lima.internal 経由で届く
// （LANには公開されない。到達性は VERIFICATION.md フェーズ4で確認済み）。

func mirrorPort() int {
	if p := os.Getenv("WTX_MIRROR_PORT"); p != "" {
		if n, err := strconv.Atoi(p); err == nil {
			return n
		}
	}
	return 5001
}

func mirrorAlive() bool {
	c := http.Client{Timeout: 2 * time.Second}
	resp, err := c.Get(fmt.Sprintf("http://127.0.0.1:%d/v2/", mirrorPort()))
	if err != nil {
		return false
	}
	resp.Body.Close()
	return resp.StatusCode < 500
}

func mirrorServe() error {
	dir := filepath.Join(wtxHome(), "mirror-cache")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return err
	}
	yml := fmt.Sprintf(`
version: 0.1
log:
  level: warn
storage:
  filesystem:
    rootdirectory: %s
  delete:
    enabled: true
http:
  addr: 127.0.0.1:%d
proxy:
  remoteurl: https://registry-1.docker.io
`, dir, mirrorPort())
	cfg, err := configuration.Parse(strings.NewReader(yml))
	if err != nil {
		return err
	}
	reg, err := registry.NewRegistry(context.Background(), cfg)
	if err != nil {
		return err
	}
	_ = os.WriteFile(mirrorPidFile(), []byte(strconv.Itoa(os.Getpid())), 0o644)
	return reg.ListenAndServe()
}

func mirrorPidFile() string { return filepath.Join(wtxHome(), "mirror.pid") }

// mirrorUp は自分自身を `wtx mirror serve` としてデタッチ起動する。冪等。
func mirrorUp() error {
	if mirrorAlive() {
		fmt.Printf("mirror: up (http://127.0.0.1:%d)\n", mirrorPort())
		return nil
	}
	self, err := os.Executable()
	if err != nil {
		return err
	}
	logf, err := os.OpenFile(filepath.Join(wtxHome(), "mirror.log"),
		os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o644)
	if err != nil {
		return err
	}
	defer logf.Close()
	cmd := exec.Command(self, "mirror", "serve")
	cmd.Stdout, cmd.Stderr = logf, logf
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true}
	if err := cmd.Start(); err != nil {
		return err
	}
	for i := 0; i < 20; i++ {
		if mirrorAlive() {
			fmt.Printf("mirror: up (http://127.0.0.1:%d)\n", mirrorPort())
			return nil
		}
		time.Sleep(250 * time.Millisecond)
	}
	return fmt.Errorf("mirror failed to start; see %s", filepath.Join(wtxHome(), "mirror.log"))
}

func cmdMirror(args []string) error {
	sub := "status"
	if len(args) > 0 {
		sub = args[0]
	}
	switch sub {
	case "serve":
		return mirrorServe()
	case "up":
		return mirrorUp()
	case "down":
		b, err := os.ReadFile(mirrorPidFile())
		if err != nil {
			return fmt.Errorf("mirror pid file not found")
		}
		pid, _ := strconv.Atoi(strings.TrimSpace(string(b)))
		if pid > 1 {
			_ = syscall.Kill(pid, syscall.SIGTERM)
		}
		_ = os.Remove(mirrorPidFile())
		fmt.Println("mirror: stopped")
		return nil
	case "status":
		if mirrorAlive() {
			fmt.Println("mirror: up")
		} else {
			fmt.Println("mirror: down")
		}
		return nil
	default:
		return fmt.Errorf("usage: wtx mirror up|down|status|serve")
	}
}
