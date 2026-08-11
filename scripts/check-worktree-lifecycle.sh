#!/usr/bin/env bash
# Verify the wtx worktree lifecycle end to end: creation, direct host visibility of in-VM
# commits, deletion, orphan detection, and pruning. Git is shared read-write from the host,
# so commits made in a VM immediately appear on its host branch.
#
# This creates and deletes two real VMs. Abort when orphaned VMs already exist so the prune
# check cannot affect the user's VMs.
# Usage: scripts/check-worktree-lifecycle.sh
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

echo "=== 1. ヘルプ ==="
chk "wtx --help に prune がある"            "$WTX --help | grep -q '^  prune'"
chk "wtx --help に sync が無い"             "! $WTX --help | grep -q '^  sync'"
chk "wtx rm --help に --with-worktree がある" "$WTX rm --help | grep -q -- '--with-worktree'"
chk "wtx rm --help に --force が無い"        "! $WTX rm --help | grep -q -- '--force'"
chk "wtx prune --help に --yes がある"       "$WTX prune --help | grep -q -- '--yes'"
chk "wtx --help に port/env がある"          "$WTX --help | grep -q '^  port' && $WTX --help | grep -q '^  env'"

echo "=== 2. 検証用リポジトリと worktree 2本 ==="
rm -rf "$REPO" "$REPO-a" "$REPO-b"
mkdir -p "$REPO" && cd "$REPO" || exit 1
git init -q -b main && echo base > base.txt && git add -A && git commit -q -m init
git worktree add -q "$REPO-a" -b feat-a
git worktree add -q "$REPO-b" -b feat-b
$WTX up wtxcheck-a "$REPO-a" --agent-access >/dev/null 2>&1 || fail "wtx up wtxcheck-a --agent-access"
$WTX up wtxcheck-b "$REPO-b" >/dev/null 2>&1 || fail "wtx up wtxcheck-b"
chk "VM 2台が起動" "[ \$(limactl list wtxcheck-a wtxcheck-b --format '{{.Status}}' | grep -c Running) -eq 2 ]"
chk "この時点では孤児ではない" "! $WTX ls | grep -q orphaned"

echo "=== 2b. 資格情報共有は既定OFF、明示opt-in ==="
chk "既定VMでは ~/.claude を共有しない" "$WTX exec wtxcheck-b -- bash -c 'test ! -L ~/.claude'"
chk "既定VMでは ssh-agent に到達しない" "$WTX exec wtxcheck-b -- bash -c 'ssh-add -l >/dev/null 2>&1; test \$? -eq 2'"
if [ -d "$HOME/.claude" ]; then
  chk "--agent-access VMでは ~/.claude を共有" "$WTX exec wtxcheck-a -- bash -c 'test -L ~/.claude && test -d ~/.claude/'"
else
  echo "SKIP  ホストに ~/.claude が無いため確認省略"
fi
if [ -n "${SSH_AUTH_SOCK:-}" ]; then
  # `ssh-add -l` returns 0 with keys, 1 without keys, and 2 when the agent is unreachable.
  # This checks forwarding itself, so any result other than 2 succeeds.
  chk "--agent-access VMでは ssh-agent が届く" "$WTX exec wtxcheck-a -- bash -c 'ssh-add -l >/dev/null 2>&1; [ \$? -ne 2 ]'"
else
  echo "SKIP  ホストに SSH_AUTH_SOCK が無いため確認省略"
fi

echo "=== 2c. Simulatorなしのnamed port ==="
$WTX exec wtxcheck-b -- bash -c 'nohup python3 -m http.server 8765 >/dev/null 2>&1 & sleep 1' >/dev/null 2>&1
(cd "$REPO-b" && $WTX port add api:8765 >/dev/null 2>&1) || fail "port add api:8765"
PORT_API=$(cd "$REPO-b" && $WTX env --json | python3 -c 'import json, sys; print(json.load(sys.stdin)["ports"]["api"]["host"])')
chk "env --jsonがhost portを返す"           "test -n '$PORT_API'"
chk "自動割当forwardでVM serviceへ届く"     "curl -s -o /dev/null --max-time 5 http://127.0.0.1:$PORT_API/"
chk "shell envも同じWTX_PORT_APIを返す"     "cd '$REPO-b' && eval \"\$($WTX env)\" && test \"\$WTX_PORT_API\" = '$PORT_API'"

echo "=== 3. TUI がプロジェクト単位でまとめる ==="
$WTX tui --snapshot > /tmp/wtxcheck-tui1.txt 2>&1
chk "TUI にプロジェクト見出しと [2/2 running]" "grep -q 'wtxcheck' /tmp/wtxcheck-tui1.txt && grep -q '2/2 running' /tmp/wtxcheck-tui1.txt"

