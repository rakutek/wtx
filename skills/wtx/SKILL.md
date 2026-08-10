---
name: wtx
description: >-
  Use the `wtx` CLI to give each git worktree its own microVM (Lima/vz) with a
  dedicated in-VM dockerd, so parallel worktree × coding-agent development does
  not collide on DBs, ports or images. Git, ~/.claude and the ssh-agent are
  shared with the host: commits made in a VM land directly on the host branch
  and `git push` works from inside. A new worktree VM can be seeded from an
  existing one (`wtx up --from`: DB volumes, images and installed tools carry
  over). Each worktree can also get a dedicated iOS simulator device on the
  host (`wtx sim`): its lifecycle follows the VM and agents address it via
  `eval "$(wtx sim env)"` → $WTX_SIM_UDID / $WTX_PORT_*. wtx is a convenience
  tool, NOT a security sandbox — do not use it to contain untrusted code. Use
  when the user says "wtx", "worktreeごとにVM", "worktreeごとにDB",
  "DBを引き継いでworktreeを生やす", "VM内でdocker",
  "VM内でエージェントを走らせる", "ゴールデンVM",
  "レジストリミラー / pull-throughキャッシュ", "worktreeごとにシミュレータ",
  "worktree専用シミュレータ", or wants parallel agents each with their own
  database, ports and iOS simulator. Boundary: use orca-cli when the task is
  about Orca-managed worktrees/terminals/handoffs (Orca can call wtx from its
  terminals); use plain `git worktree` when no per-worktree VM/docker is
  needed.
---

# wtx

git worktree × コーディングエージェントの並列開発のための CLI／TUI。worktree ごとに
独立したVM（Lima vz microVM）＋VM内専用 dockerd を与えるので、各ブランチが自分の
DB・ポート・イメージを持ち、複数エージェントを同時に走らせても衝突しない。
ホストと同じ絶対パスで worktree をマウントするので、ホスト側での直接編集はそのまま使える。
VM内には docker（rootful）+ Node 22 + Claude Code + git が入っている。

git・`~/.claude`・ssh-agent は**ホストと共有**される。VM内のコミットは即ホストのブランチに
乗り、`git push` / `gh` / `claude` はVM内でそのまま使える。wtx は便利ツールであって
**セキュリティサンドボックスではない**（信頼できないコードの封じ込めには使わない）。

コマンドやフラグを暗記や推測で書かないこと。実行前に `wtx --help` /
`wtx <cmd> --help` で確認する。例外は `wtx image` と `wtx mirror` の
ACTION 引数で、これらは `--help` に列挙されないため本ファイルに明記してある。

## セットアップ（初回のみ）

```bash
brew install lima
cargo build --release          # リポジトリ: https://github.com/rakutek/wtx
wtx mirror install             # 任意: レジストリキャッシュ（launchdオンデマンド、常駐なし）
wtx image build                # ゴールデンVM構築（3〜4分）
```

ゴールデンVMがあると以後の `wtx up` は `limactl clone` で約8秒。無いと毎回
3〜4分のフルプロビジョニングに落ちる。

## 基本フロー

```bash
wtx up NAME ~/repos/worktree-dir       # VM作成・起動（worktree自動判別、gitはホストと共有）
wtx up NAME2 ~/repos/dir2 --from NAME  # 既存VMからDB(volume)・イメージごと引き継いで作成
wtx exec NAME -w ~/repos/worktree-dir docker compose up -d --wait
wtx shell NAME                         # 対話シェル（中で claude も使える）
wtx rm NAME [--with-worktree]          # VM削除（DB・イメージごと消える。コミットはホストに残る）
wtx ls                                 # 一覧（worktreeが消えたVMは orphaned と表示）
wtx prune --yes                        # 孤児VMを掃除
wtx                                    # 引数なしで ratatui コンソール
```

- VM内でコミットすると**そのままホストのブランチが進む**。回収の手順（旧 `wtx sync`）は
  存在しない。push もVM内からそのまま実行できる（ホストの ssh-agent に鍵がある場合）。
- TUI はVMを**プロジェクト（`wtx up` 時に記録したメインリポジトリ）ごとにまとめて**表示する。
  見出し行で `Space`/`Enter` を押すと開閉し、`[稼働数/総数]` の要約だけになる。
  VM行では `s` 起動/停止、`d` 削除、`Enter` でシェル。
- `wtx exec` は **argv 素通し**でシェル構文を解釈しない。パイプ・glob・リダイレクトは
  `wtx exec NAME bash -c '...'` の形で渡す。終了コードは素通しされる。
- `wtx up` の主なフラグ: `--from`（既存VMから環境を引き継ぐ）、`--memory/--cpus`
  （省略時は新規 4GiB/2、clone は元の値を引き継ぐ）、`--disk`（新規プロビジョニング時のみ）、
  `--no-claude`（`~/.claude` をマウントしない）、
  `--no-clone`（clone せず新規プロビジョニング。`--from` と排他）。
  追加マウントは位置引数で、`:ro` を付けると読み取り専用。

## 環境の引き継ぎ（`wtx up --from`）

`wtx up NAME WORKDIR --from SRC` はゴールデンVMの代わりに既存VM SRC を clone する。
docker volume（DBデータ）・pull済みイメージ・導入済みツールが新VMに乗るので、
マイグレーション済み・データ投入済みの「メインVM」から新しい worktree のVMを生やすのが基本形。

- SRC は複製の間だけ停止し（約10秒）、バックグラウンドで自動復帰する。`wtx ls` で確認
- compose の volume 名接頭辞（プロジェクト名 = ディレクトリ名）は自動で新側に付け替わる。
  compose ファイルで `name:` を固定している場合は接頭辞が変わらないのでそのまま使われる
