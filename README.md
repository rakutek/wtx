# wtx

git worktree × コーディングエージェントの並列開発を快適にするRust製CLI／TUI。
worktree ごとに独立したVM（Lima/vz microVM）とVM内専用dockerdを与えるので、
各ブランチが自分のDB・ポート・イメージを持ち、複数エージェントを同時に走らせても衝突しない。
`wtx up --from` で既存VMを clone すれば、**DBデータ（docker volume）やpull済みイメージごと**
新しい worktree に引き継げる。Docker Desktop 不要（Docker Sandboxes のOSS代替としても使える）。
全メカニズムの実機検証記録は [VERIFICATION.md](VERIFICATION.md)（採用しなかった方式と
その失敗理由、摘出したバグまで含む）。

エージェント用スキル（[skills/wtx/SKILL.md](skills/wtx/SKILL.md)）は `npx skills add rakutek/wtx` で導入できる。

```
brew install lima && cargo build --release
wtx mirror install                                 # 任意: レジストリキャッシュ（launchdオンデマンド）
wtx image build                                    # 初回のみ: ゴールデンVM（3〜4分）
wtx up myapp-feature-a ~/repos/myapp-feature-a     # 以後のVM作成は約8秒
wtx exec myapp-feature-a -w ~/repos/myapp-feature-a docker compose up -d --wait
wtx up myapp-feature-b ~/repos/myapp-feature-b --from myapp-feature-a  # DB・イメージごと引き継ぐ
wtx shell myapp-feature-a                          # 中でclaudeも使える
wtx sync myapp-feature-a                           # コミットをホストへ回収
wtx rm myapp-feature-a --with-worktree             # VMとworktreeをまとめて片付ける
wtx ls                                             # 孤児VM（worktree消失）も表示
wtx prune --yes                                    # 孤児VMを掃除（未回収コミットがあればスキップ）
wtx                                                # 引数なしで ratatui コンソール
```

## TUI コンソール（`wtx` / `wtx tui`）

VMを**プロジェクト（ホスト側リポジトリ）ごとにまとめて**表示し、状態・隔離gitの有無・
ミラーの稼働状況を1画面で見て操作する。

```
 wtx   mirror[launchd]  ●docker.io  ●ghcr.io  ●quay.io  ●registry.k8s.io
    NAME                STATUS    GIT       BRANCH
┌ VMs ──────────────────────────────────────────────────────────────────┐
│▶▾ hono-test  ~/dev/hono-test  [2/2 running]                           │
│   books-api           Running   isolated  books-api                   │
│   hono-dev            Running   isolated  main                        │
│ ▾ myapp  ~/repos/myapp  [1/2 running]                                 │
│   myapp-feature-a     Running   isolated  feature-a                   │
│   myapp-feature-b     Stopped   isolated  feature-b                   │
│ ▾ (no project)  [0/1 running]                                         │
│   wtx-golden          Stopped   -                                     │
└───────────────────────────────────────────────────────────────────────┘
r:refresh  s:start/stop  y:sync  Enter:shell/fold  Space:fold  d:delete  q:quit
```

CLIの出力・ヘルプ・TUIのラベルはすべて英語（このREADMEとコード中のコメントは日本語）。

グループ化のキーは `wtx up` 時に記録したメインリポジトリのパス。worktree を複数切っている
プロジェクトはまとめて並び、リポジトリに紐づかないVM（ゴールデンVM など）は末尾に集まる。
見出し行で `Space`（または `Enter` / `←` / `→`）を押すと開閉し、`[稼働数/総数]` だけが残る。

`Enter` はVM行では TUI を畳んでVM内シェルに入り、抜けると復帰する。`--snapshot` を付けると
tty なしで1フレームだけ描画して終了する（動作確認用）。

## 検証

`scripts/check-worktree-lifecycle.sh` が、作成 → VM内コミット → 回収 → 削除 → 孤児検出 →
`prune` までを実VMで通しで検証する（VMを2台作って消すので1〜2分）。
既に孤児VMがあるときは `prune` が既存VMを巻き込む恐れがあるため中止する。
`scripts/check-seed.sh` は `wtx up --from` の引き継ぎ（volume 付け替え・compose での採用・
git の非汚染・clone 元の自動復帰）を実VMで検証する。

設計判断ごとの実機検証記録は [VERIFICATION.md](VERIFICATION.md)。

## 設計

