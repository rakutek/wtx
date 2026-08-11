---
name: wtx
description: >-
  Use the `wtx` CLI to give each git worktree a microVM with a dedicated
  in-VM dockerd, DBs, ports, images, and optional host iOS simulator. Use for
  parallel coding agents, Orca/Herdr worktree runtime integration,
  fresh-worktree bootstrap, `.env.example` fallback, `wtx up --from`
  environment seeding, golden VMs, registry mirrors,
  release update checks,
  "worktreeごとにVM/DB", "VM内でdocker", or worktree-specific simulators.
  The worktree and Git metadata are host-shared, but credentials stay on the
  host unless --agent-access is explicitly requested. wtx is not a security sandbox. Use orca-cli for
  Orca-owned worktrees and terminals; use plain git worktrees when no isolated
  VM or Docker runtime is needed.
---

# wtx

git worktree × コーディングエージェントの並列開発のための CLI／TUI。worktree ごとに
独立したVM（Lima vz microVM）＋VM内専用 dockerd を与えるので、各ブランチが自分の
DB・ポート・イメージを持ち、複数エージェントを同時に走らせても衝突しない。
ホストと同じ絶対パスで worktree をマウントするので、ホスト側での直接編集はそのまま使える。
VM内にはversion固定のdocker（rootful）とgitが入る。agent固有CLIはglobal installしない。

worktreeとgit metadataはホストと共有され、VM内コミットは即ホストのブランチに乗る。
`~/.claude`とssh-agentは既定で共有せず、信頼できるVM内agentで必要な場合だけ
`--agent-access`を指定する。wtxは**セキュリティサンドボックスではない**。
agent・編集・Git・資格情報はhost、Docker/DB/serviceだけVMへ送るのが標準形。

コマンドやフラグを暗記や推測で書かないこと。実行前に `wtx --help` /
`wtx <cmd> --help` で確認する。`image`と`mirror`も型付きsubcommandとしてhelpに列挙される。

## セットアップ（初回のみ）

```bash
brew install lima
cargo build --release          # リポジトリ: https://github.com/rakutek/wtx
wtx mirror install             # 任意: レジストリキャッシュ（launchdオンデマンド、常駐なし）
wtx image build                # ゴールデンVM構築（3〜4分）
```

ゴールデンVMがあると以後の `wtx up` は `limactl clone` で約8秒。無い・古い場合は
別環境へ黙ってfallbackせず失敗する。fresh構築が必要なら`--no-clone`を明示する。

## 基本フロー

```bash
wtx new BRANCH                         # worktree + VM を一発で作成（repo内から実行。ブランチが無ければ作る）
wtx new BRANCH --from NAME             # 既存VMからDB(volume)・イメージごと引き継いで作成
wtx up                                 # worktree内で引数なし: そのworktreeのVMを解決して作成/起動
wtx up NAME ~/repos/worktree-dir       # 明示形（worktree自動判別、gitはホストと共有）
wtx ensure NAME ~/repos/worktree-dir --json # 冪等に作成/起動し、dockerd ready receiptを返す
wtx inspect NAME --json                # runtime・worktree・owner・port・sim状態
wtx exec -- docker compose up -d --wait # worktree内ではNAME/-w不要
wtx exec --name NAME -- docker compose up -d --wait # 明示形
wtx shell                              # worktree内ではNAME省略
wtx rm NAME [--if-exists --json]       # 対応版ではオーケストレータ向け冪等cleanup
wtx rm NAME [--with-worktree]          # 単独利用向け削除（コミットはホストに残る）
wtx ls --json                          # 一覧（機械可読。worktreeが消えたVMは orphaned 扱い）
wtx prune --yes                        # 孤児VMを掃除
wtx                                    # 引数なしで ratatui コンソール
wtx update check --json                # 新しい安定版を明示確認（更新はしない）
```

- VM内でコミットすると**そのままホストのブランチが進む**。回収の手順（旧 `wtx sync`）は
  存在しない。VM内pushは`--agent-access`で作成し、host ssh-agentに鍵がある場合だけ使える。
- TUI はVMを**プロジェクト（`wtx up` 時に記録したメインリポジトリ）ごとにまとめて**表示する。
  見出し行で `Space`/`Enter` を押すと開閉し、`[稼働数/総数]` の要約だけになる。
  VM行では `s` 起動/停止、`d` 削除、`Enter` でシェル。
