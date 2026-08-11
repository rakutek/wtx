# リリース手順

`vX.Y.Z` タグを push すると、GitHub Actions が Apple Silicon 上で release build を行い、
次の処理を自動実行する。

1. `Cargo.toml` の version とタグが一致することを確認する
2. `aarch64-apple-darwin` バイナリ、MIT/Apache-2.0 ライセンスを tar.gz にまとめる
3. SHA-256 チェックサムとともに GitHub Releases へ公開する
4. `rakutek/homebrew-tap` の `Formula/wtx.rb` を同じ version / SHA-256 に更新する

## 初回セットアップ

- `rakutek/homebrew-tap` という public repository を作成し、default branch を `main` にする
- deploy key を作成し、public key を `rakutek/homebrew-tap` へ write access 付きで登録する
- private key を `wtx` repository の Actions secret `HOMEBREW_TAP_DEPLOY_KEY` に登録する

`GITHUB_TOKEN` は実行元の `wtx` repository にしか書き込めないため、tap 更新には別の認証が必要。
deploy key は `homebrew-tap` だけに権限が限定される。

## リリース

version を更新して CI が成功した後、annotated tag を push する。

```bash
git tag -a v0.5.0 -m "wtx 0.5.0"
git push origin v0.5.0
```

workflow が完了したら、次を Apple Silicon Mac で確認する。

```bash
brew update
brew install rakutek/tap/wtx
wtx --version
```

## 実 VM E2E

GitHub-hosted macOS arm64 runner は nested virtualization をサポートしないため、
Lima/vz を起動する `scripts/check-*.sh` は通常の CI に含めない。
リリース前に Apple Silicon Mac で手動実行するか、必要になった時点で専用の
self-hosted runner workflow を追加する。通常の CI は fmt、clippy、build、unit test に限定する。
