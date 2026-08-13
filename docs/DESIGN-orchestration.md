# オーケストレータ連携契約

## 境界

wtx は worktree ごとの runtime provider であり、task scheduler ではない。

- Orca / Herdr 等: task、agent terminal、worktree、完了状態を所有する
- wtx: VM、dockerd、DB、port、Simulator、runtime readinessを所有する
- 両者のjoin key: worktreeの正規化済み絶対パス

## 冪等な準備

```bash
wtx ensure NAME WORKDIR \
  --json
```

`ensure`のactionは次のいずれか。

- `created`: VMが無く、新規作成した
- `started`: VMが存在したが停止中で、起動した
- `reused`: VMがすでに実行中だった

全actionでdockerdが応答するまで待つ。既存VMに`--from`を指定しても再cloneせず、metadataの
`seeded_from`と一致することだけを検証する。wtx metadataを持たない同名Lima VMは暗黙に採用しない。

## JSON receipt

成功receiptは`schema_version`を持つ。破壊的なschema変更ではversionを上げ、既存fieldの意味を
同じversion内で変えない。

```json
{
  "schema_version": 2,
  "action": "created",
  "instance": {
    "name": "worker-a",
    "status": "Running",
    "ready": true,
    "runtime": { "docker": "ready" },
    "worktree": {
      "path": "/abs/worktree",
      "repo": "/abs/repo",
      "branch": "feature-a",
      "head": "0123456789abcdef",
      "orphaned": false
    },
    "ports": {}
  }
}
```

`wtx inspect [NAME] --json`は同じ`instance` schemaを返す。NAME省略時はカレントディレクトリを
覆うVMを解決する。JSONはstdout、warning/errorはstderrへ出し、stdoutへ進捗を混ぜない。

## VM commandとTTY

```bash
wtx exec --name NAME -w WORKDIR -- docker compose up -d --wait
wtx exec --name NAME --tty -- <interactive-command>
```

`--tty`はSSHへ`-tt`を渡す。SSHがPTY、window resize、signalを中継し、wtxはremote processの
終了コードをそのまま返す。標準形ではagent自体をhostで動かす。信頼できるagentをVM内で動かす
例外経路では、VM作成時に`--agent-access`を明示しない限り資格情報を共有しない。

## agent開始のbarrier

オーケストレータはworktree作成後、`wtx ensure ... --json`が成功してからagentを開始する。
`worktree.created`のような事後eventだけに準備を委ねず、agent開始側がready receiptをbarrierとして扱う。
失敗時はworktreeを残して理由を表示し、host Dockerへfallbackしない。

- Orca: native setup hookで`wtx ensure "$ORCA_WORKTREE_PATH" --json`を実行し、
  agent startup policyを`wait-for-setup`にする
- Herdr: `worktree create`のJSONからworktree pathとroot paneを取得し、親agentが
  `wtx ensure WORKTREE_PATH --json`を待ってから、そのpaneで`agent start`する

Orca/Herdrはagent、worktree、terminalを所有し続ける。wtxはagent開始やpane操作を行わない。

## commandの実行場所

coding agentとオーケストレータはhostで動かす。編集、検索、Git、GitHub操作もhostで行い、
Docker、DB、service、container依存testだけを`wtx exec`でVMへ送る。Composeは置き換えず、
VM内で通常の`docker compose`として実行する。host Dockerへのsilent fallbackは禁止する。

## cleanup

オーケストレータ管理下では、まず`wtx rm NAME --if-exists --json`でruntimeを消し、その成功後に
オーケストレータがworktreeを閉じる。この順序を逆にしない。cleanup失敗時はworktreeを残して
再試行可能にする。

`rm --if-exists --json`は、削除時に`action: deleted`、既に無い場合に`action: not_found`を返し、
どちらも成功終了する。`wtx rm --with-worktree`はwtx単独運用向けであり、Orca/Herdr管理下では
使わない。

`up` / `new` / `ensure`による7日猶予の孤児VM自動回収は、cleanup漏れに対する安全網である。
実行時刻も完了receiptも保証しない。
自動回収の通知とwarningはstderrへ出力し、`ensure --json`のstdoutには混ぜない。
VM内だけにcommitが残る旧隔離Git形式を検出した場合は、自動回収と`prune --yes`の対象から外す。
そのため、オーケストレータはこれを通常のcleanup経路として扱わず、引き続き`rm --if-exists --json`の成功を確認してからworktreeを閉じる。

Orcaのarchive hookはUI操作時の安全網として同じcleanupを実行できるが、coding agentによる
削除ではarchive hookだけに依存せず、先に明示的な`wtx rm`成功を確認する。
