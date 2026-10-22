# Ferry 審查交接：CLI/UX、測試、CI/release、文件

> 2026-10-22：已完成的項目（29 項）移到 [done/cli-tests-ci-docs.md](done/cli-tests-ci-docs.md)，這裡只留還沒完成或決定不做的。

本文件給之後負責修正的 coding agent 使用。每一項都可以獨立閱讀，不需要看過原本的審查對話。

- 基準版本：`main` 的 HEAD `3c17a2f`（chore: version 0.14.4）。所有 `path:line` 都對照過這個版本。
- 實測環境：Linux x86_64，rustc 1.95.0，debug build（`CARGO_TARGET_DIR=.scratch/review-cli/target`）。所有測試目錄樹都放在 `.scratch/review-cli/` 底下，用完已刪除。
- 「fake ssh」指 `tests/remote.rs:15-24` 的做法：`FERRY_SSH` 指向一個 shell script，略過 ssh 的選項與 host，在本機以 `sh -c` 執行命令。搭配 `--remote-ferry <本 build>` 與 `--remote-join`，不需要真的 host 就能走完遠端流程。
- 不在本文件範圍：IPv6 `[::1]:path` 的解析，以及 release 的簽章與 provenance。這兩項由 transport 審查者負責，本文件只在 CI-07 留交叉參照。
- 用語依照 `docs/design/*.zh-TW.md`：client、server、peer、Source、Destination、manifest、record、frame、packet、thread、stage、connection、host，以及程式識別字、路徑與指令一律保留英文。handshake 寫作「交握」。

## 索引

| ID | 嚴重度 | 標題 |
|---|---|---|
| CI-04 | 低 | 沒有 MSRV，toolchain 浮動，release 不可重現 |
| CI-05 | 低 | 推 tag 就 release，不要求該 commit 的 CI 已通過 |
| CI-07 | 中 | （交叉參照）release 沒有簽章與 provenance |

文末另有「其他小問題」。

---

## CLI/UX

## 測試

## CI/release

### CI-04：沒有 MSRV，toolchain 浮動，release 不可重現

- 嚴重度：低
- 驗證狀態：已核對程式碼與設定。
  - `Cargo.toml` 沒有 `rust-version`。
  - `rust-toolchain.toml` 是 `channel = "stable"`。
  - 程式使用 edition 2024 與 let chains（例如 `src/cli.rs:573-575`、`src/dashboard.rs:144-147`），需要 Rust 1.88。`std::io::pipe()` 需要 1.87。
  - 依賴中 `ratatui 0.30.2` 宣告 `rust-version = "1.88.0"`，`clap 4.6.7` 與 `toml 1.1.8` 宣告 1.85。
- 位置：`Cargo.toml:1-6`、`rust-toolchain.toml`、`.github/actions/setup-rust/action.yml:22-29`、`Makefile:58-60`、`build.rs`。
- 現象：
  1. 使用者以較舊的 Rust 執行 `cargo install --path .` 時，會看到一堆「let chains are unstable」之類的錯誤，而不是清楚的「需要 rustc 1.88」。
  2. dev build 在 host 上用 host 的 cargo 建置（`docs/cli.md` 的「Installing on a host」第 4 點）。host 若是發行版自帶的 cargo，沒有 rustup，`rust-toolchain.toml` 就沒有作用，同樣得到難懂的錯誤。
  3. release 用的是 tag 當天的 stable，不同時間重建同一個 tag，會得到不同的二進位檔。二進位檔中也嵌入了 `/home/runner/...` 這類絕對路徑（panic 訊息、debug 資訊），無法逐位元組重現。
- 根本原因：沒有宣告 MSRV，也沒有固定 toolchain 版本。
- 修法：
  1. `Cargo.toml` 加 `rust-version = "1.88"`。用 `cargo msrv verify`，或實際以 `cargo +1.88 check --locked --all-targets` 確認；必要時調高。
  2. CI 新增 `msrv` job：`rustup toolchain install 1.88 --profile minimal` 後 `cargo +1.88 check --locked --all-targets`。
  3. `rust-toolchain.toml` 改成固定版本，例如 `channel = "1.95.0"`，由 Dependabot 或手動升級。若想保留「開發者用最新 stable」，至少讓 release workflow 明確安裝固定版本並 `rustup override set`。
  4. 可重現性：在 `make release` 加上 `RUSTFLAGS="--remap-path-prefix=$(CURDIR)=. --remap-path-prefix=$(HOME)/.cargo/registry/src=/registry"`，加上固定的 toolchain，加上不使用快取（CI-03）。之後可以在 release job 中 build 兩次並比較 hash 來驗證。macOS 的 linker 會嵌入 UUID 等資訊，可能需要 `-C link-arg=-Wl,-no_uuid`，需實測。
  5. 注意：`build.rs` 會把 `rust-toolchain.toml` 納入 source hash（`src/build_info/source.rs:14-19`）。固定版本後，每次升級 toolchain 都會改變 dev build 的 identity，這是可以接受的。
- 要加的測試：`msrv` job。
- 驗收條件：
  - [ ] 以 1.87 建置時，cargo 直接報出需要 1.88。
  - [ ] release workflow 使用固定的 toolchain 版本，並記錄在 log 中。
  - [ ] （選做）同一個 tag build 兩次，Linux 二進位檔的 hash 相同。
- 文件要同步的地方：`README.md` 的「From source」節加上「Needs Rust 1.88 or later.」；`docs/cli.md` 的「Installing on a host」第 4 點註明 host 的 cargo 版本需求，`docs/cli.zh-TW.md` 同步。
- 相關項目：CI-03。

