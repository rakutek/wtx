# wtxの信頼モデル

wtxはworktreeごとの**runtime分離**を提供する。セキュリティサンドボックスではない。

## 分離するもの

- VM内のdockerd、container、volume、image store
- VM内のlocalhost、port、process空間
- worktreeごとのDB stateと、任意のiOS Simulator device

これにより、複数agentがprojectの通常コマンドを同時実行しても、同じport、volume名、image tag、
DB migrationを取り合わない。

## 分離しないもの

- worktreeのsource: hostと同じpathをrw mountする
- Git metadata: hostの`.git`をrw mountする
- 明示追加したrw mount

したがってVM内processはhost-visibleなsource、Git config、ref、hookを変更できる。
VMを信頼できないcodeやagentの封じ込めには使わない。完全な封じ込めにはsourceをcopyまたは
read-onlyで渡し、成果物を別経路で回収する設計が必要であり、「VM内commitをhostへ即反映する」
wtxの契約とは両立しない。

## 資格情報

既定では`~/.claude`をmountせず、ssh-agent forwardingも無効にする。agent、編集、Git、GitHub操作は
hostで実行し、Docker、DB、service、container依存testだけを`wtx exec`へ送る。

`--agent-access`は、信頼できるagentをVM内で動かす場合の明示的なopt-inである。このflagは
`~/.claude`のrw mountとssh-agent forwardingを有効にする。秘密鍵file自体はVMへcopyしないが、
VM内processはagentに載った鍵の権限で署名・認証を要求できる。`--agent-access`を指定しても
wtxがセキュリティサンドボックスになるわけではない。

mount policyはVM作成時に固定される。共有なしで作った既存VMへ後から`--agent-access`を付けると
黙って無視せずエラーにする。資格情報共有が既定だった旧versionのVMも、再アタッチで安全に
なったと仮定せず、切り替える場合は削除して作り直す。
