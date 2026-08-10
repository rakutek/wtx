//go:build darwin

package main

/*
#include <stddef.h>
#include <stdlib.h>
extern int launch_activate_socket(const char *name, int **fds, size_t *cnt);
*/
import "C"

import (
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"unsafe"
)

const launchdLabel = "com.wtx.mirror"

func launchdPlistPath() string {
	h, _ := os.UserHomeDir()
	return filepath.Join(h, "Library", "LaunchAgents", launchdLabel+".plist")
}

func launchdInstalled() bool {
	_, err := os.Stat(launchdPlistPath())
	return err == nil
}

// launchdListeners は launchd から渡されたソケットを受け取る。
// launchd 管理下でなければエラーを返すので、呼び出し側は net.Listen にフォールバックする。
func launchdListeners(name string) ([]net.Listener, error) {
	cname := C.CString(name)
	defer C.free(unsafe.Pointer(cname))
	var fds *C.int
	var cnt C.size_t
	if rc := C.launch_activate_socket(cname, &fds, &cnt); rc != 0 {
		return nil, fmt.Errorf("launch_activate_socket(%s): %d", name, int(rc))
	}
	defer C.free(unsafe.Pointer(fds))
	var out []net.Listener
	for _, fd := range unsafe.Slice(fds, int(cnt)) {
		f := os.NewFile(uintptr(fd), "launchd-socket-"+name)
		ln, err := net.FileListener(f)
		if err != nil {
			return nil, err
		}
		out = append(out, ln)
	}
	return out, nil
}

// launchdInstall はソケットアクティベーションを登録する。
// 常駐プロセスは無くなり、VM からの pull が来た瞬間だけ wtx が起動する。
func launchdInstall() error {
	self, err := os.Executable()
	if err != nil {
		return err
	}
	var sockets strings.Builder
	for _, e := range mirrorConfig() {
		fmt.Fprintf(&sockets, `    <key>%s</key>
    <dict>
      <key>SockNodeName</key><string>127.0.0.1</string>
      <key>SockServiceName</key><string>%d</string>
      <key>SockType</key><string>stream</string>
    </dict>
`, e.Registry, e.Port)
	}
	log := filepath.Join(wtxHome(), "mirror.log")
	plist := fmt.Sprintf(`<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>%s</string>
  <key>ProgramArguments</key>
  <array>
    <string>%s</string><string>mirror</string><string>serve</string>
  </array>
  <key>Sockets</key>
  <dict>
%s  </dict>
  <key>StandardOutPath</key><string>%s</string>
  <key>StandardErrorPath</key><string>%s</string>
</dict>
</plist>
`, launchdLabel, self, sockets.String(), log, log)

	path := launchdPlistPath()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	// 既存の常駐プロセスが同じポートを掴んでいると bootstrap が失敗する
	if b, err := os.ReadFile(mirrorPidFile()); err == nil {
		_ = exec.Command("kill", strings.TrimSpace(string(b))).Run()
		_ = os.Remove(mirrorPidFile())
	}
	_ = exec.Command("launchctl", "bootout", fmt.Sprintf("gui/%d/%s", os.Getuid(), launchdLabel)).Run()
	if err := os.WriteFile(path, []byte(plist), 0o644); err != nil {
		return err
	}
	out, err := exec.Command("launchctl", "bootstrap", fmt.Sprintf("gui/%d", os.Getuid()), path).CombinedOutput()
	if err != nil {
		return fmt.Errorf("launchctl bootstrap: %v: %s", err, strings.TrimSpace(string(out)))
	}
	fmt.Println("mirror: launchd オンデマンド起動を登録しました（常駐プロセスなし）")
	return nil
}

func launchdUninstall() error {
	_ = exec.Command("launchctl", "bootout", fmt.Sprintf("gui/%d/%s", os.Getuid(), launchdLabel)).Run()
	if err := os.Remove(launchdPlistPath()); err != nil && !os.IsNotExist(err) {
		return err
	}
	fmt.Println("mirror: launchd 登録を解除しました")
	return nil
}
