# iOSシミュレータのworktree連動管理（設計案）

状態: **実装済み**（2026-08-11）。
「実機検証項目」は全件検証済みで、結果と実装後のエンドツーエンド確認は
VERIFICATION.md フェーズ9に記録した。本ドキュメントは設計判断と却下案の記録として残す。

## 何を解決するか

db、api、iOSアプリを1リポジトリに持つ構成では、wtxがdbとapiをworktreeごとに分離しても、シミュレータだけが全worktreeで共有のままになる。
worktree Aのアプリとworktree Bのアプリが同じデバイスを取り合い、インストール済みアプリの状態も接続先のapiも混線する。
この設計は、worktree VMと同じライフサイクルで**worktree専用のシミュレータデバイス**を持たせ、デバッグがworktree内で完結するようにする。
あわせて、Claude Code、codex、hermesといった複数のエージェントCLIへ「このworktreeではこのVMとこのシミュレータを使う」という指定を、単一の契約（wtxコマンドと環境変数）で行えるようにする。

## 前提となる制約

シミュレータはVMの中では動かない。
CoreSimulatorはmacOSホストのXcodeに属する仕組みで、wtxのゲストはLinuxだからである。
したがって「worktree専用シミュレータ」はホスト側に置くしかなく、設計は次の分担になる。

- **VM側**：db、api、docker（現行のwtxそのまま）
- **ホスト側**：xcodebuildによるアプリのビルドと、シミュレータデバイス
- **wtxの追加責務**：デバイスのライフサイクルをVMに連動させることと、ホスト側シミュレータからVM内apiへ届くポートの配線

worktreeのソースはホストと同じ絶対パスで両側から見えるため、ビルドはホストで、apiはVMで、同じworktreeを対象に動かせる。

## 設計

### デバイスの割り当て

`wtx up NAME DIR --sim [--sim-device DEVICE_TYPE]` で、デフォルトのdevice setに `wtx-NAME` という名前のデバイスを作る。
`--sim` はオプトインとする。iOSを扱わない利用者にデバイス作成の副作用を負わせないためである。

- UDIDとdevice typeは `~/.wtx/NAME.json` に記録する（`sim_udid`、`sim_devicetype`）
- `wtx rm NAME` はwtxが作ったデバイスを `simctl delete` する。`wtx prune` も同様で、破壊的操作なので既存の `--yes` ゲートに従う
- `wtx ls` とTUIはデバイスの状態（Booted / Shutdown）を併記する
- `wtx up --from SRC` はSRCのデバイスを `simctl clone` する。VMのclone（volume、イメージの引き継ぎ）と対になり、インストール済みアプリとそのデータごと新worktreeに乗る想定（要検証）
- ブートは必要になるまで行わず、不要なresource消費を避けるため`wtx rm`とVM停止時にはshutdownする

`--sim` を付けずに作ったVMには `wtx sim up [NAME] [DEVICE_TYPE]` で後からデバイスを追加できる（冪等）。
VMの作り直しを要求しないための救済経路であり、エージェント自身がデバイスを確保するときの入口でもある。

`xcrun` が無い環境の扱いはコマンドで分ける。
単体の `wtx sim up` は明確なエラーにする。
一方 `wtx up --sim` は警告してVM作成を成功で終える（この時点でVMはできており、シミュレータの失敗で全体を失敗させる方が害が大きい）。
wtxは既にmacOSとLimaを前提にしているので、Xcodeコマンドラインツールへの依存は sim 利用時に限り許容する。

### worktreeからのVM解決（NAME省略）

エージェントに「そのworktreeのシミュレータ」を使わせる鍵は、エージェントがVM名を知らなくて済むことである。
`wtx sim` 系コマンドと新設の `wtx which` は、NAMEを省略するとカレントディレクトリから対象VMを解決する。

- カレントディレクトリをsymlink解決込みで正規化し（macOSでは `/tmp` が `/private/tmp` になる類）、全 `~/.wtx/*.json` の `workdir` と前方一致で照合する。複数一致時は最長一致を採る（エージェントはworktreeのサブディレクトリで作業していることが多い）
- 同じworkdirを持つVMが複数あるときは、候補を列挙してエラーにする。推測で選ばない
- `wtx which` は解決したVM名だけを出力する。sim以外にも合成できる（`wtx exec "$(wtx which)" ...`）

### ポート配線

シミュレータはホストのネットワークをそのまま使うため、アプリから見た接続先はホストの `localhost` である。
一方、worktreeのapiはVM内の `localhost` に閉じている（Limaの自動フォワードは全無効化済み）。
この間を `wtx forward`（ssh -L）で繋ぐが、複数worktreeが同じホストポートを取り合わないよう、wtxがホストポートを払い出して記録する。

```bash
wtx port add api:3000     # VM内3000をホスト側の空きポートへ。割当を記録してforwardを張る
wtx env                    # WTX_VM_NAME / WTX_SIM_UDID / WTX_PORT_API などを出力（evalで取り込む）
wtx sim status            # デバイス状態とforwardの生死
```

