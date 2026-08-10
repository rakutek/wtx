# 検証記録: worktreeごとの隔離環境 + ブランチ専用DB

目的: 「git worktree = 隔離VM = 専用DB」の開発フロー（エージェントのYOLO実行を安全にし、ブランチごとにDBを分離する）を実機検証する。

## 前提環境

- macOS 26.3 (Darwin 25.3.0) / Apple Silicon
- Docker (server 28.2.2, ホスト側) 稼働中、ホストの5432は空き
- Node v24.19.0 / npm 11.17.0

## フェーズ1: Docker Sandboxes (sbx) — 中断

- `brew trust docker/tap && brew install docker/tap/sbx` → **sbx 0.38.0 導入成功**
- CLI実仕様の確認結果（ドキュメント未記載だった部分）:
  - `sbx create [flags] AGENT PATH [PATH...]` — AGENTは必須位置引数。`shell`エージェントあり
  - `:ro`サフィックス、`--name`、`-m`、`-p`、`sbx exec SANDBOX CMD` は想定どおり存在
  - `--clone`: host repoをro提供し`sandbox-<name>`リモートで回収、と明記
- `sbx create` 実行 → **`ERROR: Not authenticated to Docker`**。`sbx login`（Dockerアカウント必須、Subscription Service Agreement同意が前提）が必要
- 判断: **Dockerアカウント/ライセンスへの依存を嫌い、OSSスタック（Lima）での再構築に方針転換**。
  隔離強度は同等（sbxのmicroVMもLima vzも同じApple Virtualization.framework）

## フェーズ2: Lima版 (wtx) の設計方針

- worktreeごとにLima VM（vz + virtiofs、ホストと同じパスでマウント）
- VM内に dockerd + Node。DBはVM内の`docker compose up`（`.db-seed/` initdb自動適用は変更なし）
- イメージ重複対策: ホストのpull-throughレジストリキャッシュ + VM側`daemon.json`の`registry-mirrors`
  （sbxでは不可能だった正攻法。save/load不要）
- ポート: Limaの自動フォワードは全ポート無効化（複数VMの5432衝突防止）。公開は`ssh -L`で明示的に
- オーケストレータ連携: `ssh -R`の汎用逆方向ブリッジ（Orca等に非依存、VM内からホストのruntimeに届く）
- Lima 2.2.0 (brew)

## フェーズ2の検証結果（2026-08-10、全項目PASS）

| # | 項目 | 結果 |
|---|---|---|
| 1 | virtiofsでworktree+メイン`.git`を同パスマウントし、VM内で`git status`/空コミットが通る（案1成立条件） | **PASS** — gitdirポインタ解決・共有`.git`へのref/lock書き込みとも成功。コミット`8e8897a`がホストからも見えた |
| 2 | VM内dockerdがホストのレジストリミラー経由でpullできる | **PASS** — `daemon.json`の`registry-mirrors`+`insecure-registries`で成立。ミラーログに67ヒット |
| 3 | `.db-seed/seed.sql`がinitdbで自動適用される | **PASS** — VM内初回`compose up`でmainの2行が再現 |
| 4 | ブランチのマイグレーションがVM内DBにだけ当たり、ホストmain DBが無傷 | **PASS** — `recipes.description`はprobe VMのDBにのみ存在 |
| 5 | 2つのVMが同時に各自の`localhost:5432`を持てる | **PASS** — probe/feature-b同時稼働、データ独立。ミラー温態でのpullは**5.4秒** |
| 6 | `ssh -L`でホストから任意ポートに接続できる | **PASS** — ホスト15432→probe VMの5432でpsql成功 |
| 7 | 読み取り専用マウントのreviewer VMが編集できない | **PASS** — touchもgit commitもRead-only file systemで拒否。`--no-optional-locks`でlog/diffは可 |
| 8 | `ssh -R`ブリッジでVM内からホストのループバック限定サービスに届く | **PASS** — VM内curl→ホスト127.0.0.1:18777のHTTPサーバ応答 |

### 実装上の知見

- 初回`compose up --wait`はhealthcheck未定義だとinitdbの一時再起動中に接続しうる → composeにhealthcheckを書くか`pg_isready`で待つ
- provisionの`usermod -aG docker`はLimaのssh control master確立後だと既存セッションに効かない →
  `ssh -o ControlMaster=no -o ControlPath=none`の新規接続なら有効（wtx execはこの方式）
- git identityはVM内で未設定 → provisionでホストの`git config`から注入する
- VM作成時間: 初回（イメージDL込み）約5分、2台目以降 約2〜3分（get.docker.comのインストールが支配的）

## フェーズ3: wtx CLIへの固め込みとエンドツーエンド確認（PASS）

検証済みメカニズムを`wtx`（`../wtx/wtx`、`/opt/homebrew/bin/wtx`にリンク済み）として実装し、
`scripts/new-env.sh`をsbxからwtxに移植。未検証の新ブランチ`feature-c`で一発構築を実行:

```
scripts/new-env.sh feature-c
# → worktree作成 → pg_dump → wtx up（VM作成 約4分） → VM内で npm install
#   → docker compose up -d --wait（seed自動適用） → drizzle-kit migrate まで自動完走
```

- VM内で`npm run start`（Hono）→ VM内`curl localhost:3000/health` OK
- `wtx forward myapp-feature-c 13000:3000` → ホストから`/health`・`/recipes`OK（mainのseed 2行を返却）
- `wtx exec`の注意: 引数はargvとして安全にクォートされるため、シェル構文は`bash -c '...'`で渡す

## フェーズ4: Go実装と隔離gitモード

wtxをGoで再実装（`../wtx/`）。同時に、フェーズ2で「規約ベース」だった共有`.git`の保護を
構造的な隔離に置き換えるため、方式を比較検証した。

### `.git`保護方式の検証

| 方式 | 結果 |
|---|---|
| overlayfs（lower=ホスト`.git`、upper=VMローカル） | **不成立** — virtiofsマウント上のlowerdirに対し、overlayのupperがroot所有になり
一般ユーザーのgit書き込みが`Permission denied`。fuse-overlayfsもLima環境では`fusermount3: Permission denied` |
| **roマウント＋alternates参照clone（採用）** | **成立** — ホスト`.git`を`/run/wtx/base.git`にroマウントし、
VM内で`git clone --bare --shared`。objectsはalternates参照（コピーゼロ）、refsはスナップショット。
worktreeのgitdirポインタが指す先をVMローカルに再現する |

### 検証結果（feature-e、wtxのコード経由）

| 項目 | 結果 |
|---|---|
| alternatesが`/run/wtx/base.git/objects`を指す | PASS |
| base `.git`への書き込み | PASS（Read-only file systemで拒否） |
| **hooks注入の封じ込め** — VM内で`$MAIN/.git/hooks/post-checkout`を作成 | PASS（VMローカルに落ち、**ホストの`.git/hooks`は不変**。ホストでのコード実行＝VM脱出が構造的に不可能） |
| VM内コミット | PASS |
| `wtx sync`で`refs/wtx/<name>/*`へ回収、**ホストのブランチは不変** | PASS |
| 非sudo docker（グループ反映） | PASS |
| DB seed適用 | PASS |
| Go内蔵ミラー（distributionライブラリ、127.0.0.1バインド、Docker不要） | PASS（pull-through HTTP 200、キャッシュ165MB） |

### 摘出したリグレッション2件（いずれも修正済み）

1. **隔離gitが静かに無効化されていた**（feature-d）: `new-env.sh`が旧仕様のままメイン`.git`を
   追加マウント引数で渡しており、rwマウントされた結果`setupIsolatedGit`の冪等チェックが誤発火。
   共有モードに黙って落ちていた。→ スクリプト修正に加え、wtx側で
   (a) 自動マウント済みパスの重複指定を無視、(b) `.git`があるのにalternatesがない場合は
   **エラーで停止**（黙って共有モードに落ちない）よう修正
2. **Claude資格情報のコピーが空ファイルになっていた**: `vmScript`がスクリプト末尾に改行を付けずに
   stdinデータを連結していたため、`bash -s`がJSONをコマンド行の一部として解釈。
   `cat > file`はEOFで空ファイルを作り、`chmod 600`も失敗（パーミッションが664のまま＝発見の手がかり）。
   → `vmScript`で末尾改行を保証

### 修正後の再検証（feature-e VMを修正版バイナリで作り直して確認）

- 資格情報: ホストとVMの**md5一致**、パーミッション**600** — PASS
- 隔離git: 既存worktreeに対する再構築でもalternatesが正しく設定される — PASS
- `wtx exec`の終了コード素通し（`exit 42` → 42）— PASS（オーケストレータ連携の契約）
- VM再作成後の`docker compose up -d --wait`でseedが再適用され2行を復元 — PASS
- Go版`wtx forward 15432:5432`でホストからDB接続、`wtx unforward`で解放 — PASS
  （`wtx bridge`は`-R`フラグ以外は同一コード経路。bash版＋raw `ssh -R`での到達性はフェーズ2で確認済み）
- 注: VMを作り直すとVM内commitは失われる。`wtx sync`で回収した`refs/wtx/*`はホストに残るが、
  ブランチに取り込むには`git merge --ff-only`が別途必要（refへのfetchはブランチを動かさない設計）

## フェーズ5: 残TODOの解消と Rust への全面移行

### 成果

| 項目 | 結果 |
|---|---|
| **VM作成の高速化** | `wtx image build` でプロビジョニング済みゴールデンVMを作り、`wtx up` は `limactl clone --mount-only`。**3〜4分 → 約8秒**（隔離git構築・資格情報コピー込み） |
| **通常リポジトリの隔離git** | bind mount 方式に統一し、linked worktree と通常リポジトリの両方に適用。VM再起動後は systemd unit が再現 |
| **gc事故対策** | `wtx up` 時にホストへ `refs/wtx/keep/<name>/*` を作成し、alternates 参照中の object を保護（`wtx rm` で削除） |
| **ミラーの常駐廃止** | launchd ソケットアクティベーション（cgo/FFI で `launch_activate_socket`）。**常駐0の状態からアクセスで起動**し、10分アイドルで終了することを確認 |
| **非Hubレジストリ** | **未達（Docker側の制約）**。下記参照 |

