# wtx

git worktree × コーディングエージェントの並列開発を快適にするRust製CLI／TUI。
worktree ごとに独立したVM（Lima/vz microVM）とVM内専用dockerdを与えるので、
各ブランチが自分のDB・ポート・イメージを持ち、複数エージェントを同時に走らせても衝突しない。
`wtx up --from` で既存VMを clone すれば、**DBデータ（docker volume）やpull済みイメージごと**
新しい worktree に引き継げる。git・`~/.claude`・ssh-agent はホストと共有なので、
VM内のコミットは即ホストのブランチに乗り、VM内から `git push` も `claude` もそのまま使える。
Docker Desktop 不要。

wtxは**セキュリティサンドボックスではない**。VMで分かれているのは docker・ポート・
プロセス空間であり、権限境界ではない（VM内プロセスはホストの `.git` や `~/.claude` に
書けるし、ssh-agent も使える）。信頼できないコードやエージェントを閉じ込める用途には
使わないこと。

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
wtx shell myapp-feature-a                          # 中でclaudeも使える（設定・認証はホストと共有）
wtx rm myapp-feature-a --with-worktree             # VMとworktreeをまとめて片付ける
wtx ls                                             # 孤児VM（worktree消失）も表示
wtx prune --yes                                    # 孤児VMを掃除
wtx                                                # 引数なしで ratatui コンソール
```

## TUI コンソール（`wtx` / `wtx tui`）

VMを**プロジェクト（ホスト側リポジトリ）ごとにまとめて**表示し、状態と
ミラーの稼働状況を1画面で見て操作する。

```
 wtx   mirror[launchd]  ●docker.io  ●ghcr.io  ●quay.io  ●registry.k8s.io
    NAME                STATUS    BRANCH
