---
name: wtx
description: >-
  Use the `wtx` CLI to give each git worktree its own microVM (Lima/vz) with a
  dedicated in-VM dockerd, so parallel worktree × coding-agent development does
  not collide on DBs, ports or images. A new worktree VM can be seeded from an
  existing one (`wtx up --from`: DB volumes, images and installed tools carry
  over). Use when the user says "wtx", "worktreeごとにVM", "worktreeごとにDB",
  "DBを引き継いでworktreeを生やす", "VM内でdocker",
  "VM内でエージェントを走らせる", "Docker Sandboxesの代替",
  "コミットを回収 / wtx sync", "ゴールデンVM",
  "レジストリミラー / pull-throughキャッシュ", or wants parallel agents each
  with their own database and ports. Boundary: use orca-cli when the task is
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
wtx up NAME ~/repos/worktree-dir       # VM作成・起動（worktree自動判別、隔離git適用）
wtx up NAME2 ~/repos/dir2 --from NAME  # 既存VMからDB(volume)・イメージごと引き継いで作成
wtx exec NAME -w ~/repos/worktree-dir docker compose up -d --wait
wtx shell NAME                         # 対話シェル（中で claude も使える）
wtx sync NAME                          # VM内コミットをホストの refs/wtx/NAME/* へ回収
wtx rm NAME [--with-worktree]          # VM削除（DB・イメージごと消える。worktreeも畳む）
wtx ls                                 # 一覧（worktreeが消えたVMは orphaned と表示）
wtx prune --yes                        # 孤児VMを掃除（未回収コミットがあるVMはスキップ）
wtx                                    # 引数なしで ratatui コンソール
```

- TUI はVMを**プロジェクト（`wtx up` 時に記録したメインリポジトリ）ごとにまとめて**表示する。
  見出し行で `Space`/`Enter` を押すと開閉し、`[稼働数/総数]` の要約だけになる。
  VM行では `s` 起動/停止、`y` sync、`d` 削除、`Enter` でシェル。

- `wtx exec` は **argv 素通し**でシェル構文を解釈しない。パイプ・glob・リダイレクトは
  `wtx exec NAME bash -c '...'` の形で渡す。終了コードは素通しされる。
- `wtx up` の主なフラグ: `--from`（既存VMから環境を引き継ぐ）、`--memory/--cpus`
  （省略時は新規 4GiB/2、clone は元の値を引き継ぐ）、`--disk`（新規プロビジョニング時のみ）、
  `--share-git`（隔離git無効化・旧方式）、`--no-claude`（資格情報コピーなし）、
  `--no-clone`（clone せず新規プロビジョニング。`--from` と排他）。
  追加マウントは位置引数で、`:ro` を付けると読み取り専用（reviewer 用 VM に使う）。

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

`git worktree remove` に hook は無いため、**worktree を消してもVMは残る**。残ったVMはマウント先が
空になり、VM内のコミットが取り残される。

- `wtx ls` / TUI は該当VMを `orphaned` と表示する
- 孤児VMからでも `wtx sync NAME` は動く（fetch 元はVMローカル git で worktree に依存しない）
- `wtx prune --yes` で孤児VMを削除。**未回収コミットがあるVMは自動でスキップ**され、
  `wtx sync` を促す（確認を省くなら `--force`）
- 最初から一度で片付けるなら `wtx rm NAME --with-worktree`
- `wtx rm` は未回収コミットがあると拒否する（`--force` で破棄）

## 隔離 git の運用（データ消失に直結する注意）

デフォルトでホストの `.git` は ro マウントされ、VM は自分専用の index/refs を持つ
（objects は alternates 参照でコピーゼロ）。ホストへの hooks/config 注入による
VM 脱出は worktree モードでは構造的に不可能。通常リポジトリ（非 worktree）モードのみ、
VM 起動直後に `wtx-gitmount.service` が bind を張るまでのごく短い間、ホストの `.git` が
VM から書き込み可能になる窓がある。運用上の帰結:

- **`wtx rm` の前に必ず `wtx sync`**。VM ローカルのコミットは VM と一緒に消える。
- VM 内でコミットしてもホストのブランチは動かない。ホスト側 `git status` には
  VM の変更が**未コミットとして見える**が、これは正常。ホストで作業しているときに
  この表示を「未保存の変更」と誤読しないこと。
- 回収手順: `wtx sync NAME` → ホストで `git merge --ff-only refs/wtx/NAME/<branch>`。
  ホストのブランチを勝手に動かす仕組みは無い。
- `wtx up` 時にホストへ gc 保護 ref `refs/wtx/keep/NAME/*` が作られ、`wtx rm` で消える。

## ポート

Lima の自動フォワードは全無効化されている（複数 VM が各自の `localhost:5432` を持てる）。

```bash
wtx forward NAME 8080:3000    # SPEC は HOST:GUEST — VMのポートをホストへ公開 (ssh -L)
wtx bridge  NAME 9000:9000    # SPEC は GUEST:HOST — ホストのポートをVM内へ露出 (ssh -R)
wtx unforward NAME 8080       # 解除
```

**forward と bridge で SPEC の順序が逆**（forward=HOST:GUEST、bridge=GUEST:HOST）。
間違えやすいので必ず上の対応で書く。

## image / mirror の ACTION（`--help` に出ないので暗記対象）

```bash
wtx image  [status|build|rm]                        # 省略時 status
wtx mirror [status|serve|up|down|install|uninstall] # 省略時 status
```

- ミラーが**透過的に効くのは docker.io のみ**（Docker Engine 側の制約）。
  ghcr.io / quay.io などは `docker pull localhost:5002/<org>/<image>` の明示形なら使える。
- ミラーが落ちていても pull は上流直行にフォールバックする（ビルドは止まらない）。
- `~/.wtx/mirrors.json` を編集したら `wtx mirror install` を再実行する。
- ゴールデンVMには Hub ミラーポート設定が焼き込まれる。`daemon.json` のポートを
  変えたら `wtx image rm && wtx image build`。

## エージェント運用のヒント

- tty なしで TUI の状態を確認するには `wtx tui --snapshot`（1フレーム描画して終了）。
  VM 一覧だけなら `wtx ls`。
- `wtx up` はホストの `~/.claude/.credentials.json` を VM へ**コピー**する
  （マウントではない。トークンのスナップショット）。不要なら `--no-claude`。
- オーケストレータ（Orca 等）からは `wtx up` / `wtx exec` をそのまま呼べばよい。
  worker から ホスト常駐サービスへ届かせるには `wtx bridge`。完了通知は
  共有マウント上のファイル（例: `.result/`）で受ける運用も可。
