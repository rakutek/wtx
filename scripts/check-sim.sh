#!/usr/bin/env bash
# wtx sim(worktree専用iOSシミュレータ)を実VM・実デバイスで通しで検証する。
#
# デバイス作成のVM連動・NAME省略解決・wire/envと再arm・--fromでのデバイスclone・
# rm時の掃除を確認する。simctlを使うのでmacOS + Xcodeコマンドラインツールが必要。
#   使い方: scripts/check-sim.sh   (VMを2台とデバイスを作って消すので数分かかる)
set -u
WTX=${WTX:-$(command -v wtx 2>/dev/null || echo "$(cd "$(dirname "$0")/.." && pwd)/target/release/wtx")}
BASE=${WTX_SIM_DIR:-$HOME/wtxsimcheck}
A=wtxsim-a
B=wtxsim-b
FAILED=0
pass() { echo "PASS  $1"; }
fail() { echo "FAIL  $1"; FAILED=1; }
chk()  { if eval "$2" >/dev/null 2>&1; then pass "$1"; else fail "$1"; fi; }

cleanup() {
  $WTX rm "$A" >/dev/null 2>&1
  $WTX rm "$B" >/dev/null 2>&1
  rm -rf "$BASE"
}

echo "=== 0. 事前ガード ==="
if ! xcrun simctl help >/dev/null 2>&1; then
  echo "SKIP: xcrun simctl が使えない環境(要 Xcode コマンドラインツール)"
  exit 0
fi
for vm in "$A" "$B"; do
  if limactl list "$vm" --format '{{.Name}}' 2>/dev/null | grep -q .; then
    echo "ABORT: VM $vm が既に存在します"
    exit 2
  fi
done
if xcrun simctl list devices | grep -q "wtx-$A\|wtx-$B"; then
  echo "ABORT: 検証用デバイス wtx-$A / wtx-$B が既に存在します"
  exit 2
fi
pass "検証用のVM名・デバイス名は未使用"

echo "=== 1. ヘルプ ==="
chk "wtx up --help に --sim がある"      "$WTX up --help | grep -q -- '--sim'"
chk "wtx sim --help に wire/env がある"  "$WTX sim --help | grep -q wire && $WTX sim --help | grep -q env"

echo "=== 2. up --sim: VMとデバイスが同時にできる ==="
cleanup
mkdir -p "$BASE/a" "$BASE/b"
UP_LOG=$(mktemp)
$WTX up "$A" "$BASE/a" --sim >"$UP_LOG" 2>&1 || { fail "wtx up $A --sim"; sed 's/^/  | /' "$UP_LOG" | tail -8; }
UDID_A=$(python3 -c "import json; print(json.load(open('$HOME/.wtx/$A.json'))['sim_udid'])" 2>/dev/null)
chk "メタに sim_udid が記録される"       "test -n '$UDID_A'"
chk "デバイス wtx-$A が存在する"          "xcrun simctl list devices | grep -q '$UDID_A'"

echo "=== 3. NAME省略の解決(wtx which / sim系) ==="
chk "worktree直下で which が解決"        "cd '$BASE/a' && test \"\$($WTX which)\" = '$A'"
mkdir -p "$BASE/a/sub/deep"
chk "サブディレクトリでも解決"           "cd '$BASE/a/sub/deep' && test \"\$($WTX which)\" = '$A'"
chk "worktree外では明確なエラー"         "! (cd /tmp && $WTX which 2>/dev/null)"

echo "=== 4. wire: ポート払い出しと実トラフィック ==="
(cd "$BASE/a" && $WTX sim wire api:8765 >/dev/null 2>&1) || fail "sim wire api:8765"
PORT_A=$(cd "$BASE/a" && eval "$($WTX sim env 2>/dev/null)" && echo "${WTX_PORT_API:-}")
chk "ホストポートが払い出される"         "test -n '$PORT_A'"
$WTX exec "$A" bash -c 'nohup python3 -m http.server 8765 >/dev/null 2>&1 & sleep 1' >/dev/null 2>&1
chk "forward 経由でVM内サーバに届く"     "curl -s -o /dev/null --max-time 5 http://localhost:$PORT_A/"