┌ VMs ──────────────────────────────────────────────────────────────────┐
│▶▾ hono-test  ~/dev/hono-test  [2/2 running]                           │
│   books-api           Running   books-api                             │
│   hono-dev            Running   main                                  │
│ ▾ myapp  ~/repos/myapp  [1/2 running]                                 │
│   myapp-feature-a     Running   feature-a                             │
│   myapp-feature-b     Stopped   feature-b                             │
│ ▾ (no project)  [0/1 running]                                         │
│   wtx-golden          Stopped                                         │
└───────────────────────────────────────────────────────────────────────┘
r:refresh  s:start/stop  Enter:shell/fold  Space:fold  d:delete  q:quit
```

CLIの出力・ヘルプ・TUIのラベルはすべて英語（このREADMEとコード中のコメントは日本語）。

グループ化のキーは `wtx up` 時に記録したメインリポジトリのパス。worktree を複数切っている
プロジェクトはまとめて並び、リポジトリに紐づかないVM（ゴールデンVM など）は末尾に集まる。
見出し行で `Space`（または `Enter` / `←` / `→`）を押すと開閉し、`[稼働数/総数]` だけが残る。

`Enter` はVM行では TUI を畳んでVM内シェルに入り、抜けると復帰する。`--snapshot` を付けると
tty なしで1フレームだけ描画して終了する（動作確認用）。

## 検証

`scripts/check-worktree-lifecycle.sh` が、作成 → VM内コミットのホスト直接反映 →
2VM同時コミット → 削除 → 孤児検出 → `prune` までを実VMで通しで検証する
（VMを2台作って消すので1〜2分）。既に孤児VMがあるときは `prune` が既存VMを
巻き込む恐れがあるため中止する。
`scripts/check-seed.sh` は `wtx up --from` の引き継ぎ（volume 付け替え・compose での採用・
共有gitの非干渉・clone 元の自動復帰）を実VMで検証する。

設計判断ごとの実機検証記録は [VERIFICATION.md](VERIFICATION.md)。

## 設計

- **microVM**: Lima vz = Apple Virtualization.framework。worktree ごとに専用 dockerd を
  持つための器であって、セキュリティ境界としては設計していない
- **同パスマウント**: virtiofsでホストと同じ絶対パス。worktreeのホスト直接編集は維持される
- **git はホストと共有**: worktree のメイン `.git` を rw マウントする。VM内コミットは
  ホストのブランチをそのまま動かすので、回収の儀式は無く、**VMを消しても作業が失われる
  経路が無い**。worktree は各自独立した index/HEAD を持つため、複数VMが同じリポジトリに
  同時コミットしても衝突しない（2VM同時コミット・fsckクリーンを実機検証済み）
- **`~/.claude` はマウント共有**: 資格情報・settings.json・skills がホストとライブで一致し、
  VM側でのトークンリフレッシュもホストとずれない。ホスト側パスに virtiofs マウントし、
  ゲストの `~/.claude` から symlink を張る。無効化は `--no-claude`
- **ssh-agent フォワード**: VM内から `git push` / `gh` がそのまま使える。鍵ファイル自体は
  VMに置かない。ホスト側 agent に鍵が入っていることが前提
- **ゴールデンVM**: `wtx image build` でプロビジョニング済みVMを作り、`wtx up` は `limactl clone`
  するだけ。VM作成が**3〜4分から約8秒**になる（`--no-clone` で毎回プロビジョニング）
- **環境の引き継ぎ（`wtx up --from SRC`）**: ゴールデンVMの代わりに既存VMを clone し、
  docker volume（DBデータ）・pull済みイメージ・導入済みツールをまるごと新VMに乗せる。
  マイグレーション済み・データ投入済みのメインVMから新しい worktree のVMを生やす使い方。
  clone 元は複製の間だけ停止して at-rest のディスクを写し（稼働中コピーの不整合が無い）、
  その後バックグラウンドで自動復帰する（実測ダウンタイム約11秒）。compose の volume 名は
  `<プロジェクト名>_` 接頭辞（既定はディレクトリ名）で worktree ごとに変わるため、
  自動で新しい名前に付け替える（compose ファイルで `name:` を固定していれば接頭辞は
  変わらず、そのまま使われる）。clone 元由来のコンテナは新VMから除去される
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
- **VM内ツール**: docker（rootful）+ Node 22 + Claude Code + git（identityはホスト設定から注入）
- **worktree専用iOSシミュレータ（`wtx sim`）**: シミュレータはVMに入らない（CoreSimulatorは
  ホストのXcodeに属する）ので、ホスト側にworktree専用デバイス `wtx-NAME` を作り、寿命だけを
  VMに連動させる（`wtx up --sim` で作成、`rm`/`prune` で削除、`--from` でアプリ・データごとclone）。
  `wtx sim wire api:3000` でVM内ポートをホストへ払い出し（42000〜、記録式）、エージェントは
  worktree内で `eval "$(wtx sim env)"` → `$WTX_SIM_UDID` / `$WTX_PORT_API` を使う。
  NAME省略時はカレントディレクトリから解決（`wtx which` も同じ）。操作（tap等）はwtxには無く、
  orca emulator / `xcrun simctl` に任せる。設計と検証は [docs/DESIGN-sim.md](docs/DESIGN-sim.md)
  と VERIFICATION.md フェーズ9

## オーケストレータ（Orca等）との連携方針

wtxは何にも依存しない。連携はすべて汎用インターフェース経由:

- Orca terminal から `wtx up` / `wtx exec` / `wtx shell` をそのまま呼べる（**exec終了コードは素通し**）
- worker内からホストのruntimeに届かせたいときは `wtx bridge NAME GUEST:HOST`
- 完了通知をファイルで受けるなら共有マウント上に `.result/` を書く運用も可

## 運用上の注意

- **worktree を消してもVMは残る**（gitにフックが無いため連動できない）。`wtx ls` と TUI は
  そうしたVMを `orphaned` と表示し、`wtx prune --yes` でまとめて掃除できる。
  コミットはホストの `.git` に刻まれているので、VMを消しても作業は失われない。
  片付けを一度で済ませたいときは `wtx rm NAME --with-worktree`（linked worktree のときだけ畳む。
  通常リポジトリでは本体を消さないよう何もしない）
- **旧バージョンのwtx（隔離gitモード）で作ったVM**は、VM内コミットがホストに現れない。
  `wtx up` で再アタッチすると検知して警告するので、作り直すこと。旧バージョンが残した
  gc保護 ref（`refs/wtx/keep/*`）は `wtx rm` がベストエフォートで片付ける
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
- ゴールデンVMには mirrors 設定と `ssh.forwardAgent` が焼き込まれる（`wtx up` 時に certs.d は
  再適用されるが、`daemon.json` の Hub ミラーポートを変えた場合や、旧ゴールデンのままで
  agent フォワードが効かない場合は `wtx image rm && wtx image build` で作り直す）
- `wtx exec` はシェル構文を解釈しない（argv素通し）。パイプ等は `bash -c '...'` で渡す
- clone されたVM（ゴールデン / `--from`）のディスクサイズは clone 元のまま
  （`--disk` は新規プロビジョニング時のみ有効）。`--memory`/`--cpus` は省略すると
  clone 元の値を引き継ぐ
- `--from` の volume 付け替えは `<ディレクトリ名>_` 接頭辞の一致で判定する。
  `COMPOSE_PROJECT_NAME` 環境変数などwtxから見えない方法でプロジェクト名を
  変えている場合は付け替わらない（`docker volume` を手で rename する）
- VM内からの `git push` はホストの ssh-agent に鍵が入っているときだけ通る
  （macOS は `ssh-add --apple-use-keychain` などで agent に鍵を載せておく）
- ミラーのキャッシュ削除（GC）は未実装。`~/.wtx/mirror-cache` を手動で消す

## ライセンス

以下のいずれかのライセンスを選択して利用できる（デュアルライセンス）:

- Apache License, Version 2.0（[LICENSE-APACHE](LICENSE-APACHE) または http://www.apache.org/licenses/LICENSE-2.0）
- MIT License（[LICENSE-MIT](LICENSE-MIT) または http://opensource.org/licenses/MIT）

特に明示しない限り、このリポジトリへ意図的に提出されたコントリビューションは
（Apache-2.0 ライセンスの定義に従い）追加の条件なく上記のデュアルライセンスで
提供されたものとみなす。