- `wtx exec` は **argv 素通し**でシェル構文を解釈しない。パイプ・glob・リダイレクトは
  `wtx exec -- bash -c '...'` の形で渡す。終了コードは素通しされる。
- 対話CLIは `wtx exec [--name NAME] --tty -- CMD...` で起動する。`--tty` はSSHのPTYを
  強制割り当てし、window resize・signal・終了コードをSSH経由で中継する。
- オーケストレータからは `wtx ensure ... --json` を使う。VMが無ければ作成、停止中なら起動、
  実行中なら再利用し、dockerd readyまで待って `schema_version: 1` のreceiptを返す。
  `--owner orca` / `--owner herdr` はcleanup用の来歴であり、wtx自身はtask statusや
  dispatch lifecycleを管理しない。
- `ensure` で既存VMに `--from` を指定した場合は再cloneせず、記録済み `seeded_from` と
  一致するか検証する。違うseedへ変更したい場合は新しいVMを作る。
- `wtx up` の主なフラグ: `--from`（既存VMから環境を引き継ぐ）、`--memory/--cpus`
  （省略時は新規 4GiB/2、clone は元の値を引き継ぐ）、`--disk`（新規プロビジョニング時のみ）、
  `--agent-access`（信頼できるVM内agent向けに`~/.claude`とssh-agentを共有）、
  `--no-clone`（clone せず新規プロビジョニング。`--from` と排他）。
  追加マウントは位置引数で、`:ro` を付けると読み取り専用。
  credential mount policyは作成時に固定され、切り替える場合はVMを作り直す。

## 新しいworktreeの初回bootstrap

新しいworktreeでは、プロジェクトのコマンドを初めて実行する前に一度だけbootstrapする。
wtxを呼ぶたびには実行しない。

1. worktree rootの`AGENTS.md`、README、package script、`bin/setup`、`scripts/setup`、Makefile、
   justfileなどを調べ、文書化されたリポジトリ固有setupを最優先する。名前だけからコマンドを推測しない
2. リポジトリ固有setupが`.env`を生成しない場合、`.env`が無く`.env.example`があるときだけ、
   この`SKILL.md`からの相対pathで同梱`scripts/bootstrap-env.sh`を解決し、worktreeの絶対pathを
   引数にして実行する。worktree内の同名scriptを使わず、既存`.env`は上書きしない
3. Docker、DB、serviceを必要とするsetup、migration、seedより先に`wtx ensure ... --json`の成功を待つ。
   host側だけで完結する依存導入や静的ファイル生成は先に実行してよい
4. リポジトリ固有のsetup、migration、seedを、文書化されたhost/VMの実行場所で行う。
   container依存コマンドは`wtx exec`へ送る
5. secretや未解決placeholderが必要なら値を推測せず停止して報告する。設定値をログへ表示しない

別worktreeやmainの`.env`をコピーしない。`.env.example`以外の候補を自動選択しない。
`WTX_SIM_UDID`や`WTX_PORT_*`など動的なwtx値を`.env`へ保存せず、使用直前にwtxから解決する。
同梱scriptは`.env.example`から欠けている`.env`を作るだけで、依存導入や任意のsetup commandは
実行しないため、Orca、Herdr、Codex、手動worktreeのどれでも同じfallbackとして使える。

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

## Orca / Herdrから使うとき

agentとオーケストレータはhostで動かし、編集・検索・Gitもhostで行う。Docker、DB、service、
container依存testだけを`wtx exec`でVMへ送る。Composeは禁止せず、VM内で通常の
`docker compose`を使う。wtxが無い・準備に失敗した場合にhost Dockerへfallbackしない。

worktree作成とagent開始を次の順で直列化する:

1. OrcaまたはHerdrでworktreeを作成し、返された絶対pathを読む
2. `wtx ensure WORKTREE_PATH --owner orca|herdr --json`の成功を待つ
3. 成功後にだけ、そのworktreeのagentを開始する

Orcaではnative setup hookから`wtx ensure "$ORCA_WORKTREE_PATH" --owner orca --json`を呼び、
agent startupをsetup完了待ちにする方法も使える。Herdrではworktree createの結果を受けた
親agentが`ensure`を待ってからroot paneでagentを開始する。事後eventだけに準備を任せない。