### 途中で摘出したバグ2件

1. **mount propagation で退避が壊れる**: `mount --bind` で作った `/run/wtx/base.git` が
   共有マウントの peer になっており、後から `.git` に被せた bind が伝播して**退避したホスト実体まで
   隠していた**（mount table に二重マウントが出る／git が object を読めなくなる）。
   `mount --make-private` で解決
2. **worktreeモードで bind 判定が誤爆**: `mountpoint -q "$GITDIR"` は Lima のマウント地点でも真になるため、
   VMローカルを被せる bind がスキップされていた。マーカーファイル `.wtx-local` による判定に変更

### 非Hubレジストリの透過キャッシュ（できなかったこと）

当初 `/etc/containerd/certs.d` が効いたと判定したが、**ミラーにアクセスログを入れて再検証したところ
これは誤りだった**。ghcr.io の pull はミラーに1リクエストも来ず（docker.io は10リクエスト）、
システム containerd への切り替え＋transfer プラグインの `config_path` 指定でも改善しなかった。
Docker Engine 29 の `registry-mirrors` は Hub 専用というのが結論。
wtx のミラー自体は ghcr/quay でも正常応答する（curl で HTTP 200 を確認済み）。

## フェーズ6: Rust 全面移行 + TUI

Go実装を削除し Rust で再実装（`../wtx/src/`）。

- **レジストリキャッシュを自前実装**（distribution 依存を廃止）。blob は digest で不変なのでキャッシュ、
  manifest は常に上流へ問い合わせ、401 は `WWW-Authenticate` を解釈してトークン取得。
  docker.io の透過キャッシュ（ログで10リクエスト確認、blob 5件/4MB）と ghcr.io の直接配信を確認
- **ratatui コンソール**: VM一覧・状態・隔離gitの有無・ミラー稼働を1画面で表示し、
  起動/停止・sync・削除・シェル起動を操作。tty のない環境でも検証できるよう
  `wtx tui --snapshot`（TestBackend で1フレーム描画）を用意し、描画を確認
### Rust版での最終検証バッテリー（すべてPASS）

| 項目 | 結果 |
|---|---|
| VM作成（worktree / 通常リポジトリ） | 8.25秒 / 7.31秒 |
| 隔離git（alternates が `/run/wtx/base.git/objects`） | 両モードでPASS |
| VM内コミット | `c0cd863`（worktree）/ `df99924`（通常リポジトリ） |
| **hooks注入の封じ込め** | 両モードでPASS（ホストの `.git/hooks` は不変、ブランチも不変） |
| 退避したホスト実体への書き込み | Read-only file system で拒否 |
| **VM再起動後のbind再現**（systemd unit、`wtx up` を挟まずに確認） | PASS（マーカー在り／base.git マウント1件） |
| Claude資格情報 | md5一致・パーミッション600 |
| DB（seed適用） | 2行 |
| `wtx forward` → psql → `wtx unforward` | PASS（rows=2、解除後はポート閉） |
| `wtx exec` の終了コード素通し | `exit 42` → 42（Orca連携の契約） |
| `wtx sync` → ホストのブランチ不変 | PASS |
| TUI描画（`--snapshot`） | PASS（VM一覧・隔離git表示・ミラー状態） |

**未検証**: TUIの対話ループ（キー操作・シェル退避と復帰）。この環境に tty がないため
（`script` が socket stdin で失敗）、実端末での手動確認が残る。

## 決着した設計判断

- **エージェント認証**: ホストの`~/.claude/.credentials.json`を**コピー**（マウントではない）。
  `~/.claude`をrw共有すると、VM内エージェントがホスト側で実行される`settings.json`のhooksを
  書き換えられ隔離が破れるため。`--no-claude`で無効化可
- **レジストリミラー**: Docker上の`registry:2`をやめ、**Goバイナリに内蔵**（distributionライブラリ）。
  Docker Desktop非依存。ミラーが落ちていてもdockerdがHub直行にフォールバックするため常駐は任意
- **`.git`保護**: 隔離gitモードをデフォルト化（`--share-git`で旧方式）
- **VM基盤**: limactlをexecラップ（テンプレートは`go:embed`）

## 残る制約（`../wtx/README.md`に記載）

- VMのobjectsはホスト`.git`をalternates参照するため、回収前のホスト側`git gc`で刈られうる
- workdirが通常リポジトリ（linked worktreeでない）の場合は隔離gitが適用されない
- ミラーはdocker.ioのみ（`registry-mirrors`自体がHub専用）。launchdオンデマンド起動は未実装
- VM作成に3〜4分（provision済みイメージを焼けば短縮可能）
