# Changelog

## 0.1.0 (2026-06-19)


### Features

* PyO3 バインディングを実装（Xgrep, SearchResult, IndexStatus） ([bae762f](https://github.com/momokun7/xgrep/commit/bae762f083f11371378925cf0790a5a5af1bf273))
* PyPI への xgrep-search パッケージ追加（PyO3 バインディング + xg バイナリ） ([#58](https://github.com/momokun7/xgrep/issues/58)) ([7785704](https://github.com/momokun7/xgrep/commit/7785704dcb2027db7dafad8d9be185bab1b5a93c))
* PyPI メタデータに description と readme を追加 ([bf55900](https://github.com/momokun7/xgrep/commit/bf55900e7fefaeddae608cf776d2abe39d982b14))
* Python 型スタブ (_xg.pyi) を追加 ([d02014e](https://github.com/momokun7/xgrep/commit/d02014eb699ec4a4cdf24c9aef0ca9eb0fdce056))
* python/ crate の骨格を追加（PyO3 + xg バイナリ） ([636028d](https://github.com/momokun7/xgrep/commit/636028dad7c9c5f37ba9ada85b0e9a1bfe4cb7a9))


### Bug Fixes

* pip install maturin を requirements-build.txt に移行（Scorecard PinnedDependencies） ([#65](https://github.com/momokun7/xgrep/issues/65)) ([d7b5220](https://github.com/momokun7/xgrep/commit/d7b5220299f0dfd1da545304a1bb2c5d8db10df4))
* PyO3 バインディングの型変換を安全化（try_from、pub削除） ([0b0d428](https://github.com/momokun7/xgrep/commit/0b0d428d55830988d1decc8c4f22961f6c417c98))
* python/Cargo.toml のバージョンを 0.6.0 に修正 ([4e7bfde](https://github.com/momokun7/xgrep/commit/4e7bfde93ed85a19a72b9ad1d62738311af0fd0a))
* sed -i を macOS 互換に修正 (-i.bak) ([f24ef35](https://github.com/momokun7/xgrep/commit/f24ef3523b70aa8114fe56b30e873ee32a60a62f))
* パッケージレジストリのメタデータを修正 (PyPI 0.5.0 固着・crates.io README なし) ([fe1b355](https://github.com/momokun7/xgrep/commit/fe1b3558536eb85e8397fec3c0412e9abd382b71))

## [0.6.0](https://github.com/momokun7/xgrep/compare/v0.5.0...v0.6.0) (2026-06-16)


### Features

* PyPI への xgrep-search パッケージ追加（PyO3 バインディング + xg バイナリ） ([#58](https://github.com/momokun7/xgrep/issues/58)) ([7785704](https://github.com/momokun7/xgrep/commit/7785704dcb2027db7dafad8d9be185bab1b5a93c))
