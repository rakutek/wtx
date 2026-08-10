#!/usr/bin/env bash
# wtx up --from(環境の引き継ぎ)を実VMで通しで検証する。
#
# 同一メインリポジトリの worktree 2本を使い、volume の付け替え・イメージ引き継ぎ・
# 共有 git の非干渉(新VMのコミットが自分のブランチにだけ乗る)を確認する。
#   使い方: scripts/check-seed.sh   (VMを2台作って消すので数分かかる)
set -u
WTX=${WTX:-$(command -v wtx 2>/dev/null || echo "$(cd "$(dirname "$0")/.." && pwd)/target/release/wtx")}
REPO=${WTX_SEED_REPO:-$HOME/wtxseed}
A=$(basename "$REPO")-a   # VM名 = worktree ディレクトリ名(compose プロジェクト名の検証のため)
B=$(basename "$REPO")-b
FAILED=0
pass() { echo "PASS  $1"; }
fail() { echo "FAIL  $1"; FAILED=1; }
chk()  { if eval "$2" >/dev/null 2>&1; then pass "$1"; else fail "$1"; fi; }

cleanup() {
  $WTX rm "$A" >/dev/null 2>&1
  $WTX rm "$B" >/dev/null 2>&1
  git -C "$REPO" worktree remove --force "$REPO-a" >/dev/null 2>&1
  git -C "$REPO" worktree remove --force "$REPO-b" >/dev/null 2>&1
  rm -rf "$REPO" "$REPO-a" "$REPO-b"
}

echo "=== 0. 事前ガード ==="
for vm in "$A" "$B"; do
  if limactl list "$vm" --format '{{.Name}}' 2>/dev/null | grep -q .; then
    echo "ABORT: VM $vm が既に存在します"
    exit 2
  fi
done
pass "検証用VM名は未使用"

echo "=== 1. ヘルプ ==="
chk "wtx up --help に --from がある" "$WTX up --help | grep -q -- '--from'"
chk "--from と --no-clone は排他"    "! $WTX up x y --from a --no-clone 2>/dev/null"

echo "=== 2. 同一リポジトリの worktree 2本 + compose プロジェクト ==="
cleanup
mkdir -p "$REPO" && cd "$REPO" || exit 1
git init -q -b main
cat > compose.yaml <<'YAML'
services:
  app:
    image: alpine:3.20
    command: sleep 600
    volumes:
      - dbdata:/data
volumes:
  dbdata:
YAML
git add -A && git commit -q -m init
git worktree add -q "$REPO-a" -b seed-a
git worktree add -q "$REPO-b" -b seed-b

echo "=== 3. clone 元VMを作り、DB相当のデータを volume に置く ==="
$WTX up "$A" "$REPO-a" >/dev/null 2>&1 || fail "wtx up $A"
$WTX exec "$A" -w "$REPO-a" docker compose up -d >/dev/null 2>&1 || fail "compose up in $A"
$WTX exec "$A" -w "$REPO-a" docker compose exec -T app sh -c 'echo inherited > /data/seed.txt' \
  >/dev/null 2>&1 || fail "write seed data in $A"
chk "clone 元に volume ${A}_dbdata がある" \
    "$WTX exec $A docker volume inspect ${A}_dbdata"

echo "=== 4. wtx up --from で引き継ぎ ==="
$WTX up "$B" "$REPO-b" --from "$A" || fail "wtx up $B --from $A"
chk "メタに seeded_from が記録される" "grep -q '\"seeded_from\": \"$A\"' ~/.wtx/$B.json"

echo "=== 5. 新VMの docker 状態 ==="
chk "volume が ${B}_dbdata に付け替わった"   "$WTX exec $B docker volume inspect ${B}_dbdata"
chk "旧名 ${A}_dbdata は残っていない"        "! $WTX exec $B docker volume inspect ${A}_dbdata"
chk "clone 元のコンテナは消えている"          "[ -z \"\$($WTX exec $B docker ps -aq 2>/dev/null)\" ]"
chk "イメージは引き継がれている(pull不要)"    "$WTX exec $B docker image inspect alpine:3.20"

echo "=== 6. compose が引き継いだ volume をそのまま使う(本命) ==="
$WTX exec "$B" -w "$REPO-b" docker compose up -d >/dev/null 2>&1 || fail "compose up in $B"
chk "サービスから引き継いだデータが見える" \
    "$WTX exec $B -w '$REPO-b' docker compose exec -T app cat /data/seed.txt | grep -q inherited"

echo "=== 7. 共有 git: 新VMのコミットが自分のブランチにだけ乗る ==="
chk "隔離git残骸(/var/lib/wtx/git)が無い" "! $WTX exec $B test -e /var/lib/wtx/git"
SEED_A_BEFORE=$(git -C "$REPO" rev-parse seed-a)
$WTX exec "$B" -w "$REPO-b" bash -c 'echo b > b.txt && git add b.txt && git commit -qm "work in seeded VM"' \
  >/dev/null 2>&1 || fail "commit in $B"
chk "ホストの seed-b にコミットが直接見える" "git -C '$REPO' log --oneline seed-b | grep -q 'work in seeded VM'"
chk "clone 元のブランチ seed-a は動かない"   "[ \"\$(git -C '$REPO' rev-parse seed-a)\" = '$SEED_A_BEFORE' ]"

echo "=== 8. clone 元VMがバックグラウンドで復帰している ==="
n=0
until [ "$(limactl list "$A" --format '{{.Status}}' 2>/dev/null)" = Running ] || [ $n -ge 60 ]; do
  sleep 2; n=$((n+1))
done
chk "clone 元が Running に戻った"        "[ \"\$(limactl list $A --format '{{.Status}}')\" = Running ]"
chk "clone 元の volume は元の名前のまま" "$WTX exec $A docker volume inspect ${A}_dbdata"

echo "=== 9. 後始末 ==="
cleanup
chk "VMと検証用リポジトリを撤去" "[ ! -d '$REPO' ] && ! limactl list $A --format '{{.Name}}' 2>/dev/null | grep -q ."

echo
if [ "$FAILED" = "0" ]; then echo "==> ALL PASS"; else echo "==> FAILURES あり"; fi
exit $FAILED
