<div align="center">

# wtx

**どのworktreeにも、同じlocalhostと別々のruntimeを。**

プロジェクトへwtx専用設定を足さず、各エージェントへいつもの`localhost:5432`と
DBごとcloneできる専用runtimeを渡す。

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](#ライセンス)
[![Platform: macOS on Apple Silicon](https://img.shields.io/badge/platform-macOS%20on%20Apple%20Silicon-black.svg?logo=apple)](#動作環境)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](Cargo.toml)
[![CI](https://github.com/rakutek/wtx/actions/workflows/ci.yml/badge.svg)](https://github.com/rakutek/wtx/actions/workflows/ci.yml)

[English](README.md) | 日本語

</div>

---

別々のブランチで並列に動くエージェントも、1つのDocker daemon、DB、
`localhost:5432`を共有すれば衝突する。**wtx**はgit worktreeごとに
専用のLima/vz microVMを用意し、dockerd、volume、image、localhostを分離する。

エージェント、editor、Git、資格情報はmacOS hostに置く。Docker、DB、service、
container依存testだけを`wtx exec`で実行する。Docker Desktopは不要。

```text
 wtx   mirror[launchd]  ●docker.io
    NAME                    STATUS        BRANCH          SIM         NOTE
┌ VMs ──────────────────────────────────────────────────────────────────────┐
│▶▾ hono-test  ~/dev/hono-test  [2/2 running]                               │
│   books-api               Running       books-api       sim:Booted        │
│   hono-dev                Running       main                              │
│ ▾ myapp  ~/repos/myapp  [1/2 running]                                     │
│   myapp-feature-a         ⠹ starting    feature-a                         │
│   myapp-feature-b         Stopped       feature-b                         │
│ ▾ (no project)  [0/1 running]                                             │
│   wtx-golden              Stopped                                         │
└───────────────────────────────────────────────────────────────────────────┘

 j/k:move  Enter:shell/fold  s:start/stop  d:delete  Space:fold  r:refresh  q:quit
```

## ハイライト

- **準備済みVMから開始：** 毎回新規provisioningせず、準備済みgolden VMをcloneする
- **必要な状態を丸ごと引き継ぐ：** `wtx up --from`でDB volume、image、導入済みtool、
  必要ならSimulator dataも新しいworktreeへ移せる
- **通常のportを維持：** どのworktreeも`localhost:5432`を使えるため、branch別offsetや
  project固有設定が要らない
- **Gitを直接共有：** VM内のcommitはhost branchへそのまま反映され、回収や同期の工程がない
- **安定した自動化interface：** `ensure --json`と`inspect --json`がversion付きの
  readiness・owner情報を返す
- **必要な機能を同梱：** 容量制限付きregistry cache、worktree専用iOS Simulator、
  project単位のTUI、Homebrew経由の1 command upgrade

> [!WARNING]
> **wtxが分離するのはruntimeの衝突であり、信頼境界ではない。** worktreeと`.git`は
> hostの読み書きmountなので、VM内codeはhostから見えるsourceとGit metadataを変更できる。
> agentと資格情報はhostに置き、信頼できないcodeの封じ込めには使わない。
> `--agent-access`は信頼できるVM内agentへ資格情報を明示共有するoptionである。

正確な境界は[信頼モデル](docs/TRUST-MODEL.md)を参照。

## 動作環境

- Apple SiliconのmacOS
- [Lima](https://lima-vm.io/)（Homebrew formulaが自動で導入）
- `wtx sim`を使う場合のみXcode
- sourceからbuildする場合のみRust toolchain

## インストール

```bash
brew install rakutek/tap/wtx
```

> [!NOTE]
> crates.ioの`wtx` crateは無関係の別project。sourceからbuildする場合は、このrepositoryを
> cloneし、Limaを別途導入して`cargo install --path .`を実行する。

## クイックスタート

```bash
cd ~/repos/myapp
wtx new feature-a     # worktreeとVMを作成。初回だけ共通base VMも自動準備
cd ../myapp-feature-a
wtx exec -- docker compose up -d --wait
wtx port add web:3000 # VMの3000番へ衝突しないhost portを自動割当
eval "$(wtx env)"     # WTX_PORT_WEBをexportし、必要ならforwardを再接続

cd ~/repos/myapp
wtx new feature-b --from myapp-feature-a # DB data、image、toolを引き継ぐ

wtx rm myapp-feature-a --with-worktree    # VMとlinked worktreeをまとめて削除
wtx                                      # TUIを開く
```

## 仕組み

```mermaid
flowchart LR
    subgraph HOST["macOS host"]
        AG["agent · editor · Git"]
        WT["worktree files + .git"]
        AG --> WT
    end
    subgraph A["microVM: feature-a"]
        AD["dockerd · postgres :5432 · images"]
    end
    subgraph B["microVM: feature-b"]
        BD["dockerd · postgres :5432 · images"]
    end
    WT -->|"same absolute path"| A
    WT -->|"same absolute path"| B
    AG -->|"wtx exec"| AD
    AG -->|"wtx exec"| BD
```

- 初回は共通base VMを自動provisioningし、以後の`wtx up`は`limactl clone`を使う。
  baseに互換性がなければ自動更新する
- virtiofsで各worktreeをhostと同じ絶対pathへmountし、書き込み可能なGit metadataも共有する
- Limaの自動port forwardingは無効。記録付き自動割当は`wtx port add api:3000`、host portを
  明示する場合は`wtx forward`、host serviceをVMへ届ける場合は`wtx bridge`を使う
- VMの既定値はRAM 4 GiB、CPU 2、disk 20 GiB。完全なruntime分離がそのcostに見合わない場合は、
  素のworktreeやCompose project名による分離を使う

環境引き継ぎ、network、registry cache、Simulator、TUI、更新の詳細は
[機能ガイド](docs/FEATURES.md)にまとめている。

## 主なコマンド

| コマンド | 用途 |
|---|---|
| `wtx new BRANCH` | worktreeとVMをまとめて作成 |
| `wtx up [NAME] [DIR]` | 既存worktree用のVMを作成・起動 |
| `wtx exec -- CMD…` | 現在のworktreeのVM内でcommandを実行 |
| `wtx shell [NAME]` | VM内shellを開く |
| `wtx ls` | VM一覧と孤児VMを表示 |
| `wtx port add LABEL:GUEST` | VM service用のhost portを自動割当して記録 |
| `wtx env` | `WTX_PORT_*`をexportし、記録済みforwardを再接続 |
| `wtx forward HOST:GUEST` | VM portをhostへ公開 |
| `wtx stop [NAME]` / `wtx rm NAME` | VMを停止・削除 |
| `wtx prune --yes` | worktreeが消えたVMを削除 |
| `wtx` | TUIを開く |

全commandは[コマンドリファレンス](docs/CLI.md)または`wtx --help`を参照。

## エージェントとオーケストレータ

taskとworktreeはorchestrator、runtimeだけをwtxが所有する。

```bash
wtx ensure worker-a /abs/worktree --owner orca --json
wtx inspect worker-a --json
wtx exec --name worker-a -w /abs/worktree -- docker compose up -d --wait
```

readiness schemaとcleanup順序は[オーケストレータ連携契約](docs/DESIGN-orchestration.md)に記載。

## エージェント用skill

このrepositoryには、対応するcoding agentへwtxのsetup・運用方法を伝える
[エージェント用skill](skills/wtx/SKILL.md)を同梱している。Agent Skills CLIでは
次のcommandで導入できる。

```bash
npx skills add rakutek/wtx
```

Codexでは、会話から組み込みのskill installerへrepository内のskill pathを渡す方法も
使える。

```text
$skill-installer install the skill from https://github.com/rakutek/wtx/tree/main/skills/wtx
```

skillは`wtx`実行file自体を導入しないため、先にHomebrewでwtx本体をインストールする。
導入後、Codexはskillを自動検出する。`/skills`から選択するか、`$wtx`で明示的に
呼び出せる。
一覧に現れない場合はCodexを再起動する。

## ドキュメント

- [機能ガイド](docs/FEATURES.md)：runtime、環境引き継ぎ、mirror、Simulator、TUI、更新
- [コマンドリファレンス](docs/CLI.md)：全commandと自動化上の注意
- [運用と制約](docs/OPERATIONS.md)：resource、cleanup、既知の制約、E2E検証
- [信頼モデル](docs/TRUST-MODEL.md)：mountと資格情報の境界
- [オーケストレータ連携契約](docs/DESIGN-orchestration.md)：readinessとowner schema
- [Simulator設計](docs/DESIGN-sim.md)：deviceとportの割り当て
- [検証記録](VERIFICATION.md)：実VMでの実験と不採用案

CLI、help、TUIは英語。READMEには[英語版](README.md)があり、一部の設計・検証文書は日本語。

## ライセンス

このrepositoryはMIT License（[LICENSE-MIT](LICENSE-MIT)）で公開しています。
特に明示しない限り、このrepositoryへ提出されたcontributionもMIT Licenseで扱います。
