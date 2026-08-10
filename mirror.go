package main

import (
	"context"
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync/atomic"
	"syscall"
	"time"

	"github.com/distribution/distribution/v3/configuration"
	"github.com/distribution/distribution/v3/registry/handlers"
	_ "github.com/distribution/distribution/v3/registry/storage/driver/filesystem"
)

// 内蔵 pull-through レジストリキャッシュ。127.0.0.1 にバインドし、VM からは
// host.lima.internal 経由で届く（LANには公開されない）。
// docker.io は dockerd の registry-mirrors、その他は containerd の
// /etc/containerd/certs.d/<registry>/hosts.toml で透過的に使われる（Docker 29 で検証済み）。

type mirrorEntry struct {
	Registry string // docker.io, ghcr.io, ...
	Port     int
	Upstream string // https://registry-1.docker.io, ...
}

var defaultMirrors = map[string]int{
	"docker.io":       5001,
	"ghcr.io":         5002,
	"quay.io":         5003,
	"registry.k8s.io": 5004,
}

func upstreamFor(registry string) string {
	if registry == "docker.io" {
		return "https://registry-1.docker.io"
	}
	return "https://" + registry
}

// mirrorConfig は ~/.wtx/mirrors.json（{"ghcr.io": 5002, ...}）を読む。無ければ既定値。
func mirrorConfig() []mirrorEntry {
	m := map[string]int{}
	if b, err := os.ReadFile(filepath.Join(wtxHome(), "mirrors.json")); err == nil {
		if err := json.Unmarshal(b, &m); err != nil || len(m) == 0 {
			m = nil
		}
	}
	if len(m) == 0 {
		m = defaultMirrors
	}
	var out []mirrorEntry
	for reg, port := range m {
		out = append(out, mirrorEntry{Registry: reg, Port: port, Upstream: upstreamFor(reg)})
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Registry < out[j].Registry })
	return out
}

// mirrorPort は docker.io 用のポート（daemon.json の registry-mirrors 用）。
func mirrorPort() int {
	if p := os.Getenv("WTX_MIRROR_PORT"); p != "" {
		if n, err := strconv.Atoi(p); err == nil {
			return n
		}
	}
	for _, e := range mirrorConfig() {
		if e.Registry == "docker.io" {
			return e.Port
		}
	}
	return 5001
}

func portAlive(port int) bool {
	c := http.Client{Timeout: 3 * time.Second}
	resp, err := c.Get(fmt.Sprintf("http://127.0.0.1:%d/v2/", port))
	if err != nil {
		return false
	}
	resp.Body.Close()
	return resp.StatusCode < 500
}

func mirrorAlive() bool { return portAlive(mirrorPort()) }

var lastActivity atomic.Int64

func activityTracker(h http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		lastActivity.Store(time.Now().Unix())
		h.ServeHTTP(w, r)
	})
}

func registryHandler(e mirrorEntry) (http.Handler, error) {
	dir := filepath.Join(wtxHome(), "mirror-cache", e.Registry)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return nil, err
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
  remoteurl: %s
`, dir, e.Port, e.Upstream)
	cfg, err := configuration.Parse(strings.NewReader(yml))
	if err != nil {
		return nil, err
	}
	return activityTracker(handlers.NewApp(context.Background(), cfg)), nil
}

func mirrorServe() error {
	entries := mirrorConfig()
	activated := false
	for _, e := range entries {
		h, err := registryHandler(e)
		if err != nil {
			return err
		}
		var ln net.Listener
		if ls, err := launchdListeners(e.Registry); err == nil && len(ls) > 0 {
			ln, activated = ls[0], true
		} else if ln, err = net.Listen("tcp", fmt.Sprintf("127.0.0.1:%d", e.Port)); err != nil {
			fmt.Fprintf(os.Stderr, "wtx: %s のミラーを起動できません: %v\n", e.Registry, err)
			continue
		}
		go func(e mirrorEntry, ln net.Listener, h http.Handler) {
			_ = http.Serve(ln, h)
		}(e, ln, h)
	}
	_ = os.WriteFile(mirrorPidFile(), []byte(strconv.Itoa(os.Getpid())), 0o644)
	lastActivity.Store(time.Now().Unix())

	if activated {
		// launchd 起動時はアイドルで終了する（次のアクセスで launchd が再起動する）
		idle := 10 * time.Minute
		for {
			time.Sleep(time.Minute)
			if time.Since(time.Unix(lastActivity.Load(), 0)) > idle {
				_ = os.Remove(mirrorPidFile())
				return nil
			}
		}
	}
	select {} // 常駐モード
}

func mirrorPidFile() string { return filepath.Join(wtxHome(), "mirror.pid") }

// applyMirrorConfig は certs.d をVMに反映する。ゴールデンVMのビルド後に
// mirrors.json を変えても追随できる。containerd は pull ごとに読むので docker の再起動は不要
// （再起動すると稼働中のDBコンテナが落ちるため、意図的に行わない）。
func applyMirrorConfig(vm string) error {
	var b strings.Builder
	b.WriteString("set -eu\n")
	for _, e := range mirrorConfig() {
		fmt.Fprintf(&b, `sudo mkdir -p /etc/containerd/certs.d/%s
sudo tee /etc/containerd/certs.d/%s/hosts.toml >/dev/null <<'EOF'
server = "%s"

[host."http://host.lima.internal:%d"]
  capabilities = ["pull", "resolve"]
  skip_verify = true
EOF
`, e.Registry, e.Registry, e.Upstream, e.Port)
	}
	return vmScript(vm, b.String(), nil)
}

// mirrorUp は自分自身を `wtx mirror serve` としてデタッチ起動する。冪等。
func mirrorUp() error {
	if mirrorAlive() {
		fmt.Printf("mirror: up (127.0.0.1:%d ほか)\n", mirrorPort())
		return nil
	}
	self, err := os.Executable()
	if err != nil {
		return err
	}
	logf, err := os.OpenFile(filepath.Join(wtxHome(), "mirror.log"), os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o644)
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
			fmt.Printf("mirror: up (127.0.0.1:%d ほか)\n", mirrorPort())
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
	case "install":
		return launchdInstall()
	case "uninstall":
		return launchdUninstall()
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
		mode := "手動 (wtx mirror up)"
		if launchdInstalled() {
			mode = "launchd オンデマンド（アクセス時に起動、10分アイドルで終了）"
		}
		fmt.Println("mode:", mode)
		for _, e := range mirrorConfig() {
			state := "down"
			if portAlive(e.Port) {
				state = "up"
			}
			fmt.Printf("  %-16s :%d  %s\n", e.Registry, e.Port, state)
		}
		return nil
	default:
		return fmt.Errorf("usage: wtx mirror up|down|status|install|uninstall|serve")
	}
}
