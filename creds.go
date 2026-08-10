package main

import (
	"bytes"
	"fmt"
	"os"
	"path/filepath"
)

// copyClaudeCreds はホストの Claude Code 資格情報をVMへコピーする（マウントではなくコピー）。
// コピーにする理由: ~/.claude を rw マウントすると VM 内エージェントがホストの settings.json
// （hooks 等、ホストで実行される設定）を書き換えられ、隔離が破れるため。
func copyClaudeCreds(name string) error {
	h, _ := os.UserHomeDir()
	src := filepath.Join(h, ".claude", ".credentials.json")
	b, err := os.ReadFile(src)
	if err != nil {
		return fmt.Errorf("host credentials not found (%s)", src)
	}
	script := `mkdir -p ~/.claude && cat > ~/.claude/.credentials.json && chmod 600 ~/.claude/.credentials.json`
	return vmScript(name, script, bytes.NewReader(b))
}
