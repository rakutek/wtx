# wtx

git worktree ごとに隔離VM（Lima/vz microVM）＋VM内専用dockerdを与えるGo製CLI。
Docker Sandboxes のOSS代替（Dockerアカウント・ライセンス・Docker Desktop不要）。
全メカニズムの実機検証記録は `../myapp/VERIFICATION.md`。

```
brew install lima && go build -o wtx .
wtx up myapp-feature-a ~/repos/myapp-feature-a     # worktreeを自動判別
wtx exec myapp-feature-a -w ~/repos/myapp-feature-a docker compose up -d --wait
wtx shell myapp-feature-a                          # 中でclaudeも使える
wtx sync myapp-feature-a                           # コミットをホストへ回収
wtx rm myapp-feature-a
```

## 設計

- **microVM隔離**: Lima vz = Apple Virtualization.framework。Docker SandboxesのmicroVMと同じ隔離クラス
- **同パスマウント**: virtiofsでホストと同じ絶対パス。worktreeのホスト直接編集は維持される
- **隔離gitモード（デフォルト）**: ホストの`.git`は`/run/wtx/base.git`に**roマウント**し、VM内の
  `<main>/.git`パスにはVMローカルのリポジトリを構築（objectsはalternates参照＝コピーゼロ・即時）。
  ホストの`.git`は物理的に不変なので、`.git/hooks`や`.git/config`への注入による**ホストでのコード実行
  （VM脱出）・ref破壊・gc事故が構造的に不可能**。コミット回収は`wtx sync`
  （`refs/wtx/<name>/*`にfetch、ホストブランチは勝手に動かさない）。旧方式のrw共有は`--share-git`
- **エージェント認証**: `wtx up`時にホストの`~/.claude/.credentials.json`をVMへ**コピー**（`--no-claude`で無効）。
  マウントにしない理由: `~/.claude`をrw共有するとVM内エージェントがホストのsettings.json（hooks等）を
  書き換えられ、隔離が破れるため
- **内蔵レジストリミラー**: `wtx mirror up`でGoプロセスとしてpull-throughキャッシュを起動
  （distributionライブラリ、127.0.0.1バインド、Docker不要）。VMの`daemon.json`は常にミラーを向くが、
  **ミラーが落ちていればDocker Hub直行に自動フォールバック**するので常駐は任意。
  個人利用なら不要、エージェント並列運用（Hubレートリミットが見える規模）で入れる
- **ポート**: Limaの自動フォワードは全無効化。複数VMが各自の`localhost:5432`を同時に持てる。
  公開は`wtx forward`（ssh -L）、ホスト常駐サービスへの逆方向は`wtx bridge`（ssh -R）
- **読み取り専用VM**: 追加マウントに`:ro`でreviewer用（書き込みはFSレベルで拒否）
- **VM内ツール**: docker（rootful）+ Node 22 + Claude Code + git（identityはホスト設定から注入）

## オーケストレータ（Orca等）との連携方針

wtxは何にも依存しない。連携はすべて汎用インターフェース経由:

- Orca terminal から `wtx up` / `wtx exec` / `wtx shell` をそのまま呼べる（**exec終了コードは素通し**）
- worker内からホストのruntimeに届かせたいときは `wtx bridge NAME GUEST:HOST`
- 完了通知をファイルで受けるなら共有マウント上に`.result/`を書く運用も可

## 隔離gitモードの運用上の注意

- VMとホストは**同じ作業ツリーを共有しつつ、別々のindex/refsを持つ**。VM内でコミットしても
  ホストのブランチは動かないので、ホスト側`git status`にはVMの変更が未コミットとして見える。
  回収は`wtx sync`（`refs/wtx/<name>/*`）→ `git merge --ff-only` の順で行う
- VMのobjectsはホストの`.git`をalternates参照している。**回収前にホスト側で`git gc`すると
  VMが参照中のobjectが刈られうる**。長期間VMを放置する運用では`wtx sync`をこまめに行う
- workdirが通常のリポジトリ（linked worktreeでない）場合、隔離gitは適用されない。
  この場合`.git`はworkdir内にありrwのまま共有されるため、hooks注入のリスクは残る

## 既知の制約 / TODO

- VM作成は3〜4分（docker/node/claude-codeインストールが支配的）。provision済みイメージを焼けば数十秒にできる
- ミラーはdocker.ioのみ（dockerdの`registry-mirrors`自体がHub専用。ghcr等は直行）
- ミラーのlaunchdソケットアクティベーション化（pull到来時のみ起動・アイドル自動終了）は未実装
- `wtx exec`はシェル構文を解釈しない（安全なargv素通し）。パイプ等は`bash -c '...'`で渡す
- 資格情報コピーはトークンのスナップショット。VM側でのOAuthリフレッシュがホスト側セッションと
  競合する可能性は未検証