削除は逆順にせず、先にruntimeを消す:

1. worktree内で`wtx which`を実行してVM名を得る
2. `wtx rm --help`で`--if-exists`と`--json`の対応有無を確認する
3. 対応版では`wtx rm NAME --if-exists --json`を実行し、`action=deleted`または`action=not_found`を成功として扱う
4. 未対応版では`wtx ls --json`の`name`を完全一致で確認し、存在するときだけ`wtx rm NAME`を実行する。存在しなければcleanup済みとして扱う
5. cleanup成功後にだけ、OrcaまたはHerdrでworktreeを削除する

cleanup失敗時はworktreeを残して再試行する。Orcaのarchive hookはUI操作の安全網にできるが、
agent操作ではそれだけに依存しない。VMの寿命はworker terminalではなくworktreeに合わせる。

## ポート

Lima の自動フォワードは全無効化されている（複数 VM が各自の `localhost:5432` を持てる）。

```bash
wtx forward 8080:3000    # HOST:GUEST — VMの3000番をhostの8080番へ (ssh -L)
wtx bridge  9000:9001    # HOST:GUEST — hostの9000番をguestの9001番へ (ssh -R)
wtx unforward 8080       # cwdのVMから解除。明示時は--name NAME
```

## worktree専用 iOS シミュレータ（wtx sim）

iOSアプリを含むリポジトリでは、worktreeごとに専用のシミュレータデバイスを持てる
（`wtx up --sim` または `wtx sim up`）。デバイスは**ホスト側**にあり、寿命はVMと連動する
（`wtx rm` / `prune` で一緒に消え、`--from` ではアプリ・データごとcloneされる）。

このworktreeでシミュレータを使うときは、次を守ること:

- shellから直接使う場合は、セッション開始時と長い待機・VM再起動後にworktreeディレクトリで
  `eval "$(wtx sim env)"` を実行する。エージェントや外部ツールから使う場合は、操作開始の
  直前に同じディレクトリで `wtx sim env --json` を実行し、`sim_udid` を解決する。
  ポートやUDIDは変わりうるので、セッションをまたいで値をキャッシュしない
- シミュレータは `$WTX_SIM_UDID` のデバイス**だけ**を使う。他のデバイスを作成・起動・削除しない
- ビルド: `xcodebuild -destination "id=$WTX_SIM_UDID" -derivedDataPath .wtx-derived ...`
- 起動前に boot: デバイスは Shutdown で作られ、そのままでは launch が
  `SimError 405` になる。`xcrun simctl boot "$WTX_SIM_UDID" && xcrun simctl bootstatus "$WTX_SIM_UDID" -b`
  で起動を待つ（自分のデバイスの boot はこの節の禁止事項に含まれない）
- 起動: `SIMCTL_CHILD_API_BASE_URL="http://127.0.0.1:$WTX_PORT_API" xcrun simctl launch "$WTX_SIM_UDID" <bundle-id>`
  （`WTX_PORT_<LABEL>` は `wtx sim wire <label>:<VM内ポート>` で払い出したホストポート）
- 操作: 外部ツールには後述の規約で担当UDIDを明示する。直接操作する場合は
  `xcrun simctl ... "$WTX_SIM_UDID" ...` のように対象を必ず指定する。wtx に操作コマンドは無い
- VM側（db・api・docker）の作業は `wtx exec -- ...`（NAMEとworkdirは省略時に
  カレントディレクトリから解決する。`wtx sim` 系も NAME 省略で同じ解決が効く）
- シミュレータ操作は**ホスト側でだけ**可能。VM内シェル（`wtx shell` の中）に simctl は
  存在しないので、VM内で頼まれたら実行せずその旨を報告する

### 外部ツールへの汎用バインド規約

Argent、agent-browser、Xcode連携、Computer Useなど、wtx外のツールでシミュレータを
操作するときも、wtxにツール固有のadapterや設定を追加しない。AIエージェントが次の規約で
担当デバイスへbindする:

1. 操作開始の直前に対象worktreeで `wtx sim env --json` を実行し、返された空でない`sim_udid`を
   その作業で唯一使用可能なSimulator IDとする。空なら操作を開始しない
