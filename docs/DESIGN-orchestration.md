# オーケストレータ連携契約

## 境界

wtx は worktree ごとの runtime provider であり、task scheduler ではない。

- Orca / Herdr 等: task、agent terminal、worktree、完了状態を所有する
- wtx: VM、dockerd、DB、port、Simulator、runtime readinessを所有する
- 両者のjoin key: worktreeの正規化済み絶対パス

owner metadataはcleanupと監査のための来歴であり、wtxがRunやDispatchの状態を解釈することはない。

## 冪等な準備

```bash
wtx ensure NAME WORKDIR \
  --owner orca \
  --owner-label run_id=run_123 \
  --owner-label task_id=task_456 \
  --owner-label dispatch_id=dispatch_789 \
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
  "schema_version": 1,
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
    "owner": {
      "kind": "orca",
      "labels": {
        "dispatch_id": "dispatch_789",
        "run_id": "run_123",
        "task_id": "task_456"
      }
    },
    "ports": {}
  }
}
```

`wtx inspect [NAME] --json`は同じ`instance` schemaを返す。NAME省略時はカレントディレクトリを
覆うVMを解決する。JSONはstdout、warning/errorはstderrへ出し、stdoutへ進捗を混ぜない。

## 対話agent

```bash
wtx exec NAME --tty -w WORKDIR claude
```

`--tty`はSSHへ`-tt`を渡す。SSHがPTY、window resize、signalを中継し、wtxはremote processの
終了コードをそのまま返す。非対話コマンドは従来どおり`--tty`なしで実行する。

## cleanup

オーケストレータ管理下では、まず`wtx rm NAME`でruntimeを消し、その後オーケストレータが
worktreeを閉じる。`wtx rm --with-worktree`はwtx単独運用向けであり、Orca/Herdr管理下では使わない。