- clone 元のコンテナは新VMでは消される（`docker compose up` で作り直す）。
  引き継がれるのは volume とイメージ
- `COMPOSE_PROJECT_NAME` 環境変数でプロジェクト名を変えている場合は付け替え対象にならない

## worktree を消したときの後始末

`git worktree remove` に hook は無いため、**worktree を消してもVMは残る**。
コミットはホストの `.git` に刻まれているので、VMを消しても作業は失われない。

- `wtx ls` / TUI は該当VMを `orphaned` と表示する
- `wtx prune`（dry-run）→ `wtx prune --yes` で孤児VMを削除
- 最初から一度で片付けるなら `wtx rm NAME --with-worktree`

## ポート

Lima の自動フォワードは全無効化されている（複数 VM が各自の `localhost:5432` を持てる）。

```bash
wtx forward NAME 8080:3000    # SPEC は HOST:GUEST — VMのポートをホストへ公開 (ssh -L)
wtx bridge  NAME 9000:9000    # SPEC は GUEST:HOST — ホストのポートをVM内へ露出 (ssh -R)
wtx unforward NAME 8080       # 解除
```

**forward と bridge で SPEC の順序が逆**（forward=HOST:GUEST、bridge=GUEST:HOST）。
間違えやすいので必ず上の対応で書く。

## worktree専用 iOS シミュレータ（wtx sim）

iOSアプリを含むリポジトリでは、worktreeごとに専用のシミュレータデバイスを持てる
（`wtx up --sim` または `wtx sim up`）。デバイスは**ホスト側**にあり、寿命はVMと連動する
（`wtx rm` / `prune` で一緒に消え、`--from` ではアプリ・データごとcloneされる）。

このworktreeでシミュレータを使うときは、次を守ること:

- セッション開始時に worktree ディレクトリで `eval "$(wtx sim env)"` を実行する。
  ポートやUDIDは変わりうるので、セッションをまたいで値をキャッシュしない
- シミュレータは `$WTX_SIM_UDID` のデバイス**だけ**を使う。他のデバイスを作成・起動・削除しない
- ビルド: `xcodebuild -destination "id=$WTX_SIM_UDID" -derivedDataPath .wtx-derived ...`
- 起動前に boot: デバイスは Shutdown で作られ、そのままでは launch が
  `SimError 405` になる。`xcrun simctl boot "$WTX_SIM_UDID" && xcrun simctl bootstatus "$WTX_SIM_UDID" -b`
  で起動を待つ（自分のデバイスの boot はこの節の禁止事項に含まれない）
- 起動: `SIMCTL_CHILD_API_BASE_URL="http://127.0.0.1:$WTX_PORT_API" xcrun simctl launch "$WTX_SIM_UDID" <bundle-id>`
  （`WTX_PORT_<LABEL>` は `wtx sim wire <label>:<VM内ポート>` で払い出したホストポート）
- 操作: orca が使えるなら `orca emulator attach "$WTX_SIM_UDID" --worktree "path:$PWD"`、
  無ければ `xcrun simctl` を直接使う。wtx に操作コマンドは無い
- VM側（db・api・docker）の作業は `wtx exec "$(wtx which)" ...`（`wtx which` は
  カレントディレクトリからVM名を解決する。`wtx sim` 系も NAME 省略で同じ解決が効く）
- シミュレータ操作は**ホスト側でだけ**可能。VM内シェル（`wtx shell` の中）に simctl は
  存在しないので、VM内で頼まれたら実行せずその旨を報告する

`wtx sim env` は死んだ forward（VM再起動後など）を自動で張り直すので、
接続できないときはまず `eval "$(wtx sim env)"` を再実行する。状態確認は
`wtx sim status`（`--json` あり）。

## image / mirror の ACTION（`--help` に出ないので暗記対象）

```bash
wtx image  [status|build|rm]                        # 省略時 status
wtx mirror [status|serve|up|down|install|uninstall] # 省略時 status
```

- ミラーが**透過的に効くのは docker.io のみ**（Docker Engine 側の制約）。
  ghcr.io / quay.io などは `docker pull localhost:5002/<org>/<image>` の明示形なら使える。
- ミラーが落ちていても pull は上流直行にフォールバックする（ビルドは止まらない）。
- `~/.wtx/mirrors.json` を編集したら `wtx mirror install` を再実行する。
- ゴールデンVMには Hub ミラーポート設定と `ssh.forwardAgent` が焼き込まれる。
  変えたら `wtx image rm && wtx image build`。

## エージェント運用のヒント

- tty なしで TUI の状態を確認するには `wtx tui --snapshot`（1フレーム描画して終了）。
  VM 一覧だけなら `wtx ls`。
- `wtx up` はホストの `~/.claude` を VM に**マウント**する（資格情報・settings・skills が
  ホストとライブで一致）。不要なら `--no-claude`。
- VM内からの `git push` はホストの ssh-agent フォワード経由。鍵が agent に無いと失敗する
  （その場合はホスト側で push するか、`ssh-add` で鍵を載せてもらう）。
- 旧バージョンのwtx（隔離gitモード）で作られたVMでは、VM内コミットがホストに現れない。
  `wtx up` での再アタッチ時に警告が出たら、そのVMは作り直す。
- オーケストレータ（Orca 等）からは `wtx up` / `wtx exec` をそのまま呼べばよい。
  worker から ホスト常駐サービスへ届かせるには `wtx bridge`。完了通知は
  共有マウント上のファイル（例: `.result/`）で受ける運用も可。