2. 外部ツールのhelpまたはschemaを確認し、次のうち上から使える最も強い方法でbindする:
   - UDID / device ID / destinationを受け取る引数へ完全なUDIDを渡す
   - ツールが定義するSimulator UDID環境変数へ完全なUDIDを渡す
   - 担当UDIDへbind済みで、かつworktreeごとに分離したsessionを使う
   - 担当UDIDとの対応を検証済みの専用window / viewだけをComputer Useで操作する
3. device一覧は担当UDIDの存在確認にだけ使う。先頭、`Booted`、active、focused、前回選択された
   デバイスを暗黙に選ばない。複数台が起動中でも担当UDID以外へfallbackしない
4. 常駐するbrowser・MCP・GUI sessionはworktree単位で分離し、再接続時にも担当UDIDを再確認する
5. 担当UDIDが見つからない、消えた、またはsessionとの対応を検証できない場合は操作を止め、
   `wtx sim status --json` と `wtx sim env --json` で再解決する。別デバイスで継続しない
6. shutdown、erase、delete、session closeなどのcleanupは、担当UDIDとそのworktree専用sessionだけを
   対象にする。ホスト全体や全Simulatorを対象にしたcleanupを行わない
7. 外部ツールが完全なUDIDを指定できず、専用sessionまたは専用windowとの対応も検証できない場合、
   そのツールは並列Simulator作業には使わない。UDIDを指定できる別の手段へ切り替える

構造化CLI/MCPでは`udid`、`device_id`、`destination`などのtarget field、browser automationでは
完全なUDIDに加えてworktree固有のsession/scope、Computer Useでは担当デバイスだと検証できる
専用window/viewを使う。実際のfield名はツールのhelp/schemaで毎回確認し、名前から推測しない。

`wtx sim env` は死んだ forward（VM再起動後など）を自動で張り直すので、
接続できないときはまず `eval "$(wtx sim env)"` を再実行する。状態確認は
`wtx sim status`（`--json` あり）。

## image / mirror

```bash
wtx image  [status|build|rm]                        # 省略時 status
wtx mirror [status|serve|up|down|install|uninstall|gc] # 省略時 status
```

- 既定endpointは、透過的に効くdocker.ioのみ。cacheは20GiB上限で自動GCされ、
  `wtx mirror gc --max-gib N`で永続上限を変更できる。
- ミラーが落ちていても pull は上流直行にフォールバックする（ビルドは止まらない）。
- `~/.wtx/mirrors.json` を編集したら `wtx mirror install` を再実行する。
- ゴールデンVMはprovision schema receiptで互換性を確認する。古ければrebuildする。

## 更新確認

- ユーザーがversion確認を求めたときだけ`wtx update check --json`を実行し、構造化結果を読む。
- 通常コマンドは更新確認の通信をしない。対話TUIだけが24時間cache付きで非同期確認する。
  `wtx tui --snapshot`は確認も通知表示もしない。
- TUI確認を無効化する場合は`WTX_NO_UPDATE_CHECK=1`。wtxにself-update機能はないため、
  更新を頼まれた場合は結果を示して`brew upgrade wtx`を案内し、勝手に実行しない。

## エージェント運用のヒント

- tty なしで TUI の状態を確認するには `wtx tui --snapshot`（1フレーム描画して終了）。
  VM 一覧だけなら `wtx ls`。
- `wtx up --agent-access`を明示した場合だけ、ホストの`~/.claude`とssh-agentをVMへ共有する。
- そのVM内からの `git push` はホストの ssh-agent フォワード経由。鍵が agent に無いと失敗する
  （その場合はホスト側で push するか、`ssh-add` で鍵を載せてもらう）。
- 旧バージョンのwtx（隔離gitモード）で作られたVMでは、VM内コミットがホストに現れない。
  `wtx up` での再アタッチ時に警告が出たら、そのVMは作り直す。
- オーケストレータ（Orca 等）からは `wtx ensure --json` / `wtx inspect --json` /
  `wtx exec --tty` を使う。worktreeとtaskはオーケストレータ、runtimeはwtxが所有する。
  worker から ホスト常駐サービスへ届かせるには `wtx bridge`。完了通知は
  共有マウント上のファイル（例: `.result/`）で受ける運用も可。
