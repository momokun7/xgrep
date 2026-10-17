# Changelog

## [0.8.2](https://github.com/momokun7/xgrep/compare/v0.8.1...v0.8.2) (2026-10-17)


### Bug Fixes

* **release:** ship license files; generate the npm README API section ([7cf8090](https://github.com/momokun7/xgrep/commit/7cf8090b93660c250a9a1ba7ce58ec28a67c2de3))
* **release:** ship the license files in every package ([1ec22f4](https://github.com/momokun7/xgrep/commit/1ec22f485019953eb93f60d689b5bd6234b58f04))

## [0.8.1](https://github.com/momokun7/xgrep/compare/v0.8.0...v0.8.1) (2026-10-17)


### Features

* **cli:** search standard input and exit quietly on a closed pipe ([#108](https://github.com/momokun7/xgrep/issues/108)) ([27b594d](https://github.com/momokun7/xgrep/commit/27b594d28eca8184a05bd48d4cb2758d8a42428d))


### Bug Fixes

* **cli:** search the directory when piped standard input is empty ([#108](https://github.com/momokun7/xgrep/issues/108)) ([0f560df](https://github.com/momokun7/xgrep/commit/0f560dfc4bdf327f9ec0695f4f1d0f0610c0db20))
* **python:** build abi3 wheels so every CPython >= 3.8 can install them; 0.8.0 had wheels for Python 3.11 only ([#110](https://github.com/momokun7/xgrep/issues/110)) ([488c717](https://github.com/momokun7/xgrep/commit/488c717a7adc4c5a3969e6c87ff9029ca26b0ab3))
* **docker:** build the xg binary with Rust 1.85 in the benchmark image ([#109](https://github.com/momokun7/xgrep/issues/109)) ([2cf7b35](https://github.com/momokun7/xgrep/commit/2cf7b3516747187ed3286d6b288a3018f818019f))


### Miscellaneous

* dual-license under MIT OR Apache-2.0 ([#111](https://github.com/momokun7/xgrep/issues/111)) ([6b42237](https://github.com/momokun7/xgrep/commit/6b422373aa485bb52b49e376b8b5aa22a60c35ea))

## [0.8.0](https://github.com/momokun7/xgrep/compare/v0.7.0...v0.8.0) (2026-10-17)


### ⚠ BREAKING CHANGES

* **index:** index format v4 with folded trigrams, posting masks and a build-only sidecar
* --json output is no longer a single JSON array.

### Features

* **cli:** accept multiple PATH arguments ([ef5f1ff](https://github.com/momokun7/xgrep/commit/ef5f1ff35f5807348fa081d4e1cfed009c39e746))
* emit --json as JSON Lines with a summary object ([85d9f3a](https://github.com/momokun7/xgrep/commit/85d9f3a4a09839bb053ee99b7f61cedb65ebbcc1))
* enforce llm token budgets per line and add --max-tokens/--max-columns ([ba0e5fd](https://github.com/momokun7/xgrep/commit/ba0e5fd184f7deff708358186dc8199bcb47f7e5))
* **history:** search git diffs (--in-diff) and commit messages (--log) ([83493e0](https://github.com/momokun7/xgrep/commit/83493e08547d823046e4be511c4c85c1dda28f3c))
* **index:** index format v4 with folded trigrams, posting masks and a build-only sidecar ([cac0e23](https://github.com/momokun7/xgrep/commit/cac0e23ab7289fa1ee492823d8023669c77c1e1d))
* **mcp:** add ranking, definition heuristics, session and paging modules ([1eb3135](https://github.com/momokun7/xgrep/commit/1eb31356a42d1385fbd1945487bb8f7b82cb470f))
* **mcp:** ignore unknown arguments with a note instead of failing ([aa98181](https://github.com/momokun7/xgrep/commit/aa98181fc411d6198c0815cfff9ee0ed4766c196))
* **mcp:** ranked, paginated tools with validation, find_files and multi-query ([1685465](https://github.com/momokun7/xgrep/commit/16854655ffbc51b769eee3cf89751b09b6986ba4))
* report files skipped for exceeding the max file size ([34df71c](https://github.com/momokun7/xgrep/commit/34df71c5198352a72c1d40a34b93e5647aececeb))
* **search:** stream results in path order, add -l/-c fast paths, AND/NOT queries and match columns ([76d3019](https://github.com/momokun7/xgrep/commit/76d3019658a79ea216c98745636ba32a99a7c536))
* **serve:** watch the tree and skip the freshness probe in long-lived processes ([fe897e0](https://github.com/momokun7/xgrep/commit/fe897e05123ec45ae75c3bf40838b635933687f9))
* **syntax:** tree-sitter outline, definitions, references, block expansion and repo map ([905d0d3](https://github.com/momokun7/xgrep/commit/905d0d36716d1b81dfa85d3d09dae476cfd3843b))


### Bug Fixes

* canonicalize the root before hashing the cache index path ([238334d](https://github.com/momokun7/xgrep/commit/238334dc01c28340a618bb292e24623707158c3e))
* **ci:** make watcher tests portable and update audited dependencies ([dc81e35](https://github.com/momokun7/xgrep/commit/dc81e35c44a9f1d391ce04946f1abc114918e8a3))
* **cli:** aggregate --in-diff -c counts per path, not per run ([14a0227](https://github.com/momokun7/xgrep/commit/14a0227e1405f389ae963b2c700942a593e7112e))
* **cli:** apply --exclude before --max-count ([c585973](https://github.com/momokun7/xgrep/commit/c5859732364c20439a5160b07886cdadb5fc1f50))
* eliminate false negatives in regex trigram extraction ([5043bfc](https://github.com/momokun7/xgrep/commit/5043bfc074d8af5eb8a4f9100d6385309a90bdd7))
* **freshness:** detect new files in directories without indexed files ([925ce99](https://github.com/momokun7/xgrep/commit/925ce99153c46f3ec1c6bf617b456613bef0f73c))
* **freshness:** share the builder's internal-file predicate with the probe and watcher ([9f0b21e](https://github.com/momokun7/xgrep/commit/9f0b21e0c80bc17b7d439b788b10dcc619623f90))
* **git:** read NUL-separated git output so non-ASCII paths work ([bc92c12](https://github.com/momokun7/xgrep/commit/bc92c122cd2d9c4ed47ac25bbb6d600dadf9fd74))
* harden index and file access paths ([4664a55](https://github.com/momokun7/xgrep/commit/4664a557f7fb82fd1c52e700701ebe1601d9cff1))
* harden index and file access paths ([3b1c979](https://github.com/momokun7/xgrep/commit/3b1c9796a8f012086a12a24f281ab6a6ec033531))
* **index:** catch same-second edits and harden/speed up index builds ([3c26d63](https://github.com/momokun7/xgrep/commit/3c26d6387466e1125e34b1804097e0d182213e08))
* keep default searches fresh and rebuild the index in-process ([2bdf4c1](https://github.com/momokun7/xgrep/commit/2bdf4c132aa456261b7c8fed3e8afc29d59332e3))
* make -w work for patterns with non-word edges ([9831c0b](https://github.com/momokun7/xgrep/commit/9831c0bfe97489799b514ae39c6128277382a087))
* match regex ^/$ per line and run regex on raw bytes ([8510f3c](https://github.com/momokun7/xgrep/commit/8510f3c629814ca6cc6728fbe96db6b6383f35c7))
* **mcp:** mark only emitted lines as shown when a block is hard-cut ([45a6d00](https://github.com/momokun7/xgrep/commit/45a6d00a862be227e7245dc1eb7228a310fe5370))
* never fail context/llm output on non-UTF-8 or unreadable files ([f413c12](https://github.com/momokun7/xgrep/commit/f413c126d74110493e40d45f3db1213f7d41fa3d))
* **search:** warn about a full scan only for single-byte literals ([f831c4d](https://github.com/momokun7/xgrep/commit/f831c4dc81fe6aa2fc55de1d52eedc619ebdf338))
* **syntax:** re-verify racily clean tag cache entries by content hash ([efd4865](https://github.com/momokun7/xgrep/commit/efd4865366b07f7e5483be72bdcf354c969d57d5))
* use `/`-separated relative paths on every platform ([3b04998](https://github.com/momokun7/xgrep/commit/3b049982764870c627dcd27c2a7a2c8a75a2cfdb))
* use ASCII-only pattern folding for smart-case search ([25f7ca4](https://github.com/momokun7/xgrep/commit/25f7ca443a034458c017cf838a2b8102d8194200))
* **watch:** don't trust the watcher's change set without a fence ([6cff448](https://github.com/momokun7/xgrep/commit/6cff44872add4bfea64a2ca39484509b400361de))
* **watch:** stop fencing after setup failure, probe after ignore-rule changes, sync only on post-setup builds ([6c8fb16](https://github.com/momokun7/xgrep/commit/6c8fb16b8f7642026100be80813b0ad4c9a232fc))


### Performance Improvements

* **index:** replace git-status freshness probe with a parallel stat scan ([89565c1](https://github.com/momokun7/xgrep/commit/89565c1f976eede3bebdd29bfeb1328fe53791e7))
* make context output linear in the number of matches ([04bc435](https://github.com/momokun7/xgrep/commit/04bc435c60a31628023eb3a71871f1809d0ddb39))
* push path filters and max_count down into the scan ([64d6bec](https://github.com/momokun7/xgrep/commit/64d6bec2d26a22cb6cc5b804c2123fc9cdc4504f))
* search whole buffer and map lines incrementally in matchers ([01faf09](https://github.com/momokun7/xgrep/commit/01faf097550be1b5732d0e73fea271b4cca68b79))
* **syntax:** fast repo_map on large repos and journaled tags cache ([2d0f491](https://github.com/momokun7/xgrep/commit/2d0f49126dde785b8b19f67e1161a461ab7dfdb8))
* **trigram:** dedup trigrams with a reusable bitset instead of HashSet ([b924ba1](https://github.com/momokun7/xgrep/commit/b924ba1c557975a217071e2f90e25678067c996d))
* **trigram:** reuse per-thread scratch for trigram extraction ([4dae4ba](https://github.com/momokun7/xgrep/commit/4dae4ba792c47fdb5362b1502d5f91d2cd01b169))

## [0.7.0](https://github.com/momokun7/xgrep/compare/v0.6.0...v0.7.0) (2026-06-19)


### Features

* add --version flag and optional path argument ([d9c1aee](https://github.com/momokun7/xgrep/commit/d9c1aee9864e7e696cd45e0cb717eca8b27c96b4)), closes [#17](https://github.com/momokun7/xgrep/issues/17) [#16](https://github.com/momokun7/xgrep/issues/16)
* add case-insensitive file type matching ([b6ae906](https://github.com/momokun7/xgrep/commit/b6ae906dc818108267dee126e035e5b300519d82))
* add criterion.rs benchmark infrastructure ([0d079a5](https://github.com/momokun7/xgrep/commit/0d079a518193ef10cf26f3f6e8bb26b94812a40c)), closes [#26](https://github.com/momokun7/xgrep/issues/26)
* add strict input validation to MCP tool handlers ([c406d20](https://github.com/momokun7/xgrep/commit/c406d20a1824ebd0814ee66f55a03373c46989bb))
* add version header to trigram cache format ([bcf0a9d](https://github.com/momokun7/xgrep/commit/bcf0a9d3dd922593d907671d8f2dc66da0fb1d2d))
* rename the crate to xgrep-search and the binary to xg (crates.io release prep) ([55a4d8c](https://github.com/momokun7/xgrep/commit/55a4d8cfde1a0ddf05a2ee77a7a8269b54e97532))
* add bottleneck measurement for distributed patterns ([10181a6](https://github.com/momokun7/xgrep/commit/10181a640cad8843fec725286eacf0c1261f9059))
* add a token limit option to format_llm ([2657002](https://github.com/momokun7/xgrep/commit/265700243da8b5171b9fccf897ea67dcb5b6b5e4))
* update the index incrementally by comparing git commit hashes ([2c5e54e](https://github.com/momokun7/xgrep/commit/2c5e54e3e04e52d3c5bfc86a07e6a6e3abb33d4b))
* public library API in lib.rs (Xgrep struct, SearchOptions, hybrid search) ([1f9f6d6](https://github.com/momokun7/xgrep/commit/1f9f6d60596a0360d3de05802ad15f8ab7fc6407))
* token limit for LLM output (format_llm and the MCP max_tokens parameter) ([2378107](https://github.com/momokun7/xgrep/commit/237810764768f5eff0e0a1da8e52942949472b23))
* add a max_tokens parameter to MCP search (default 4000 tokens) ([1f5b9fa](https://github.com/momokun7/xgrep/commit/1f5b9faef740db32d3048a104d6fb06af1bc2e62))
* add the serve subcommand and message dispatch for the MCP server ([a6d3ea1](https://github.com/momokun7/xgrep/commit/a6d3ea11e6015b65e9fc791fa18cb9cdc6406d16))
* MCP server (search, find_definitions, index_status, build_index) ([101a970](https://github.com/momokun7/xgrep/commit/101a9706e592e6af98f73e69894e6ef491776653))
* add MCP tool handlers (search, find_definitions, index_status, build_index) ([ead3d1f](https://github.com/momokun7/xgrep/commit/ead3d1fdfaf61566477e940c246a0f5572c6ff51))
* parse MCP parameters into serde structs; add max_results to find_definitions ([#55](https://github.com/momokun7/xgrep/issues/55)) ([d2e97ab](https://github.com/momokun7/xgrep/commit/d2e97ab86a5b2a7c780492df9aad3aa9f6390be5))
* add the MCP protocol layer (JSON-RPC parser, response builder, main loop) ([551d8b6](https://github.com/momokun7/xgrep/commit/551d8b6a4f6b6759a2b40e0a2d6688f66f6e4c53))
* harden MCP (jsonrpc validation, read_file tool, more definition regexes, path traversal prevention) ([769e0cc](https://github.com/momokun7/xgrep/commit/769e0cc63147f08ab186935f6056ebd6ab3bccc0))
* add the path_pattern filter and the MCP protocol layer ([895f985](https://github.com/momokun7/xgrep/commit/895f9856492c1c0f39a528b93732099b54123989))
* phase 3 quality work (case-insensitive speedups, non-git directories, UX warnings, regex docs) ([64eb583](https://github.com/momokun7/xgrep/commit/64eb5837e7a6427fb76dfc68b94a430b47022fad))
* narrow visibility to pub(crate); add cargo-fuzz targets (varint, posting list, index reader) ([f12d8a1](https://github.com/momokun7/xgrep/commit/f12d8a1e414146c084c2a8a8c3f5468f83d7d3b2))
* publish the xgrep-search package to PyPI (PyO3 bindings and the xg binary) ([#58](https://github.com/momokun7/xgrep/issues/58)) ([7785704](https://github.com/momokun7/xgrep/commit/7785704dcb2027db7dafad8d9be185bab1b5a93c))
* incremental index updates with a trigram cache, sturdier git status parsing, README update ([e222c07](https://github.com/momokun7/xgrep/commit/e222c076a863fc13dbbaf8eb0e3a4fff4ef8e04e))
* v0.2.0 release ([#24](https://github.com/momokun7/xgrep/issues/24)) ([1306afa](https://github.com/momokun7/xgrep/commit/1306afade6aa975b8343ed12d90129a0ef2b168f))
* v0.4.0 — ripgrep-compatible search UX and AI-friendly API ([#35](https://github.com/momokun7/xgrep/issues/35)) ([1768b78](https://github.com/momokun7/xgrep/commit/1768b78f98244b2b4f9c425ed132e589fb46afac))
* differential updates for the v3 index (two-phase walk, PerFileSection, fingerprint cache) ([e1cccc2](https://github.com/momokun7/xgrep/commit/e1cccc209634b822262c6b6aba48659606a371ef))
* xgrep: an index-based, very fast code search tool ([3bad6c7](https://github.com/momokun7/xgrep/commit/3bad6c711b5f93036ca45e33046daf6e974a86f5))
* add the skeleton of the public Xgrep API (SearchOptions, open, build_index, search) ([b5428a9](https://github.com/momokun7/xgrep/commit/b5428a9e60c4a9f7f39498bca043f5522817ca61))
* convert regexes to trigram queries the way Zoekt does ([8191fe3](https://github.com/momokun7/xgrep/commit/8191fe3d34868336e6da5f7e31d9bfc143e15a79))
* add color output, -c/-l, --max-count and --json ([fa249fa](https://github.com/momokun7/xgrep/commit/fa249fa6dfca734d06d2ee8fff5ec36b7b798d98))
* move hybrid search, automatic builds and git-changed search into the Xgrep API ([1d130b3](https://github.com/momokun7/xgrep/commit/1d130b3b75bdad3d933364f80b0c9c55252c9f99))
* hybrid search without rebuilds (index plus direct search of changed files) ([6661f60](https://github.com/momokun7/xgrep/commit/6661f60de8a9ed08347e77783664e8daa4f4af72))
* add file locking, split search.rs (candidates.rs), add property-based tests ([ee426a0](https://github.com/momokun7/xgrep/commit/ee426a0032076930f9eee842a7d2164285714ef3))
* rebuild the index in the background after a search (at most every 30 seconds) ([a130aec](https://github.com/momokun7/xgrep/commit/a130aecb47bc8e936b95bba8c8fa88c7441ccfce))
* warn on short patterns and regex fallbacks ([5a3dc62](https://github.com/momokun7/xgrep/commit/5a3dc62a040e203a8d0dcb6a13d2c238d3cf5466))


### Bug Fixes

* address code review findings across [#25](https://github.com/momokun7/xgrep/issues/25)-[#29](https://github.com/momokun7/xgrep/issues/29) ([bab8a5e](https://github.com/momokun7/xgrep/commit/bab8a5ee0f5c6011a774688108c2cf29e1b4694b))
* add 4 bincache tests, fix the text-to-binary bug, add the v2 to v3 migration message ([b907864](https://github.com/momokun7/xgrep/commit/b9078642227c7e1681815c5133cff9c6b47124fa))
* replace unsafe raw pointer code in builder.rs with safe slices; prevent infinite recursion when taking the lock ([94d5996](https://github.com/momokun7/xgrep/commit/94d5996a4cb382b048badcec2cc35b9ee88acb8b))
* add cargo publish --allow-dirty, v0.1.4 ([4e6f2a0](https://github.com/momokun7/xgrep/commit/4e6f2a00be279b9bf63d6c2315a210d1976c919c))
* point the crates.io readme at a path inside the package ([cc538fd](https://github.com/momokun7/xgrep/commit/cc538fd6b99bc10564f60bb60cb98d06c4ff1fe9))
* exclude index-related files from newest_file_mtime detection ([c5d0347](https://github.com/momokun7/xgrep/commit/c5d034793c7a01fff5ba138009706ceb6168d8b9))
* include the old name of a git rename in changed_set to drop stale results ([fb0d75c](https://github.com/momokun7/xgrep/commit/fb0d75cc663938eaffbca9b9e8b2b2acbbda7143))
* split git status -uall to avoid hangs in large repositories ([1dc83dc](https://github.com/momokun7/xgrep/commit/1dc83dc7128a3e7c2b1c6657b2386db7af70ffc1))
* sturdier git status parsing, tests for uncommitted changes, README update ([e6eae65](https://github.com/momokun7/xgrep/commit/e6eae65950ea2d9d788a15210127560b4892e65d))
* GitHub Actions on Node 24, housekeeping, move to Arc&lt;str&gt; ([#49](https://github.com/momokun7/xgrep/issues/49)) ([8aae6f6](https://github.com/momokun7/xgrep/commit/8aae6f68e58d8a64016c827207081b1184622152))
* raise MSRV to 1.85 (ignore 0.4.25 uses edition 2024) ([5ff0f69](https://github.com/momokun7/xgrep/commit/5ff0f69a796da9744666713372851de0c83438c2))
* lower MSRV from 1.85 to 1.74 (replace is_none_or with map_or) ([3e3029a](https://github.com/momokun7/xgrep/commit/3e3029ae396d8d55c59c816b13de06d1054fb535))
* keep pub mod mcp in alphabetical order (lint fix) ([cca5408](https://github.com/momokun7/xgrep/commit/cca5408976833ccbd52f26188859332e68d395c1))
* use the canonical path in read_file, merge mcp_server, better token estimates ([a2b7be3](https://github.com/momokun7/xgrep/commit/a2b7be316884b09e51a4b77145e58c6c4321cca5))
* fix a read_file panic on empty files; widen posting_total_bytes to u64 ([05b2d61](https://github.com/momokun7/xgrep/commit/05b2d6169a455846c83dc839f4947a37c54838a4))
* fix release.yml (macos-13 to macos-latest, publish-crate no longer waits for builds) ([3c88b8a](https://github.com/momokun7/xgrep/commit/3c88b8af26e02c63fd1aa9f11331e3df5066fc3e))
* resolve path doubling in --fresh and --changed for git subdirectories ([9b92a7f](https://github.com/momokun7/xgrep/commit/9b92a7fac0502479c648278fdeb79532143abc1d)), closes [#15](https://github.com/momokun7/xgrep/issues/15)
* fix RUSTSEC-2026-0097 (update rand) and pin pip install hashes ([#63](https://github.com/momokun7/xgrep/issues/63)) ([4551e4f](https://github.com/momokun7/xgrep/commit/4551e4fbc32c9d47de9e5537fa2ac4b78a98f1b8))
* make sed -i work on macOS (-i.bak) ([f24ef35](https://github.com/momokun7/xgrep/commit/f24ef3523b70aa8114fe56b30e873ee32a60a62f))
* deduplicate hybrid search results for stale paths ([18812c0](https://github.com/momokun7/xgrep/commit/18812c04b020ae81c79bc0225725af5c0b2ba0c5))
* suppress stderr output in MCP server mode ([1bd8da5](https://github.com/momokun7/xgrep/commit/1bd8da542784b5c16ae34f0167d1991e6a341d96))
* make the TrigramCache entry count exact by subtracting skipped entries ([c7ed70f](https://github.com/momokun7/xgrep/commit/c7ed70fec0c03b7819b3e98877caded0399d4df0))
* use tempdir instead of NamedTempFile in cache tests for CI stability ([44bdd1a](https://github.com/momokun7/xgrep/commit/44bdd1a1d88710788e2ad85049ce4f4c155c7d0e))
* handle Windows mtime precision (sleep 1s to 2s) ([f95cb00](https://github.com/momokun7/xgrep/commit/f95cb000e954b9870f04934426228250d4f5529e))
* Windows support (mtime fallback for stale lock recovery, cfg(unix) tests) ([21287fe](https://github.com/momokun7/xgrep/commit/21287fe6403f313af565f56e1434fd3686ba7796))
* replace the index file atomically on build ([fba3ee6](https://github.com/momokun7/xgrep/commit/fba3ee6d34a53ef5d028c5bdcd4ba1fcdcaeaa37))
* fix the index format to little endian and remove unsafe read_unaligned ([9110009](https://github.com/momokun7/xgrep/commit/9110009f97c36ea2835503a57583bd4f9ce3d9ba))
* second review round (remove unsafe, SIMD case-insensitive search, lock recursion fix, unified intersect, tests, docs and CI) ([0ea10a3](https://github.com/momokun7/xgrep/commit/0ea10a3f2af65769dbd72f87ee54d20bfdf137bb))
* first review round (O(n*m) line numbering fix, format docs, merged git calls, separate cache, canonicalize fix, MCP merge, better token estimates) ([3f83b4a](https://github.com/momokun7/xgrep/commit/3f83b4a94a45998528e0b5631aed579ff37c8f05))
* fix crashes (prefix bounds check, empty-file panic, varint overflow, u64 postings, atomic writes) ([508531e](https://github.com/momokun7/xgrep/commit/508531e25c38dfce9c7f90ba81554222faf1c242))
* fix the SIGPIPE panic on a broken pipe ([91116c4](https://github.com/momokun7/xgrep/commit/91116c410c4e2424896f4e83b5601e6c61ca87c0))
* fix package registry metadata (PyPI stuck at 0.5.0, no README on crates.io) ([fe1b355](https://github.com/momokun7/xgrep/commit/fe1b3558536eb85e8397fec3c0412e9abd382b71))
* fix stale results for renamed files; prevent integer overflow (checked arithmetic) ([cb4ae08](https://github.com/momokun7/xgrep/commit/cb4ae08a87549e02723112a364340390804fec41))
* third review round (smoke test, MSRV 1.82, no duplicate git calls, cache overflow guard) ([9b6aaf3](https://github.com/momokun7/xgrep/commit/9b6aaf345a54d53b22e84d83b15f7d49f780a14f))
* third review round (fix smoke test, raise MSRV to 1.82, no duplicate git runs, cache overflow guard) ([5e529e2](https://github.com/momokun7/xgrep/commit/5e529e2afd5348ef1335749b6e9399fd106ea494))
* reliability work (fixed endianness, remove unsafe, fix BUG-5/PERF-1, harden MCP, add read_file) ([e129961](https://github.com/momokun7/xgrep/commit/e12996152388ee595db62890bfdbe8bb827f4015))
* internal quality (atomic writes, skip binaries, posting_total_bytes in the header, non-ASCII warning) ([05f5df8](https://github.com/momokun7/xgrep/commit/05f5df8f8c78e47555547245604e5f6c05121395))
* internal quality (atomic writes, skip binaries, posting_total_bytes in the header, non-ASCII warning) ([940767f](https://github.com/momokun7/xgrep/commit/940767fdbe8e1eca53cb9836b763b2ec09d05ed2))
* fix 4 critical/high bugs found in an audit ([71359a3](https://github.com/momokun7/xgrep/commit/71359a3d0bd30f658c87ed66f0b9b6ecd7775e6b))
* fix 6 low-severity bugs found in an audit ([45d3ee2](https://github.com/momokun7/xgrep/commit/45d3ee2ce66b784d163b1b3a37fb4778875b9e65))
* fix 5 medium-severity bugs found in an audit ([2049a97](https://github.com/momokun7/xgrep/commit/2049a97e31305da10fef92b58de4b27cc3c5568d))


### Performance Improvements

* speed up 2-character patterns with trigram prefixes ([7c3c5a4](https://github.com/momokun7/xgrep/commit/7c3c5a4a759405f98a7977a0b8255ede837ce556))
* benchmark BTreeSet vs HashSet for trigram extraction ([fbfd79b](https://github.com/momokun7/xgrep/commit/fbfd79be58da27c47b2705e9378a6cb7eda6fff6))
* fall back early instead of expanding too many case variants ([81e6099](https://github.com/momokun7/xgrep/commit/81e6099f2b4e2960217789ce3a74a4e271f3206b))
* enumerate case variants for case-insensitive search the way Zoekt does ([6953867](https://github.com/momokun7/xgrep/commit/6953867c5ab5fa4d6955cfd8cf3aa83c88d8dce2))
* use memmem SIMD search in CaseInsensitiveMatcher (replaces naive comparison) ([8534dcb](https://github.com/momokun7/xgrep/commit/8534dcbf71c6065c4adbd91a8831c9674eb1e4e7))
* fast path in check_index_status (skip git ls-files --others when the commit hash is unchanged, 170ms saved) ([8b166e0](https://github.com/momokun7/xgrep/commit/8b166e00189f6d688e426a3012f5f04f38532718))
* cut build memory from 983MB to under 90MB with counting sort and mmap ([7c02c0a](https://github.com/momokun7/xgrep/commit/7c02c0a3fdfd38f0b5c8c308f63e590a0e841b81))
* optimize the binary with LTO, mimalloc and codegen-units=1 ([702e25c](https://github.com/momokun7/xgrep/commit/702e25cf88ebdb9eed6f9975d564b684a3b18958))
* optimize RegexMatcher and CaseInsensitiveMatcher hot paths ([6379546](https://github.com/momokun7/xgrep/commit/6379546b412467378ff00a059935d4428ebba205)), closes [#27](https://github.com/momokun7/xgrep/issues/27)
* build the index in parallel with rayon ([e3ba4f0](https://github.com/momokun7/xgrep/commit/e3ba4f0c8bad25e9ee15ba86d49dcdc6a3a503db))
* restore search speed (no intermediate Vec, lazy line_offsets, no chunking) ([40c796c](https://github.com/momokun7/xgrep/commit/40c796c157029dfbd1c2c9186b2e9f4e167417dc))
* restore search speed (no intermediate Vec, lazy line_offsets), update benchmark numbers, doc comments in English ([1c04251](https://github.com/momokun7/xgrep/commit/1c04251fba8cbacf525cac19df8de2f584cf6f87))
* v3 differential update, bincache and sorted insert take Case3 from 15s to 1.6s (warm) ([93e7b72](https://github.com/momokun7/xgrep/commit/93e7b722fb9fc2952e1db29b04e9c70036781fd8))
* return early when the corpus fingerprint is unchanged (step 1) ([99e7ce6](https://github.com/momokun7/xgrep/commit/99e7ce6504399b4a69e5d1beb96cf485308e130d))
* reduce index build memory by processing in chunks ([f36d7df](https://github.com/momokun7/xgrep/commit/f36d7dfad64dae923ae11e8ae8af8e4bafb394ab))
* skip the freshness check by default (opt in with --fresh): 37ms, 55x faster than ripgrep ([fada96d](https://github.com/momokun7/xgrep/commit/fada96df54eaa5f2f0e205be6fad64d53bbd59ac))
* fix all bottlenecks (BN-1 to BN-10) ([0e2cf4f](https://github.com/momokun7/xgrep/commit/0e2cf4f6dc00f76162da3d723147857dbda94407))
* speed up differential updates with sorted insert and a binary cache (Case3: 9.05s to 1.6s) ([8b0d3f6](https://github.com/momokun7/xgrep/commit/8b0d3f67e85f1c04e188cd35a32c01b0adf21d72))
* bound search memory by processing in chunks ([6648f11](https://github.com/momokun7/xgrep/commit/6648f11995b8b6ac73b72602ac9c1de47b64d23c))

## [0.6.1](https://github.com/momokun7/xgrep/compare/v0.6.0...v0.6.1) (2026-06-17)


### Bug Fixes

* fix RUSTSEC-2026-0097 (update rand) and pin pip install hashes ([#63](https://github.com/momokun7/xgrep/issues/63)) ([4551e4f](https://github.com/momokun7/xgrep/commit/4551e4fbc32c9d47de9e5537fa2ac4b78a98f1b8))

## [0.6.0](https://github.com/momokun7/xgrep/compare/v0.5.0...v0.6.0) (2026-06-16)


### Features

* publish the xgrep-search package to PyPI (PyO3 bindings and the xg binary) ([#58](https://github.com/momokun7/xgrep/issues/58)) ([7785704](https://github.com/momokun7/xgrep/commit/7785704dcb2027db7dafad8d9be185bab1b5a93c))

## [0.5.0](https://github.com/momokun7/xgrep/compare/v0.4.2...v0.5.0) (2026-06-16)


### Features

* parse MCP parameters into serde structs; add max_results to find_definitions ([#55](https://github.com/momokun7/xgrep/issues/55)) ([d2e97ab](https://github.com/momokun7/xgrep/commit/d2e97ab86a5b2a7c780492df9aad3aa9f6390be5))

## [0.4.2](https://github.com/momokun7/xgrep/compare/v0.4.1...v0.4.2) (2026-06-16)


### Bug Fixes

* GitHub Actions on Node 24, housekeeping, move to Arc&lt;str&gt; ([#49](https://github.com/momokun7/xgrep/issues/49)) ([8aae6f6](https://github.com/momokun7/xgrep/commit/8aae6f68e58d8a64016c827207081b1184622152))
