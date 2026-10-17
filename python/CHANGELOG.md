# Changelog

## 0.1.0 (2026-06-19)


### Features

* PyO3 bindings (Xgrep, SearchResult, IndexStatus) ([bae762f](https://github.com/momokun7/xgrep/commit/bae762f083f11371378925cf0790a5a5af1bf273))
* publish the xgrep-search package to PyPI (PyO3 bindings and the xg binary) ([#58](https://github.com/momokun7/xgrep/issues/58)) ([7785704](https://github.com/momokun7/xgrep/commit/7785704dcb2027db7dafad8d9be185bab1b5a93c))
* add description and readme to the PyPI metadata ([bf55900](https://github.com/momokun7/xgrep/commit/bf55900e7fefaeddae608cf776d2abe39d982b14))
* add Python type stubs (_xg.pyi) ([d02014e](https://github.com/momokun7/xgrep/commit/d02014eb699ec4a4cdf24c9aef0ca9eb0fdce056))
* add the python/ crate skeleton (PyO3 and the xg binary) ([636028d](https://github.com/momokun7/xgrep/commit/636028dad7c9c5f37ba9ada85b0e9a1bfe4cb7a9))


### Bug Fixes

* install maturin from requirements-build.txt (Scorecard PinnedDependencies) ([#65](https://github.com/momokun7/xgrep/issues/65)) ([d7b5220](https://github.com/momokun7/xgrep/commit/d7b5220299f0dfd1da545304a1bb2c5d8db10df4))
* safer type conversions in the PyO3 bindings (try_from, fewer pub items) ([0b0d428](https://github.com/momokun7/xgrep/commit/0b0d428d55830988d1decc8c4f22961f6c417c98))
* set the python/Cargo.toml version to 0.6.0 ([4e7bfde](https://github.com/momokun7/xgrep/commit/4e7bfde93ed85a19a72b9ad1d62738311af0fd0a))
* make sed -i work on macOS (-i.bak) ([f24ef35](https://github.com/momokun7/xgrep/commit/f24ef3523b70aa8114fe56b30e873ee32a60a62f))
* fix package registry metadata (PyPI stuck at 0.5.0, no README on crates.io) ([fe1b355](https://github.com/momokun7/xgrep/commit/fe1b3558536eb85e8397fec3c0412e9abd382b71))

## [0.6.0](https://github.com/momokun7/xgrep/compare/v0.5.0...v0.6.0) (2026-06-16)


### Features

* publish the xgrep-search package to PyPI (PyO3 bindings and the xg binary) ([#58](https://github.com/momokun7/xgrep/issues/58)) ([7785704](https://github.com/momokun7/xgrep/commit/7785704dcb2027db7dafad8d9be185bab1b5a93c))
