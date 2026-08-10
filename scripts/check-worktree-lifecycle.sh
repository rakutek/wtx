#!/usr/bin/env bash
# wtx の worktree ライフサイクル（作成→回収→削除→孤児検出→prune）を通しで検証する。
#
# 実際にVMを2台作って消すため 1〜2分かかる。既に孤児VMがある場合は、prune が
# ユーザーのVMを巻き込む恐れがあるので中止する。
#   使い方: scripts/check-worktree-lifecycle.sh
set -u
WTX=${WTX:-$(command -v wtx 2>/dev/null || echo "$(cd "$(dirname "$0")/.." && pwd)/target/release/wtx")}
REPO=${WTX_CHECK_REPO:-$HOME/wtxcheck}
FAILED=0
pass() { echo "PASS  $1"; }
fail() { echo "FAIL  $1"; FAILED=1; }
chk()  { if eval "$2" >/dev/null 2>&1; then pass "$1"; else fail "$1"; fi; }

echo "=== 0. 既存VMを巻き込まないかの事前ガード ==="
PRE_ORPHANS=$($WTX ls 2>/dev/null | grep -c 'orphaned' || true)
if [ "$PRE_ORPHANS" != "0" ]; then
  echo "ABORT: 既に孤児VMが存在します。prune の検証はユーザーのVMを消す恐れがあるため中止"
  $WTX ls | grep orphaned
  exit 2
fi
pass "既存の孤児VMなし（prune検証を安全に実行できる）"

echo "=== 1. ヘルプに新コマンドが出る ==="
chk "wtx --help に prune がある"            "$WTX --help | grep -q '^  prune'"
chk "wtx rm --help に --with-worktree がある" "$WTX rm --help | grep -q -- '--with-worktree'"
chk "wtx prune --help に --yes/--force がある" "$WTX prune --help | grep -q -- '--yes' && $WTX prune --help | grep -q -- '--force'"

echo "=== 2. 検証用リポジトリと worktree 2本 ==="
rm -rf "$REPO" "$REPO-a" "$REPO-b"
mkdir -p "$REPO" && cd "$REPO" || exit 1
git init -q -b main && echo base > base.txt && git add -A && git commit -q -m init
git worktree add -q "$REPO-a" -b feat-a
git worktree add -q "$REPO-b" -b feat-b
$WTX up wtxcheck-a "$REPO-a" >/dev/null 2>&1 || fail "wtx up wtxcheck-a"
$WTX up wtxcheck-b "$REPO-b" >/dev/null 2>&1 || fail "wtx up wtxcheck-b"
chk "VM 2台が起動" "[ \$(limactl list wtxcheck-a wtxcheck-b --format '{{.Status}}' | grep -c Running) -eq 2 ]"
chk "この時点では孤児ではない" "! $WTX ls | grep -q orphaned"

echo "=== 3. TUI がプロジェクト単位でまとめる ==="
$WTX tui --snapshot > /tmp/wtxcheck-tui1.txt 2>&1
chk "TUI にプロジェクト見出しと [2/2 running]" "grep -q 'wtxcheck' /tmp/wtxcheck-tui1.txt && grep -q '2/2 running' /tmp/wtxcheck-tui1.txt"

echo "=== 4. rm の安全弁（未回収コミット）==="
$WTX exec wtxcheck-a -w "$REPO-a" bash -c 'echo a > a.txt && git add a.txt && git commit -qm "work in VM A"' >/dev/null 2>&1
OUT=$($WTX rm wtxcheck-a --with-worktree 2>&1); RC=$?
chk "未回収コミットがあると rm が失敗する" "[ $RC -ne 0 ]"
chk "エラーが sync を促す"                  "echo '$OUT' | grep -q 'wtx sync wtxcheck-a'"
chk "拒否されたVMは残っている"              "[ \$(limactl list wtxcheck-a --format '{{.Status}}') = Running ]"
chk "拒否時は worktree も残る"              "[ -d '$REPO-a' ]"

echo "=== 5. sync 後は rm --with-worktree が通る ==="
$WTX sync wtxcheck-a >/dev/null 2>&1
chk "コミットが refs/wtx/wtxcheck-a/feat-a に回収された" \
    "git -C '$REPO' rev-parse --verify -q refs/wtx/wtxcheck-a/feat-a"
$WTX rm wtxcheck-a --with-worktree >/dev/null 2>&1
chk "VM が削除された"          "! limactl list wtxcheck-a --format '{{.Name}}' 2>/dev/null | grep -q wtxcheck-a"
chk "worktree も畳まれた"      "[ ! -d '$REPO-a' ]"
chk "gc保護ref が消えた"        "[ \$(git -C '$REPO' for-each-ref refs/wtx/keep/wtxcheck-a/ | wc -l) -eq 0 ]"
chk "メタデータが消えた"        "[ ! -f ~/.wtx/wtxcheck-a.json ]"

echo "=== 6. worktree を外部から消したときの孤児検出 ==="
$WTX exec wtxcheck-b -w "$REPO-b" bash -c 'echo b > b.txt && git add b.txt && git commit -qm "work in VM B"' >/dev/null 2>&1
git -C "$REPO" worktree remove --force "$REPO-b"
chk "wtx ls が orphaned と表示"  "$WTX ls | grep wtxcheck-b | grep -q orphaned"
$WTX tui --snapshot > /tmp/wtxcheck-tui2.txt 2>&1
chk "TUI にも orphaned が出る"   "grep -q 'orphaned' /tmp/wtxcheck-tui2.txt"

echo "=== 7. prune の安全弁 ==="
OUT=$($WTX prune --yes 2>&1)
chk "未回収コミットのある孤児はスキップ" "echo '$OUT' | grep -q 'skip wtxcheck-b'"
chk "スキップされたVMは残っている"      "limactl list wtxcheck-b --format '{{.Name}}' 2>/dev/null | grep -q wtxcheck-b"

echo "=== 8. 孤児VMからでも sync できる ==="
$WTX sync wtxcheck-b >/dev/null 2>&1
chk "worktree が無くてもコミットを回収できた" \
    "git -C '$REPO' rev-parse --verify -q refs/wtx/wtxcheck-b/feat-b"

echo "=== 9. 停止中の孤児も prune が起動して検証・削除する ==="
limactl stop wtxcheck-b >/dev/null 2>&1
OUT=$($WTX prune --yes 2>&1)
chk "停止中VMを起動して検証した"  "echo '$OUT' | grep -q 'starting wtxcheck-b'"
chk "回収済みなので削除された"    "echo '$OUT' | grep -q 'deleted wtxcheck-b'"
chk "VM が消えた"                "! limactl list wtxcheck-b --format '{{.Name}}' 2>/dev/null | grep -q wtxcheck-b"
chk "gc保護ref が消えた"          "[ \$(git -C '$REPO' for-each-ref refs/wtx/keep/wtxcheck-b/ | wc -l) -eq 0 ]"

echo "=== 10. 後始末 ==="
chk "孤児が無くなった"            "$WTX prune | grep -q 'no orphaned VMs'"
cd ~ && rm -rf "$REPO" "$REPO-a" "$REPO-b"
chk "検証用リポジトリを撤去"      "[ ! -d '$REPO' ]"

echo
if [ "$FAILED" = "0" ]; then echo "==> ALL PASS"; else echo "==> FAILURES あり"; fi
exit $FAILED