いずれもNAME省略時はworktreeから解決する。
`wtx env` と `wtx sim status` は `--json` でツール向け出力も持つ。ポート配線はSimulatorに
依存しないためtop-level commandを正規入口とし、`wtx sim wire` / `wtx sim env`は互換aliasとして残す。

- ホストポートは固定レンジ（42000〜42999）からの順次割当とし、全 `~/.wtx/*.json` の記録値を避けたうえで実際にbindできることも確かめる。名前のハッシュから導く案は、衝突が起きたとき気付けないので採らない。記録された値はJSONを見れば監査できる
- 既知の制約: 複数worktreeで同時に `wtx port add` を走らせると、走査とbind確認の間に割当が競合しうる（TOCTOU）。その場合は後発のssh bindが音を立てて失敗するので、再実行すれば次のポートに逃げる。ロックは入れていない
- 現行の `ssh -L` はVMが停止すると死に、再確立の仕組みが無い。そこで割当をメタデータに記録し、`wtx up` での再アタッチ時と `wtx env` 実行時に、死んでいるforwardを張り直す（**arm on demand**）。この再確立が本設計で唯一の新しいインフラである

### アプリ側に求める契約

ポートはworktreeごとに変わるので、アプリが接続先を固定していると配線しても意味がない。
デバッグビルドのアプリは、接続先を起動環境（`simctl launch` の `SIMCTL_CHILD_*` 環境変数）またはxcconfigから読む必要がある。
これはwtxではなく利用側リポジトリのアプリコードに対する要件であり、この設計の前提である。

### ビルドの分離

xcodebuildはホストでworktreeのソースをそのままビルドする。
DerivedDataをworktree内（例: `.wtx-derived/`）に置けば、worktree間でビルドキャッシュが衝突しない。
これはwtxの機能ではなく運用の指針として文書化する。

### 利用の流れ

```bash
wtx up mono-feat-a ~/repos/mono-feat-a --sim "iPhone 16 Pro"
cd ~/repos/mono-feat-a
wtx exec -- docker compose up -d --wait
wtx port add api:3000
eval "$(wtx env)"

xcodebuild -workspace App.xcworkspace -scheme App \
  -destination "id=$WTX_SIM_UDID" -derivedDataPath .wtx-derived build
xcrun simctl install "$WTX_SIM_UDID" <built>.app
SIMCTL_CHILD_API_BASE_URL="http://127.0.0.1:$WTX_PORT_API" \
  xcrun simctl launch "$WTX_SIM_UDID" com.example.app
```

## エージェントへの指定

「このworktreeではこのVMとこのシミュレータを使う」という指定の実体は、指示テキストとコマンドの組である。
エージェントには次を指示する（この文面がそのまま配布物になる）。

```
- shellから直接使う場合は、セッション開始時と長い待機・VM再起動後にworktreeディレクトリで
  eval "$(wtx env)" を実行する。エージェントや外部ツールから使う場合は、操作開始の直前に
  同じディレクトリで wtx env --json を実行し、sim_udidを解決する。
  ポートやUDIDは変わりうるのでセッションをまたいでキャッシュしない
- シミュレータは $WTX_SIM_UDID のデバイスだけを使う。
  他のデバイスを作成・起動・削除しない
- ビルド: xcodebuild -destination "id=$WTX_SIM_UDID" -derivedDataPath .wtx-derived
- 起動前に boot（Shutdown のままの launch は SimError 405。実測）:
  xcrun simctl boot "$WTX_SIM_UDID" && xcrun simctl bootstatus "$WTX_SIM_UDID" -b
- 起動: SIMCTL_CHILD_API_BASE_URL="http://127.0.0.1:$WTX_PORT_API" \
        xcrun simctl launch "$WTX_SIM_UDID" <bundle-id>
- 操作: 外部ツールには担当UDIDを明示する。直接操作する場合もxcrun simctlへUDIDを渡す
- VM側（db、api、docker）の作業は wtx exec "$(wtx which)" ... で行う
- シミュレータ操作はホスト側でだけ可能。VM内シェル（wtx shell の中）に simctl は
  存在しないので、VM内で頼まれたら実行せずその旨を報告する
```

Argent、agent-browser、Xcode連携、Computer Useなどの外部ツールも、個別adapterではなく
次の共通契約で扱う。

1. 操作直前に対象worktreeの `wtx env --json` が返す空でない`sim_udid`を解決し、その作業で
   唯一使用可能なSimulator IDとする。空なら操作を開始しない
2. 外部ツールのhelp/schemaを確認し、完全なUDIDを渡せるtarget引数、UDID環境変数、
   担当UDIDへbind済みのworktree専用session、対応を検証済みの専用window/viewの順でbindする
3. device一覧は存在確認にだけ使い、先頭、`Booted`、active、focused、前回選択を暗黙に選ばない
4. 常駐sessionはworktree単位で分離し、再接続時にも担当UDIDを再確認する
5. targetが無い、消えた、または対応を検証できない場合は再解決し、別デバイスへfallbackしない
6. cleanupは担当UDIDとworktree専用sessionだけに限定する
7. 完全なUDIDも専用session/windowとの検証可能な対応も持たないツールは並列作業に使わない