echo "=== 4. VM内コミットがホストに直接反映される（共有git） ==="
chk "cwd解決execはNAME/-w不要" "cd '$REPO-a' && test \"\$($WTX exec -- pwd)\" = '$REPO-a'"
$WTX exec wtxcheck-a -w "$REPO-a" bash -c 'echo a > a.txt && git add a.txt && git commit -qm "work in VM A"' >/dev/null 2>&1
chk "ホストの feat-a にコミットが見える"   "git -C '$REPO' log --oneline feat-a | grep -q 'work in VM A'"
chk "ホスト側 worktree はクリーン"          "[ -z \"\$(git -C '$REPO-a' status --porcelain)\" ]"
chk "gc保護ref は作られない"                "[ \$(git -C '$REPO' for-each-ref refs/wtx/ | wc -l) -eq 0 ]"

echo "=== 5. 2台のVMから同一リポジトリへ同時コミット（virtiofs 越しの ref ロック） ==="
$WTX exec wtxcheck-a -w "$REPO-a" bash -c 'for i in 1 2 3; do echo a$i > race-a$i.txt && git add . && git commit -qm "race A $i"; done' >/dev/null 2>&1 &
PID_A=$!
$WTX exec wtxcheck-b -w "$REPO-b" bash -c 'for i in 1 2 3; do echo b$i > race-b$i.txt && git add . && git commit -qm "race B $i"; done' >/dev/null 2>&1 &
PID_B=$!
wait $PID_A; RC_A=$?
wait $PID_B; RC_B=$?
chk "両VMのコミットループが成功"    "[ $RC_A -eq 0 ] && [ $RC_B -eq 0 ]"
chk "feat-a に3コミット追加"        "[ \$(git -C '$REPO' log --oneline feat-a | grep -c 'race A') -eq 3 ]"
chk "feat-b に3コミット追加"        "[ \$(git -C '$REPO' log --oneline feat-b | grep -c 'race B') -eq 3 ]"
chk "リポジトリが壊れていない (fsck)" "! git -C '$REPO' fsck --strict 2>&1 | grep -qE 'error|fatal'"

echo "=== 6. rm --with-worktree（syncなしで即消せる） ==="
$WTX rm wtxcheck-a --with-worktree >/dev/null 2>&1
chk "VM が削除された"          "! limactl list wtxcheck-a --format '{{.Name}}' 2>/dev/null | grep -q wtxcheck-a"
chk "worktree も畳まれた"      "[ ! -d '$REPO-a' ]"
chk "コミットはホストに残っている" "git -C '$REPO' log --oneline feat-a | grep -q 'work in VM A'"
chk "メタデータが消えた"        "[ ! -f ~/.wtx/wtxcheck-a.json ]"

echo "=== 7. worktree を外部から消したときの孤児検出 ==="
$WTX exec wtxcheck-b -w "$REPO-b" bash -c 'echo b > b.txt && git add b.txt && git commit -qm "work in VM B"' >/dev/null 2>&1
git -C "$REPO" worktree remove --force "$REPO-b"
chk "wtx ls が orphaned と表示"  "$WTX ls | grep wtxcheck-b | grep -q orphaned"
$WTX tui --snapshot > /tmp/wtxcheck-tui2.txt 2>&1
chk "TUI にも orphaned が出る"   "grep -q 'orphaned' /tmp/wtxcheck-tui2.txt"
chk "コミットは worktree 削除後もホストにある" "git -C '$REPO' log --oneline feat-b | grep -q 'work in VM B'"

echo "=== 8. prune（コミットはホストにあるので無条件に消してよい） ==="
OUT=$($WTX prune 2>&1)
chk "dry-run が削除対象を表示"   "echo '$OUT' | grep -q 'would delete wtxcheck-b'"
chk "dry-run では消えない"       "limactl list wtxcheck-b --format '{{.Name}}' 2>/dev/null | grep -q wtxcheck-b"
OUT=$($WTX prune --yes 2>&1)
chk "prune --yes が削除"         "echo '$OUT' | grep -q 'deleted wtxcheck-b'"
chk "VM が消えた"                "! limactl list wtxcheck-b --format '{{.Name}}' 2>/dev/null | grep -q wtxcheck-b"

echo "=== 9. 後始末 ==="
chk "孤児が無くなった"            "$WTX prune | grep -q 'no orphaned VMs'"
cd ~ && rm -rf "$REPO" "$REPO-a" "$REPO-b"
chk "検証用リポジトリを撤去"      "[ ! -d '$REPO' ]"

echo
if [ "$FAILED" = "0" ]; then echo "==> ALL PASS"; else echo "==> FAILURES あり"; fi
exit $FAILED