### CI-05：推 tag 就 release，不要求該 commit 的 CI 已通過

- 嚴重度：低
- 驗證狀態：已核對設定。`release.yml:6-8` 在 `push: tags: ['v*']` 時直接執行，build job 只跑 `make test TARGET=…`（`release.yml:47`），不跑 distro、filesystem、lint、shellcheck，也不檢查該 commit 在 `main` 上的 CI 結果。
- 位置：`.github/workflows/release.yml:6-62`。
- 現象：在一個 CI 失敗的 commit 上打 tag，例如 XFS 上的測試失敗，或 Rocky 8 上 glibc 的問題，仍然會發布 release。release client 會把它安裝到使用者的 host。
- 修法：
  - 建議：在 build job 之前加一個 `gate` job，用 `gh api repos/$GITHUB_REPOSITORY/commits/$GITHUB_SHA/check-runs --jq '.check_runs[] | select(.name=="CI") | .conclusion'` 確認 `CI` 這個 check 是 `success`，否則失敗。
  - 也可以在 `release.yml` 用 `workflow_call` 直接重用 `ci.yml` 的 jobs，代價是 release 時間變長。
  - 另外檢查 tag 所指的 commit 在 `main` 上：`git merge-base --is-ancestor $GITHUB_SHA origin/main`。
- 要加的測試：無。
- 驗收條件：
  - [ ] 在 CI 失敗的 commit 上打 tag，release workflow 會在 build 之前失敗並說明原因。
- 文件要同步的地方：無；可在 `TODO.md` 步驟 28 的敘述補一句。
- 相關項目：CI-07。

### CI-07：（交叉參照）release 沒有簽章與 provenance

- 嚴重度：中
- 驗證狀態：已核對設定。`SHA256SUMS` 與二進位檔放在同一個 GitHub release（`release.yml:79-90`），只能防止傳輸損壞，不能防止 release 被替換。`ferry update`（`src/update.rs`）、在 host 上安裝的流程（`src/remote/release.rs`）與 `install.sh` 都信任它。
- 這一項由 transport 審查者負責，詳細設計見該份報告：GitHub artifact attestations、minisign 或 cosign 簽章，以及把公鑰嵌進二進位檔讓 `ferry update` 驗證。本文件不重複。
- 與本文件的接點：
  - 做 attestation 時，build job 需要 `permissions: id-token: write, attestations: write`。只在需要的 job 開啟，維持 `ci.yml:16`、`release.yml:10` 預設 `contents: read` 的做法。
  - 新增的 action（例如 `actions/attest-build-provenance`）同樣要以 SHA 固定（CI-03）。
  - CI-02 產生的第三方授權檔也應納入 `SHA256SUMS` 與簽章範圍。
- 相關項目：CI-02、CI-03、CI-05、CI-06。

---

## 文件

## 其他小問題

1. `scripts/bench.sh:76`、`83`、`98`：遠端命令中的 `$remote_dir` 沒有加引號，例如 `ssh "$host" "rm -rf $remote_dir/out"`。路徑含空白或特殊字元時，會刪錯地方。用 `printf %q`，或比照 `remote::shell::quote` 的方式加引號。（低）
2. `src/update.rs`：`replace()` 在 rename 前沒有對新檔與目錄 fsync。斷電後可能留下一個 0 bytes 的 `ferry`。加上 `File::sync_all()`，並在 rename 後對目錄 fsync。（低）
3. `src/dashboard.rs:302`：清單面板用 `from_utf8_lossy`，再經 ratatui 過濾掉控制字元，與 stdout 的輸出不一致。這一點在 CLI-03 一併處理。（低）
4. `ferry sync a a`（同一目錄）exit 0 且什麼都不做。CLI-01 會改為拒絕。（低）
5. 儀表板的 panic hook（`src/dashboard.rs:105-109`）在 `show()` 結束後沒有還原成原本的 hook。因為 `ACTIVE` 會變成 false，所以無害；若之後有程式碼依賴 panic hook，要記得這一點。（低）
6. `.github/workflows/ci.yml:141`：`ci` job 用 `ubuntu-latest`，與檔案中「labels are pinned」的方針不一致。已併入 CI-03。（低）

## 做得好的地方（修改時請保留）

- 終端機一定會被還原：panic hook、signal 結束路徑（`interrupt::on_exit`），以及 raw mode 下自行處理 Ctrl-C，都會呼叫 `restore()`。`NO_COLOR` 也有實作。
- signal 由專責的 `sigwait` thread 處理，暫存檔登記表以 gate 保護，以 128 + signal 結束，並寫在文件中。
- CI 的基本功紮實：
  - 最上層 `permissions: contents: read`，每個 job 都有 `timeout-minutes`；
  - `cargo fetch --locked` 提早發現 lock 檔過期；
  - 一個 `always()` 的彙總 check；
  - 固定的 runner label；
  - distro（Rocky 8、Debian 13、Fedora）與 filesystem（xfs、btrfs、ext4-casefold）矩陣；
  - release 檢查 tag 與版本一致、Linux 二進位檔是靜態連結、`install.sh` 實際安裝一次。
- 遠端測試用 fake ssh 與 fake sudo，不需要網路。對時序敏感的測試多採用寬鬆的「patient」上限（例如 `src/transport/heartbeat.rs:243-247`）。
- 解碼器都有長度上限；docs 之間的連結與錨點全部有效（以腳本檢查過）；zh-TW 翻譯與英文版在同一個 commit 中同步更新。
