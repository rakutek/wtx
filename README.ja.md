<div align="center">

# wtx

**どのworktreeにも、同じlocalhostと別々のruntimeを。**

プロジェクトへwtx専用設定を足さず、各エージェントへいつもの`localhost:5432`と
DBごとcloneできる専用runtimeを渡す。

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#ライセンス)
[![Platform: macOS on Apple Silicon](https://img.shields.io/badge/platform-macOS%20on%20Apple%20Silicon-black.svg?logo=apple)](#動作環境)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg?logo=rust)](Cargo.toml)
[![CI](https://github.com/rakutek/wtx/actions/workflows/ci.yml/badge.svg)](https://github.com/rakutek/wtx/actions/workflows/ci.yml)

[English](README.md) | 日本語

</div>

---

同じリポジトリの3ブランチで3つのコーディングエージェントを走らせると、全員が `localhost:5432` を取り合い、全員が同じデーモンへ `docker compose up` し、あるブランチのマイグレーションが別ブランチの検証中の DB を壊す。

wtx は git worktree ごとに専用の microVM（Lima/vz）と VM 内専用の dockerd を与える。
各ブランチは同じlocalhostのポートを使いながら、別々のDB、volume、イメージストアを持つ。
エージェント、編集、Git、資格情報はホストに置き、Docker、DB、service、container依存testだけを
`wtx exec`でVMへ送る。
Docker Desktop は不要。

```text
 wtx   mirror[launchd]  ●docker.io
    NAME                    STATUS        BRANCH          SIM         NOTE
┌ VMs ──────────────────────────────────────────────────────────────────────┐
│▶▾ hono-test  ~/dev/hono-test  [2/2 running]                               │
│   books-api               Running       books-api       sim:Booted        │
│   hono-dev                Running       main                              │
│ ▾ myapp  ~/repos/myapp  [1/2 running]                                     │
│   myapp-feature-a         ⠹ start 8s    feature-a                         │
│   myapp-feature-b         Stopped       feature-b                         │
│ ▾ (no project)  [0/1 running]                                             │
│   wtx-golden              Stopped                                         │
└───────────────────────────────────────────────────────────────────────────┘

 j/k:move  Enter:shell/fold  s:start/stop  d:delete  Space:fold  r:refresh  q:quit
```

## ハイライト

- ⚡ **新しい VM が約8秒**：`wtx up` はプロビジョニング済みのゴールデン VM を clone する（3〜4分が約8秒になる）
- 🌱 **環境ごと引き継ぐ**：`wtx up --from` は既存 VM を clone し、docker volume（DB データ）、pull 済みイメージ、導入済みツールを持ち越す
- 🏠 **ポート設定を増やさない**：どのworktreeも通常の`localhost:5432`を使える。branch別のoffsetやagent向け追加指示が要らない
- 🔀 **回収の儀式なし**：ホストの `.git` を読み書きマウントする。VM 内のコミットは直接ホストのブランチに乗るので、VM を消しても作業が失われる経路がない
- 🔌 **オーケストレータ向け契約**：`wtx ensure --json` はversion付きready receipt、`wtx inspect --json` はruntime/owner状態を返す
- 📦 **内蔵レジストリキャッシュ**：Docker不要のpull-through cache。blobをstream配信し、Rangeと容量上限付き自動GCに対応
- 📱 **worktree 専用 iOS シミュレータ**：`wtx sim` が worktree ごとの専用デバイスを VM と対にし、UDID とポートを環境変数でエージェントに渡す
- 🖥️ **TUI コンソール**：全 VM をプロジェクトごとにまとめ、ミラーの稼働状況とともに1画面で操作する
- ⬆️ **静かな更新確認**：明示実行は `wtx update check`。対話TUIは24時間cacheを使い、通常コマンドでは通信しない

> [!WARNING]
> **wtxが分離するのはruntimeの衝突であり、信頼境界ではない。**
> worktreeと`.git`はホストの読み書きマウントなので、VM内コードはホストから見えるsourceと
> Git metadataを変更できる。agentと資格情報はホストに置き、信頼できないコードの封じ込めには使わない。
> `--agent-access`は、信頼できるVM内agent向けに`~/.claude`とssh-agentを明示共有するoptionである。

境界と資格情報の扱いは[docs/TRUST-MODEL.md](docs/TRUST-MODEL.md)に明記している。

## 動作環境

- Apple Silicon の macOS（vz、つまり Apple Virtualization.framework を使う）
- [Lima](https://lima-vm.io/)（Homebrew formula が自動でインストールする）
- ソースからビルドするための Rust ツールチェイン
- Xcode（`wtx sim` を使う場合のみ）

## インストール

```bash
brew install rakutek/tap/wtx
```

> [!NOTE]
> crates.io の `wtx` クレートは無関係の別プロジェクト。
> ソースからビルドする場合は、このリポジトリを clone して `cargo install --path .` を実行し、
> Lima は `brew install lima` で別途インストールする。

## クイックスタート

```bash
wtx image build       # 初回のみ: ゴールデンVMを構築（3〜4分）
wtx mirror install    # 任意: レジストリキャッシュ（launchdオンデマンド、常駐なし）

# ブランチごとにworktreeを切り、worktreeごとに1 VM。それぞれが自分のdockerd、DB、ポートを持つ
cd ~/repos/myapp
wtx new feature-a     # git worktree add ../myapp-feature-a とVM作成を一発で（約8秒）
cd ../myapp-feature-a
wtx exec -- docker compose up -d --wait
wtx forward 8080:3000 # VMの3000番をホストのlocalhost:8080へ公開

# 2本目のworktreeは1本目のVMから引き継ぐ: DBデータ、イメージ、ツールが乗り移る
cd ~/repos/myapp
wtx new feature-b --from myapp-feature-a

wtx rm myapp-feature-a --with-worktree # VMとlinked worktreeをまとめて片付ける
wtx ls                # VM一覧（worktree消失の孤児VMも表示。--json で機械可読）
wtx prune --yes       # 孤児VMを掃除
wtx                   # 引数なしでTUIコンソール
```

## 仕組み

```mermaid
flowchart LR
    subgraph HOST["macOS host"]
        AG["coding agent · editor · Git"]
        WT["worktree files + .git"]
        MIRROR["bounded registry cache"]
        AG --> WT
    end
    subgraph VMA["microVM: feature-a (Lima/vz)"]
        AD["dockerd<br/>postgres :5432 · images"]
    end
    subgraph VMB["microVM: feature-b (Lima/vz)"]
        BD["dockerd<br/>postgres :5432 · images"]
    end
    WT -->|"same absolute path"| VMA
    WT -->|"same absolute path"| VMB
    AG -->|"wtx exec"| AD
    AG -->|"wtx exec"| BD
    AD -->|pull| MIRROR
    BD -->|pull| MIRROR
```

- **microVM は Lima + vz**（Apple Virtualization.framework）。worktree ごとに専用 dockerd を持つための器であって、セキュリティ境界としては設計していない
- **同パスマウント**：virtiofs でホストと同じ絶対パスにマウントする。worktree をホスト側から直接編集する使い方はそのまま維持される
- **git はホストと共有**：worktree のメイン `.git` を読み書きマウントする。VM 内のコミットはホストのブランチをそのまま動かすので、回収の工程がなく、VM を消しても作業が失われる経路がない。worktree は各自独立した index/HEAD を持つため、複数 VM が同じリポジトリに同時コミットしても衝突しない（2 VM 同時コミットと fsck クリーンを実機検証済み）
- **資格情報は既定でホストに残す**：`~/.claude`はmountせず、ssh-agent forwardingも無効。信頼できるVM内agentで必要な場合だけ作成時に`--agent-access`で両方を明示共有する。このmount policyは作成後に変えず、切り替えにはVMを作り直す
- **ゴールデン VM**：`wtx image build` で一度だけプロビジョニングし、以後の `wtx up` は `limactl clone` するだけ。goldenが無い・古い場合は黙って別環境を作らずエラーにし、fresh構築は`--no-clone`で明示する
- **ポート**：Lima の自動フォワードは全無効化。複数 VM が各自の `localhost:5432` を同時に持てる。公開は `wtx forward`（ssh -L）、ホスト常駐サービスへの逆方向は `wtx bridge`（ssh -R）
- **固定runtime**：Docker Engineとpluginはversionを固定する。agent固有CLIはglobal installしない。git identityはgoldenに焼かず、fresh/clone/再起動の全経路でホスト設定から安全に再注入する

### リソースコスト

各VMの既定値は**RAM 4GiB、CPU 2、disk 20GiB**。cloneはdiskを引き継ぎ、CPU/RAMも明示しなければ
clone元を継承する。Compose project分離より意図的に重いので、runtime stateの完全分離や無変更の
localhostが不要なら、素のworktreeや共有daemonを選ぶ方がよい。

## 機能

### 既存環境からの引き継ぎ（`wtx up --from`）

`wtx up NAME DIR --from SRC` は、ゴールデン VM の代わりに既存 VM を clone する。
docker volume（DB データ込み）、pull 済みイメージ、導入済みツールがまるごと新 VM に乗る。
マイグレーション済みでデータ投入済みのメイン VM から、新しい worktree の VM を生やす使い方が典型になる。

clone 元は複製の間だけ停止して at-rest のディスクを写すので、稼働中コピーの不整合が起きない（実測ダウンタイム約11秒）。
その後はバックグラウンドで自動復帰する。
compose の volume 名は `<プロジェクト名>_` 接頭辞（既定はディレクトリ名）を持ち worktree ごとに変わるため、wtx が自動で新しい名前に付け替える。
compose ファイルで `name:` を固定していれば接頭辞は変わらず、そのまま使われる。
clone 元由来のコンテナは新 VM から除去される。

### 内蔵レジストリキャッシュ

pull-through キャッシュを wtx 自身が実装しているので、動かすのに Docker が要らない。
blobはhit/missとも全量bufferせずstreamし、HEAD/Rangeへ対応する。SHA-256を検証できた完全なblobだけを
保存する。manifestはtagが動くので常に上流へ問い合わせる。Bearer tokenはregistry単位の1個ではなく、
repository scopeごとに保持する。

`wtx mirror install` は **launchd ソケットアクティベーション**を登録する。
常駐プロセスはなく、pull が来た瞬間に起動して10分アイドルで終了する。
既定で起動するのは、Docker Engineが透過利用できるdocker.ioだけ。追加registryは
`~/.wtx/mirrors.json`で明示したlocalhost pull用に限る。cacheは既定20GiBで、書き込み後に古いblobを
自動GCする。`wtx mirror gc --max-gib N`で上限を永続変更し、その場で回収できる。

### worktree 専用 iOS シミュレータ（`wtx sim`）

シミュレータは VM に入らない（CoreSimulator はホストの Xcode に属する）。
そこで wtx はホスト側に worktree 専用デバイス `wtx-NAME` を作り、寿命だけを VM に連動させる。
`wtx up --sim` で作成、`rm` や `prune` で削除、`--from` ではアプリとデータごと clone される。

`wtx sim wire api:3000` で VM 内ポートをホストへ払い出す（42000〜、記録式）。
エージェントは worktree 内で `eval "$(wtx sim env)"` を実行し、`$WTX_SIM_UDID` と `$WTX_PORT_API` を使う。
NAME はどこでも省略可能で、カレントディレクトリから解決される（`wtx which` も同じ）。
外部ツールを使うエージェントは操作直前に `wtx sim env --json` の`sim_udid`を解決し、
そのUDIDまたは検証済みのworktree専用session/windowへ明示的にbindする。先頭・起動中・
focusedのデバイスへ暗黙にfallbackしない。tapなどの操作やツール別adapterはwtxに持たせない。
設計と検証は [docs/DESIGN-sim.md](docs/DESIGN-sim.md) と VERIFICATION.md フェーズ9。

### TUI コンソール（`wtx` / `wtx tui`）

VM を**プロジェクト（ホスト側リポジトリ）ごとにまとめて**表示し、状態とミラーの稼働状況を1画面で見て操作する。
グループ化のキーは `wtx up` 時に記録したメインリポジトリのパス。
worktree を複数切っているプロジェクトはまとめて並び、リポジトリに紐づかない VM（ゴールデン VM など）は末尾に集まる。
見出し行で `Space`（または `Enter`、`←`、`→`）を押すと開閉し、`[稼働数/総数]` だけが残る。
`Enter` は VM 行では TUI を畳んで VM 内シェルに入り、抜けると復帰する。
`--snapshot` を付けると tty なしで1フレームだけ描画して終了する（動作確認用）。

start / stop / delete と状態ポーリングはバックグラウンドで走るので、UI は固まらない。
操作中の VM は STATUS 欄にスピナーと経過秒数を表示し、その間も他の行の操作や終了ができる。

対話TUIはGitHub Releasesも24時間に最大1回だけバックグラウンド確認し、新しい安定版がある場合だけ
`brew upgrade wtx`を表示する。失敗は表示しない。`wtx tui --snapshot`は更新確認も通知表示もしない。

CLI の出力、ヘルプ、TUI のラベルはすべて英語。

### 更新確認

明示的に確認するときは`wtx update check`、agentやscriptからはversion付きmachine-readable結果を返す
`wtx update check --json`を使う。通常コマンドは更新確認の通信をしない。
`WTX_NO_UPDATE_CHECK=1`でTUIのバックグラウンド確認も無効化できる。wtx自身は更新を行わず、
インストールと更新はHomebrewに任せる。

```bash
wtx update check --json
brew upgrade wtx
```

## なぜ既存の方法ではだめか

**素の `git worktree`** が分離するのはファイルであって、ランタイムではない。
どの worktree も一つの dockerd、一つのイメージストア、一つの `localhost:5432` を共有する。
並列エージェントが衝突するのは、まさにその資源である。

**1デーモン上で compose プロジェクトを手で分ける**方法もある。
worktree ごとに `COMPOSE_PROJECT_NAME` とポートのずらし幅を割り当てればよい。
しかしそれは共有デーモンの上に載せたブランチごとの帳簿であり、並列エージェントが既定値のまま `docker compose up` を打った瞬間に破られる。プロジェクト設定やagent指示を変更できるなら、この軽い方法を優先してよい。

**Dev Container**は開発containerを標準化する方法で、同時に使うworkspaceが1つなら有力である。
通常はhost側runtime stateを共有するため、複数worktreeが既定コマンドのまま並列実行するには、やはり名前と
portの規約が必要になる。wtxは編集をhostに残したまま、seed済みDB volumeを含むdockerd全体をcloneする。

**Docker Sandboxes（sbx）** の隔離技術は wtx と同じ（Apple Virtualization.framework の microVM）。
ただし評価した時点では、`sbx create` に Docker アカウントと Subscription Service Agreement への同意が必要だった。
また `--clone` はリポジトリを読み取り専用で渡し、`sandbox-<name>` リモート経由で作業を回収する方式だった。
wtx は OSS スタック（Lima）でアカウント不要、ホストの `.git` を直接共有するので回収の工程がない。
評価記録は [VERIFICATION.md](VERIFICATION.md) フェーズ1。

## コマンド一覧

| コマンド | 内容 |
|---|---|
| `wtx new BRANCH [--dir DIR]` | worktree と VM を一発で作成（ブランチが無ければ作る。`--from` や `--sim` も使える） |
| `wtx up [NAME] [DIR]` | 既存 worktree に VM を作成して起動。引数なしはカレントディレクトリから解決（ゴールデン VM を clone、約8秒） |
| `wtx up NAME DIR --from SRC` | 既存 VM から引き継いで作成（volume、イメージ、ツールが乗り移る） |
| `wtx ensure [NAME] [DIR] [--json]` | VMを冪等に作成・起動してdockerd readyまで待機。owner来歴も記録可 |
| `wtx inspect [NAME] [--json]` | VM/worktreeのready、seed、owner、port、Simulator状態を取得 |
| `wtx exec [--name NAME] [-w DIR] [--tty] -- CMD…` | cwdからVMを解決し、cwdをguest workdirにして実行。旧`NAME CMD…`も受理 |
| `wtx shell [NAME]` | VM内shell。NAME省略時はcwdから解決 |
| `wtx ls [--json]` | VM 一覧（worktree 消失の孤児 VM も表示） |
| `wtx` / `wtx tui` | TUI コンソール（`--snapshot` で tty なし1フレーム描画） |
| `wtx forward [--name NAME] HOST:GUEST` | VM のポートをホストへ公開（ssh -L） |
| `wtx bridge [--name NAME] HOST:GUEST` | hostのHOST番をguestのGUEST番へ届ける（ssh -R）。forwardと同じ順序 |
| `wtx unforward [--name NAME] PORT` | forward / bridge の解除 |
| `wtx stop [NAME]` | VMと起動中のworktree Simulatorを停止 |
| `wtx rm NAME [--if-exists] [--json] [--with-worktree]` | VMを削除。冪等cleanup receipt、またはlinked worktreeの同時削除に対応 |
| `wtx prune [--yes]` | worktree が消えた VM をまとめて削除 |
| `wtx image build\|rm\|status` | ゴールデン VM の管理 |
| `wtx mirror install\|uninstall\|up\|down\|status\|gc` | 容量制限付きレジストリキャッシュの管理 |
| `wtx which` | カレント worktree の VM 名を表示（他コマンドと組み合わせ可） |
| `wtx completions SHELL` | シェル補完を出力（bash, zsh, fish など） |
| `wtx sim up\|status\|wire\|env\|rm` | worktree 専用 iOS シミュレータ |
| `wtx update check [--json]` | GitHub Releasesの新しいversionを確認（インストールはしない） |

## オーケストレータ（Orca 等）との連携方針

wtx は何にも依存しない。監督付きworkerでは、task/worktreeはオーケストレータ、runtimeだけをwtxが所有する。

```bash
wtx ensure worker-a /abs/worktree \
  --owner orca \
  --json
wtx inspect worker-a --json
wtx exec --name worker-a -w /abs/worktree -- docker compose up -d --wait
```

`ensure` は、VMが無ければ作成、停止中なら起動、実行中なら再利用し、dockerd readyまで待つ。
既存VMに対する作成専用の`--from`は再cloneせず、記録済みseedと一致するか検証する。
JSON receiptは`schema_version: 1`を持つ。owner metadataはcleanup・監査用の来歴であり、
wtx自身はtask statusやdispatchを管理しない。
境界とreceipt schemaの詳細は[docs/DESIGN-orchestration.md](docs/DESIGN-orchestration.md)。

Orca/Herdrでは、worktree作成後に`ensure`の成功を待ってからagentを開始する。削除時は先に
`wtx rm NAME --if-exists --json`を成功させ、その後にオーケストレータ側のworktreeを削除する。
agent・編集・Gitはhost、Docker・DB・service・container依存testは`wtx exec`先で実行し、
ComposeはVM内で通常どおり使う。host Dockerへのsilent fallbackは行わない。

その他の連携点:

- Orca terminal から `wtx ensure` / `wtx exec` / `wtx shell` をそのまま呼べる（`wtx exec` の終了コードは素通し）
- hostの9000番をworker内9001番へ届けるなら`wtx bridge --name NAME 9000:9001`
- 完了通知をファイルで受けるなら、共有マウント上に `.result/` を書く運用も可

エージェント用スキル（[skills/wtx/SKILL.md](skills/wtx/SKILL.md)）は次で導入できる。

```bash
npx skills add rakutek/wtx
```

新しいworktreeでは、このスキルがリポジトリ固有setupを優先し、必要な場合だけ同梱scriptで
`.env.example`から欠けている`.env`を生成する。既存`.env`の上書き、別worktreeからのコピー、
secretの推測、動的な`WTX_*`値の保存は行わないため、リポジトリごとのwtx設定は不要。

## 実機検証

wtx の全メカニズムは、実 VM に対する検証を経て採用された。
採用しなかった方式とその失敗理由、途中で摘出したバグまで含めた記録が [VERIFICATION.md](VERIFICATION.md) にある。

主要フローは通しの検証スクリプトで再検証できる。

- `scripts/check-worktree-lifecycle.sh`：作成 → VM 内コミットのホスト直接反映 → 2 VM 同時コミット → 削除 → 孤児検出 → `prune` までを実 VM で通しで検証する（VM を2台作って消すので1〜2分）。既に孤児 VM があるときは、`prune` が既存 VM を巻き込む恐れがあるため中止する
- `scripts/check-seed.sh`：`wtx up --from` の引き継ぎ（volume 付け替え、compose での採用、共有 git の非干渉、clone 元の自動復帰）を実 VM で検証する
- `scripts/check-sim.sh`：`wtx sim` のデバイスライフサイクルを検証する

## 運用上の注意

- **worktree を消しても VM は残る**（git にフックがないため連動できない）。`wtx ls` と TUI はそうした VM を `orphaned` と表示し、`wtx prune --yes` でまとめて掃除できる。コミットはホストの `.git` に刻まれているので、VM を消しても作業は失われない。片付けを一度で済ませたいときは `wtx rm NAME --with-worktree`（linked worktree のときだけ畳む。通常リポジトリでは本体を消さないよう何もしない）
- **旧バージョンの wtx（隔離 git モード）で作った VM** は、VM 内コミットがホストに現れない。`wtx up` で再アタッチすると検知して警告するので、作り直すこと。旧バージョンが残した gc 保護 ref（`refs/wtx/keep/*`）は `wtx rm` がベストエフォートで片付ける
- launchd の plist には `wtx mirror install` を実行したときの実行パスが焼かれる。PATH 上のシンボリックリンク経由で実行すればそのパスが入るので移動に強いが、ビルド成果物を直接叩いて登録した場合は `cargo clean` や移動でミラーが起動しなくなる。その場合は `wtx mirror install` を再実行する。`~/.wtx/mirrors.json` を編集した場合もソケット一覧を作り直すため再実行が必要

## 既知の制約 / TODO

- **非 Docker Hub レジストリの透過キャッシュは不可（Docker 側の制約）**。Docker Engine 29 の `registry-mirrors` は Hub 専用で、containerd のcerts.dを置いてもghcr.ioのpullはミラーに来ないことを実測済み。そのため既定ではHubだけを起動し、効かないcerts.dも書かない。追加設定したendpointは`docker pull localhost:5002/<org>/<image>`の明示形で利用できる
- 資格情報共有をopt-inへ変更する前のVMには、旧`~/.claude` mountとagent forwardingが残っている可能性がある。mount policyは再アタッチで安全に変更できないのでVMを作り直す
- `wtx exec` はシェル構文を解釈しない（argv 素通し）。パイプ等は `bash -c '...'` で渡す
- clone された VM（ゴールデン / `--from`）のディスクサイズは clone 元のまま（`--disk` は新規プロビジョニング時のみ有効）。`--memory` / `--cpus` は省略すると clone 元の値を引き継ぐ
- `--from` の volume 付け替えは `<ディレクトリ名>_` 接頭辞の一致で判定する。`COMPOSE_PROJECT_NAME` 環境変数など wtx から見えない方法でプロジェクト名を変えている場合は付け替わらない（`docker volume` を手で rename する）
- `--agent-access`を明示したVM内からの`git push`は、ホストのssh-agentに鍵が入っているときだけ通る

## ドキュメントの言語

CLI の出力、ヘルプ、TUI のラベルはすべて英語。
README は英語版（[README.md](README.md)）がメインで、このページはその日本語版。
[VERIFICATION.md](VERIFICATION.md) と [docs/DESIGN-sim.md](docs/DESIGN-sim.md)、コード中のコメントは日本語。

## ライセンス

以下のいずれかのライセンスを選択して利用できる（デュアルライセンス）。

- Apache License, Version 2.0（[LICENSE-APACHE](LICENSE-APACHE) または http://www.apache.org/licenses/LICENSE-2.0）
- MIT License（[LICENSE-MIT](LICENSE-MIT) または http://opensource.org/licenses/MIT）

特に明示しない限り、このリポジトリへ意図的に提出されたコントリビューションは（Apache-2.0 ライセンスの定義に従い）追加の条件なく上記のデュアルライセンスで提供されたものとみなす。