構造化CLI/MCPではUDID/device ID/destination field、browser automationでは完全なUDIDと
worktree固有session、Computer Useでは担当デバイスだと検証できる専用window/viewに、この契約を
投影する。field名は固定せず、各ツールのhelp/schemaで確認する。

配布はエージェントごとの慣習に乗せる。SKILL.mdを読めるエージェントにはwtx同梱の
`skills/wtx/SKILL.md`を使い、それ以外には利用側リポジトリのagent指示ファイルへ同じ契約を置く。

役割分担は従来方針のまま、wtxがライフサイクル、所有identity、ポートを持ち、外部ツールが
操作（tap、type、gesture、axなど）を持つ。
wtxに `wtx sim tap` のような操作ラッパーは作らない。
外部ツールと重複するうえ、UDIDという共通契約で表現できていれば、各エージェントが対応する
CLI、MCP、browser、GUI操作を同じ安全規則で使えるからである。

エージェントの置き場所は**ホスト側**（worktreeディレクトリ）を推奨形とする。
VM内で動くエージェントからのシミュレータ操作は、`wtx bridge` でホスト側の操作エンドポイントを露出すれば原理的には可能だが、simctlそのものはVMから呼べないため、v1の対象外とする。

## 却下した代替案

- **VM内でシミュレータを動かす**：不可能。ゲストがLinuxであり、CoreSimulatorはmacOSのXcodeを要求する
- **worktreeごとに別のdevice set（`simctl --set`）**：分離は強くなるが、Xcodeのデバイス一覧からもorcaのデバイス発見からも見えなくなる。デフォルトセット内の命名規約（`wtx-NAME`）で足りる
- **名前ハッシュによるポート決定**：衝突が起きても検出できない。記録式の順次割当を採る
- **利用側リポジトリのスクリプトによる独立管理**：sim要件がバックエンド選択（VMか共有dockerか）に依存しない点では筋が良いが、エージェントへの指示がリポジトリごとに分散し、worktreeとVMの対応を別の登録簿で二重管理することになる。この対応は既に `~/.wtx/*.json` にある。複数のエージェントCLIへの指定を単一契約に集約する要求が決め手で、wtx統合を採る

## 実機検証項目（検証済み）

全件をVERIFICATION.md フェーズ9で検証した。結論のみ再掲する。

1. `simctl clone` のデータ引き継ぎ: **引き継がれる**（デバイスdata直下・アプリdataコンテナともマーカー到達）
2. cloneのShutdown要否: **必要**（BootedはSimError 405）。`--from` は「止めて写して戻す」
3. `SIMCTL_CHILD_*`: **アプリプロセスに届く**（`ps eww` で環境変数を確認）
4. orca attach: devices一覧が `id`=UDID でwtxデバイスを列挙。`orca repo add` 済みリポジトリへの `attach <UDID> --worktree path:...` は `attached: true` で完走（helperのws/stream/axエンドポイントが起動）
5. forward再確立: VM停止でsshマスターは自然終了しソケットも消える。`ensure_forward` は残骸掃除＋再張りの冪等実装
6. resource計測: simulator単体の信頼できる値を分離できなかったため、boot on demand・削除時shutdownを採用

## CLIサーフェス（実装形）

```
wtx which                             # カレントディレクトリ → VM名
wtx port add LABEL:GUESTPORT          # ホストポート払い出し＋forward（冪等、Simulator不要）
wtx env [NAME] [--json]               # eval/JSON出力。死んだforwardの再arm
wtx up NAME DIR --sim                 # VM作成と同時にデバイス作成（--sim-device TYPE で機種指定）
wtx up NAME DIR --from SRC            # SRCにデバイスがあれば自動でclone（ポート定義も引き継ぎ）
wtx sim up [NAME] [--device TYPE]     # 既存VMへデバイスを後付け（冪等。消失デバイスの再作成も）
wtx sim status [NAME] [--json]        # デバイス状態とforward生死
wtx sim wire LABEL:GUESTPORT [NAME]   # `wtx port add`の互換alias
wtx sim env [NAME] [--json]           # `wtx env`の互換alias
wtx sim rm [NAME]                     # デバイスのみ削除
```

NAMEはすべて省略可能で、省略時はカレントディレクトリのworktreeから解決する。
設計案の `--sim [DEVICE_TYPE]`（値が省略可能なフラグ）は、後続の位置引数（追加マウント）を
値として飲み込む誤解析があるため、`--sim` ＋ `--sim-device TYPE` の2フラグに分けた。
互換aliasである`sim wire`のNAMEが後置なのも同じ理由（省略可能な位置引数は必須引数に先行できない）。
`wtx stop` / TUIのstopは起動中デバイスをshutdownし、`wtx up`は記録済みforwardを再armする。
`wtx rm` / `wtx prune` / `wtx ls` / TUIのsim表示も対応済み。