- **microVM隔離**: Lima vz = Apple Virtualization.framework。Docker SandboxesのmicroVMと同じ隔離クラス
- **同パスマウント**: virtiofsでホストと同じ絶対パス。worktreeのホスト直接編集は維持される
- **ゴールデンVM**: `wtx image build` でプロビジョニング済みVMを作り、`wtx up` は `limactl clone`
  するだけ。VM作成が**3〜4分から約8秒**になる（`--no-clone` で毎回プロビジョニング）
- **環境の引き継ぎ（`wtx up --from SRC`）**: ゴールデンVMの代わりに既存VMを clone し、
  docker volume（DBデータ）・pull済みイメージ・導入済みツールをまるごと新VMに乗せる。
  マイグレーション済み・データ投入済みのメインVMから新しい worktree のVMを生やす使い方。
  clone 元は複製の間だけ停止して at-rest のディスクを写し（稼働中コピーの不整合が無い）、
  その後バックグラウンドで自動復帰する（実測ダウンタイム約11秒）。compose の volume 名は
  `<プロジェクト名>_` 接頭辞（既定はディレクトリ名）で worktree ごとに変わるため、
  自動で新しい名前に付け替える（compose ファイルで `name:` を固定していれば接頭辞は
  変わらず、そのまま使われる）。clone 元由来のコンテナと隔離git状態は新VMから除去される
- **隔離git（デフォルト）**: VMは自分専用の index/refs を持ち、VM内コミットがホストの
  ブランチを動かさない。複数VMが同じリポジトリに同時にコミットしても衝突しない。
  仕組み: ホストの `.git` を ro マウントし、VM内で
  (1) それを `/run/wtx/base.git` に ro bind して退避 →
  (2) `--shared` clone でVMローカルの複製を作り（objects は alternates 参照＝コピーゼロ）→
  (3) `.git` のパスに bind で被せる。
  ホストの `.git` は物理的に不変なので、hooks/config の書き換え・ref破壊・gc事故が
  ホストに及ぶことはない。linked worktree と通常リポジトリの両方に適用される。
  旧方式のrw共有は `--share-git`
- **gc保護**: alternates 参照中の object をホストの `git gc` から守るため、`wtx up` 時に
  ホストへ `refs/wtx/keep/<name>/*` を作る（`wtx rm` で削除）
- **コミット回収**: `wtx sync` が `refs/wtx/<name>/*` に fetch する。ホストのブランチは動かさない
- **エージェント認証**: `wtx up` 時にホストの `~/.claude/.credentials.json` を**コピー**（`--no-claude` で無効）。
  マウントにしない理由: `~/.claude` を rw 共有すると VM 内エージェントがホストの settings.json（hooks等）を
  書き換えられ、隔離が破れるため
- **内蔵レジストリキャッシュ**: pull-through キャッシュを自前実装（Docker 不要）。
  blob は digest で不変なのでディスクにキャッシュし、manifest は tag が動くので常に上流へ問い合わせる
  （キャッシュ不整合を構造的に排除）。上流の 401 は `WWW-Authenticate` を解釈してトークンを取得するので、
  docker.io だけでなく ghcr.io / quay.io なども同じ仕組みで配信できる。
  `wtx mirror install` で **launchd ソケットアクティベーション**（常駐プロセスなし。pull が来た瞬間に起動し、
  10分アイドルで終了）。対象と待受ポートは `~/.wtx/mirrors.json` で変更可能。
  ミラーが落ちていても上流直行にフォールバックする。
  **透過的に効くのは docker.io のみ**（後述）
- **ポート**: Limaの自動フォワードは全無効化。複数VMが各自の `localhost:5432` を同時に持てる。
  公開は `wtx forward`（ssh -L）、ホスト常駐サービスへの逆方向は `wtx bridge`（ssh -R）
- **読み取り専用VM**: 追加マウントに `:ro` でreviewer用（書き込みはFSレベルで拒否）
- **VM内ツール**: docker（rootful）+ Node 22 + Claude Code + git（identityはホスト設定から注入）

## オーケストレータ（Orca等）との連携方針

wtxは何にも依存しない。連携はすべて汎用インターフェース経由:

- Orca terminal から `wtx up` / `wtx exec` / `wtx shell` をそのまま呼べる（**exec終了コードは素通し**）
- worker内からホストのruntimeに届かせたいときは `wtx bridge NAME GUEST:HOST`
- 完了通知をファイルで受けるなら共有マウント上に `.result/` を書く運用も可