echo "=== 5. VM再起動をまたぐ env の再arm ==="
$WTX stop "$A" >/dev/null 2>&1
limactl start "$A" --tty=false >/dev/null 2>&1
chk "再起動直後は forward が落ちている"  "! curl -s -o /dev/null --max-time 3 http://localhost:$PORT_A/"
(cd "$BASE/a" && eval "$($WTX sim env 2>/dev/null)") || fail "sim env(再arm)"
$WTX exec "$A" bash -c 'nohup python3 -m http.server 8765 >/dev/null 2>&1 & sleep 1' >/dev/null 2>&1
chk "env 後にトラフィックが復帰"         "curl -s -o /dev/null --max-time 5 http://localhost:$PORT_A/"
# 再アタッチ(既存VMへの wtx up)はメタを書き直すので、sim情報が保持されることを確認する
META_BEFORE=$(python3 -c "import json; d=json.load(open('$HOME/.wtx/$A.json')); print(d['sim_udid'], d['ports'])" 2>/dev/null)
$WTX up "$A" "$BASE/a" >/dev/null 2>&1 || fail "wtx up(再アタッチ)"
META_AFTER=$(python3 -c "import json; d=json.load(open('$HOME/.wtx/$A.json')); print(d['sim_udid'], d['ports'])" 2>/dev/null)
chk "再アタッチで sim_udid・ports が保持" "test -n '$META_BEFORE' && test '$META_BEFORE' = '$META_AFTER'"

echo "=== 6. --from: デバイスもデータごと引き継がれる ==="
echo sim-seed-marker > "$HOME/Library/Developer/CoreSimulator/Devices/$UDID_A/data/wtx-check-marker.txt"
$WTX up "$B" "$BASE/b" --from "$A" >"$UP_LOG" 2>&1 || { fail "wtx up $B --from $A"; sed 's/^/  | /' "$UP_LOG" | tail -8; }
UDID_B=$(python3 -c "import json; print(json.load(open('$HOME/.wtx/$B.json'))['sim_udid'])" 2>/dev/null)
chk "cloneされたデバイスは別UDID"        "test -n '$UDID_B' && test '$UDID_B' != '$UDID_A'"
chk "デバイスdataがcloneに引き継がれる"  "grep -q sim-seed-marker '$HOME/Library/Developer/CoreSimulator/Devices/$UDID_B/data/wtx-check-marker.txt'"
PORT_B=$(python3 -c "import json; print(json.load(open('$HOME/.wtx/$B.json'))['ports']['api']['host'])" 2>/dev/null)
chk "ポート定義は引き継ぎ、ホスト側は別" "test -n '$PORT_B' && test '$PORT_B' != '$PORT_A'"

echo "=== 7. 表示(ls / TUI) ==="
chk "wtx ls に sim: が出る"              "$WTX ls | grep -q 'sim:'"
chk "TUI スナップショットに sim: が出る" "$WTX tui --snapshot | grep -q 'sim:'"

echo "=== 8. sim rm と heal ==="
(cd "$BASE/b" && $WTX sim rm >/dev/null 2>&1) || fail "sim rm $B"
chk "デバイスだけ消えVMは残る"           "! xcrun simctl list devices | grep -q '$UDID_B' && limactl list $B --format '{{.Name}}' | grep -q ."
(cd "$BASE/b" && $WTX sim up >/dev/null 2>&1) || fail "sim up(作り直し)"
UDID_B2=$(python3 -c "import json; print(json.load(open('$HOME/.wtx/$B.json'))['sim_udid'])" 2>/dev/null)
chk "sim up で新デバイスができる"        "test -n '$UDID_B2' && xcrun simctl list devices | grep -q '$UDID_B2'"

echo "=== 9. rm: デバイス・メタ・ソケットまで残さない ==="
$WTX rm "$B" >/dev/null 2>&1
$WTX rm "$A" >/dev/null 2>&1
chk "デバイスが消えた"                   "! xcrun simctl list devices | grep -q 'wtx-$A\|wtx-$B'"
chk "メタデータが消えた"                 "! ls $HOME/.wtx/$A.json $HOME/.wtx/$B.json"
chk "forward ソケットが残っていない"     "! ls $HOME/.wtx/$A-*.sock $HOME/.wtx/$B-*.sock"

echo "=== 10. 後始末 ==="
cleanup
pass "検証用ディレクトリを撤去"

if [ "$FAILED" = 0 ]; then echo; echo "==> ALL PASS"; else echo; echo "==> FAILED"; exit 1; fi
