// wtx — git worktree ごとの隔離VM（Lima/vz）+ VM内dockerd + 内蔵レジストリミラー
//
// Docker Sandboxes のOSS代替。設計と検証記録は ../myapp/VERIFICATION.md を参照。
package main

import (
	"fmt"
	"os"
)

const usage = `wtx — worktreeごとの隔離VM (Lima/vz) + VM内dockerd + 内蔵レジストリミラー

  wtx image build|rm|status         プロビジョニング済みゴールデンVM（cloneで高速起動）
  wtx mirror up|down|status|serve   pull-throughレジストリキャッシュ（Goプロセス、Docker不要）
        install|uninstall           launchdオンデマンド起動（常駐プロセスなし）
  wtx up NAME WORKDIR [MOUNT[:ro]...] [flags]
        --share-git   隔離gitを無効化し、ホストの.gitをrw共有（旧方式）
        --no-claude   Claude資格情報のコピーを行わない
        --no-clone    ゴールデンVMを使わず新規プロビジョニング
        --memory 4GiB --cpus 2 --disk 20GiB
  wtx exec NAME [-w DIR] CMD...     VM内で実行（シェル構文は bash -c '...' で）
  wtx shell NAME                    対話シェル
  wtx ls                            VM一覧 (limactl list)
  wtx sync NAME                     VM内のコミットを refs/wtx/NAME/* としてホストにfetch
  wtx forward NAME HOST:GUEST       VMポートをホストへ (ssh -L)
  wtx bridge NAME GUEST:HOST        ホストポートをVMへ (ssh -R、オーケストレータ連携)
  wtx unforward NAME PORT
  wtx stop NAME / wtx rm NAME
`

func main() {
	if len(os.Args) < 2 {
		fmt.Print(usage)
		os.Exit(0)
	}
	var err error
	switch os.Args[1] {
	case "up":
		err = cmdUp(os.Args[2:])
	case "exec":
		err = cmdExec(os.Args[2:])
	case "shell":
		err = cmdShell(os.Args[2:])
	case "ls":
		err = limactl("list")
	case "sync":
		err = cmdSync(os.Args[2:])
	case "forward":
		err = cmdForward(os.Args[2:], false)
	case "bridge":
		err = cmdForward(os.Args[2:], true)
	case "unforward":
		err = cmdUnforward(os.Args[2:])
	case "stop":
		err = requireName(os.Args[2:], func(n string) error { return limactl("stop", n) })
	case "rm":
		err = cmdRm(os.Args[2:])
	case "mirror":
		err = cmdMirror(os.Args[2:])
	case "image":
		err = cmdImage(os.Args[2:])
	case "help", "-h", "--help":
		fmt.Print(usage)
	default:
		fmt.Fprintf(os.Stderr, "wtx: unknown command %q\n%s", os.Args[1], usage)
		os.Exit(2)
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "wtx:", err)
		os.Exit(1)
	}
}

func requireName(args []string, f func(string) error) error {
	if len(args) < 1 {
		return fmt.Errorf("NAME required")
	}
	return f(args[0])
}