## 隔離gitモードの運用上の注意

- VMとホストは**同じ作業ツリーを共有しつつ、別々のindex/refsを持つ**。VM内でコミットしても
  ホストのブランチは動かないので、ホスト側 `git status` にはVMの変更が未コミットとして見える。
  回収は `wtx sync` → `git merge --ff-only refs/wtx/<name>/<branch>`
- VMを削除するとVMローカルのコミットも消えるため、**`wtx rm` は未回収コミットがあると拒否する**
  （`wtx sync` してから、または `--force` で破棄）。VMが停止中は問い合わせられないので判定を省く
- **worktree を消してもVMは残る**（gitにフックが無いため連動できない）。`wtx ls` と TUI は
  そうしたVMを `orphaned` と表示し、`wtx prune` でまとめて掃除できる。
  片付けを一度で済ませたいときは `wtx rm NAME --with-worktree`（linked worktree のときだけ畳む。
  通常リポジトリでは本体を消さないよう何もしない）
- 孤児VMからでも `wtx sync` は動く。隔離モードの fetch 元はVMローカル git（`/var/lib/wtx/git/<name>`）で、
  worktree の有無に依存しない
- 通常リポジトリ（非worktree）モードでは、VM起動直後に `wtx-gitmount.service` が bind を張り直すまでの
  ごく短い間だけホストの `.git` がVMから書き込み可能になる。worktree モードは Lima のマウント自体が
  ro なのでこの窓は無い
- launchd の plist には `wtx mirror install` を実行したときの実行パスが焼かれる。
  PATH 上のシンボリックリンク経由で実行すればそのパスが入るので移動に強いが、
  ビルド成果物を直接叩いて登録した場合は `cargo clean` や移動でミラーが起動しなくなる。
  その場合は `wtx mirror install` を再実行する。`~/.wtx/mirrors.json` を編集した場合も
  ソケット一覧を作り直すため再実行が必要

## 既知の制約 / TODO

- **非 Docker Hub レジストリの透過キャッシュは不可（Docker側の制約）**。
  Docker Engine 29 の `registry-mirrors` は Hub 専用で、containerd の
  `/etc/containerd/certs.d/<registry>/hosts.toml` を置いても、システム containerd に切り替えて
  transfer プラグインへ `config_path` を与えても、ghcr.io の pull はミラーに来ないことを
  アクセスログで確認済み（`wtx up` は certs.d を書くので、Docker 側が対応すれば自動で効く）。
  wtx のミラー自体は ghcr/quay でも正常に配信できるので、明示的に
  `docker pull localhost:5002/<org>/<image>` の形なら現時点でも利用できる
- ゴールデンVMには mirrors 設定が焼き込まれる（`wtx up` 時に certs.d は再適用されるが、
  `daemon.json` の Hub ミラーポートを変えた場合は `wtx image rm && wtx image build` が必要）
- `wtx exec` はシェル構文を解釈しない（安全なargv素通し）。パイプ等は `bash -c '...'` で渡す
- clone されたVM（ゴールデン / `--from`）のディスクサイズは clone 元のまま
  （`--disk` は新規プロビジョニング時のみ有効）。`--memory`/`--cpus` は省略すると
  clone 元の値を引き継ぐ
- `--from` の volume 付け替えは `<ディレクトリ名>_` 接頭辞の一致で判定する。
  `COMPOSE_PROJECT_NAME` 環境変数などwtxから見えない方法でプロジェクト名を
  変えている場合は付け替わらない（`docker volume` を手で rename する）
- 資格情報コピーはトークンのスナップショット。VM側でのOAuthリフレッシュがホスト側セッションと
  競合する可能性は未検証
- ミラーのキャッシュ削除（GC）は未実装。`~/.wtx/mirror-cache` を手動で消す

## ライセンス

以下のいずれかのライセンスを選択して利用できる（デュアルライセンス）:

- Apache License, Version 2.0（[LICENSE-APACHE](LICENSE-APACHE) または http://www.apache.org/licenses/LICENSE-2.0）
- MIT License（[LICENSE-MIT](LICENSE-MIT) または http://opensource.org/licenses/MIT）

特に明示しない限り、このリポジトリへ意図的に提出されたコントリビューションは
（Apache-2.0 ライセンスの定義に従い）追加の条件なく上記のデュアルライセンスで
提供されたものとみなす。
