# 審查的實驗素材

這次審查用來重現問題與量測的腳本、harness 與紀錄，原本放在 `.scratch/`。那個目錄可能被清掉，所以搬到這裡，納入版本控制。

規則：

- 下面每一條指令，都在 **repo 的根目錄**執行。
- 建置結果與工作目錄一律放在 repo 根目錄的 `.scratch/` 下（已被 `.gitignore` 涵蓋）。用完可以刪掉，需要時再重建。
- 這裡的檔案，只是當時發現問題的工具與證據。修好之後的回歸測試，應依各項目的「要加的測試」寫進 `src/` 或 `tests/`，不要依賴這裡的檔案。
- 三個 Rust crate 各有自己的空 `[workspace]`，並以 path 相依於 repo 的 ferry。它們不是 ferry workspace 的成員，所以 `make check` 不會建置它們。

| 位置 | 內容 | 相關項目 |
|---|---|---|
| `arch/prof.py` | 彙整 `--profile-file` 的 JSON：每個 role 的忙碌與等待時間 | ARC-01、ARC-04、ARC-05 |
| `arch/dirbench.py` | 目錄鎖的微基準：暫存名稱加 rename，對照 `O_TMPFILE` 加 `linkat`（Linux） | ARC-01 |
| `arch/profiles/` | 架構量測的 profile：`small.json`（20 萬個 4 KiB 檔，`--no-fsync`）、`small-fsync.json`（同上，含 fsync）、`big.json`（1 個 4 GiB 檔） | ARC-01、ARC-04 |
| `destination/exp/` | 把手工組出的 packet 直接餵給 `destination::run` | DST-02、DST-05、DST-08 |
| `protocol/harness/` | 以手工組出的串流驅動 `destination::run` 與 `source::run`，情境 A 至 G | PRO-01 至 PRO-08 |
| `source/mk.py` | `python3 -I mk.py ROOT DIRS PER`：建立 DIRS 個目錄，每個 PER 個空檔案 | SRC-02、SRC-04 |
| `source/repro-*.sh` | SRC-01、SRC-02、SRC-04、SRC-05、SRC-07 的重現腳本 | 見各腳本開頭 |
| `source/poolpanic/` | worker panic 之後 pool 停滯的重現 | SRC-03 |
| `source/records/` | 當時實驗的 profile 與 stderr：`fail.json`（SRC-02）、`wide.json`（SRC-04）、`deep.err`（SRC-07）、`fail.err`、`fifo.err`（SRC-01） | — |
| `transport/bin/fakessh`、`transport/bin/fwd.py` | 在本機執行一切的假 ssh，透過 `FERRY_SSH` 使用；`DELAY` 秒會延後 server 的啟動 | TRN-01、TRN-05 |
| `transport/release_script.sh` | 3c17a2f 的 `release_script` 產生的安裝腳本，host 的目錄改成參數 | TRN-02 |

## 常用指令

```sh
# Rust harness：共用一個 target 目錄
export CARGO_TARGET_DIR=$PWD/.scratch/review-materials/target
cargo build --release --manifest-path docs/review/2026-10-17/materials/destination/exp/Cargo.toml
cargo build --manifest-path docs/review/2026-10-17/materials/protocol/harness/Cargo.toml
cargo run --release -q --manifest-path docs/review/2026-10-17/materials/source/poolpanic/Cargo.toml
# 修正前印出 HANG: the pool did not finish within 5 s after a worker panicked

# Source 的重現腳本：工作目錄建在 .scratch/ 下，結束時刪除
cargo build --release
FERRY=$PWD/target/release/ferry sh docs/review/2026-10-17/materials/source/repro-depth-fds.sh 400 256

# profile 的彙整
python3 -I docs/review/2026-10-17/materials/arch/prof.py docs/review/2026-10-17/materials/arch/profiles/small.json
```

沒有保留的：

- `small-*.err`：DST-03 的 fd 上限實驗，每個約 500 KB，內容是重複的 `Too many open files`。
- `deep.out`：SRC-07 的 dry run 清單。
- 各次的建置結果、模擬 host 的目錄與測試資料。

它們的結論都已寫在各項目裡。
