# wtx

git worktree ごとに隔離VM（Lima/vz microVM）＋VM内専用dockerdを与える薄いCLI。
Docker Sandboxes のOSS代替（Dockerアカウント・ライセンス不要）。全メカニズムの実機検証記録は `../myapp/VERIFICATION.md`。

```
brew install lima          # 依存はこれだけ（+ ホストに任意のdocker: ミラー用）
wtx mirror up              # 初回に一度: pull-throughレジストリキャッシュ
wtx up myapp-feature-a ~/repos/myapp-feature-a ~/repos/myapp/.git
wtx exec myapp-feature-a -w ~/repos/myapp-feature-a docker compose up -d --wait
wtx shell myapp-feature-a
wtx rm myapp-feature-a
```

## 設計

- **microVM隔離**: Lima vz = Apple Virtualization.framework。Docker SandboxesのmicroVMと同じ隔離クラス
- **同パスマウント**: virtiofsでホストと同じ絶対パス。worktreeの`.git`ポインタ（`gitdir: <main>/.git/worktrees/<name>`）が
  そのまま解決するよう、メインリポジトリの`.git`も一緒にマウントする（検証済み）
- **イメージ重複対策**: 各VMの`daemon.json`に`registry-mirrors`を注入し、ホストのpull-throughキャッシュ
  （`registry:2`）経由でpull。2台目以降のpullは約5秒（検証済み）。sbxのようなsave/load運用は不要
- **ポート**: Limaの自動フォワードは全無効化。複数VMが各自の`localhost:5432`を同時に持てる。
  公開は`wtx forward`（ssh -L）、ホスト常駐サービス（オーケストレータ等）への逆方向は`wtx bridge`（ssh -R）
- **読み取り専用VM**: マウントに`:ro`を付けるとreviewer用VMになる（書き込みはファイルシステムレベルで拒否。
  gitは`--no-optional-locks`で読む）
- **VM内ツール**: docker（rootful）+ Node 22 + git（identityはホストの`git config --global`から注入）

## オーケストレータ（Orca等）との連携方針

wtx自体は何にも依存しない。連携はすべて汎用インターフェース経由:

- Orca worktree/terminal から `wtx up` / `wtx exec` / `wtx shell` をそのまま呼べる（終了コードは素通し）
- `wtx ls --json` で状態取得
- worker内からホストのruntime（例: Orcaのlocal API）に届かせたいときは `wtx bridge NAME GUEST:HOST`
- 完了通知をファイルで受けるなら、共有マウント上に`.result/`を書く運用も可

## 既知の制約 / 今後

- VM作成は2〜3分（get.docker.com + nodesourceが支配的）。短縮するならprovision済みqcow2を焼く
- ミラーはdocker.ioのみ。ghcr等は直接pull
- ミラーがホストDocker上で動く。完全OSS化するならbrewの`distribution`か専用Lima VMへ移す
- VM内エージェントの認証は未解決（`~/.claude`をマウントするとYOLOエージェントに資格情報を渡すことになる）
- 共有`.git`はVM間で書き込み可（gitのロックが並行アクセスは捌くが、暴走エージェントへの防御は規約。
  完全隔離したい場合はマウントを`:ro`にしてVM内cloneする運用に切り替える）
