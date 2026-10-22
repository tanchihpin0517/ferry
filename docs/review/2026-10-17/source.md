# Source 端審查：交接文件

> 2026-10-22：已完成的項目（13 項）移到 [done/source.md](done/source.md)，這裡只留還沒完成或決定不做的。

範圍：`src/source.rs`、`src/scan.rs`、`src/pool.rs`、`src/mux.rs`（傳送端的排程）、`src/interrupt.rs`、`src/progress.rs`、`src/profile.rs`，以及它們之間的並行。另外接手 protocol 審查者的兩項：Source 的 file sender 從不送出 `Control::Error`（併入 SRC-03），以及失敗後 Source 仍掃描整棵目錄樹時、Source 不再讀取自己的輸入（併入 SRC-02）。Destination 批次中每個檔案佔 3 個 fd、在 `RLIMIT_NOFILE` 偏低時幾乎每個檔案都失敗的問題，由 Destination 審查者負責，這裡不重複；SRC-07 只指向它，共用同一個修法。

基準：HEAD `3c17a2f`（工作目錄乾淨）。所有行號都已對照這個 commit 重新核對。

實測環境：Linux 5.15，x86_64，96 核心，release build（`CARGO_TARGET_DIR=.scratch/review-source/target`，用完已刪除），兩側都在本機，目錄樹在 `/home`（md RAID，ext4）。

## 腳本與實驗素材

都在 [`materials/source/`](materials/source/)，也就是 `docs/review/2026-10-17/materials/source/`。執行腳本前先 build，並設 `FERRY=<ferry 執行檔>`。腳本把工作目錄建在 repo 根目錄的 `.scratch/` 下，結束時刪除。下表的 `*.err` 與 JSON 在 `materials/source/records/`。

| 檔案 | 用途 | 能否作為回歸測試的基礎 |
|---|---|---|
| `mk.py` | `python3 -I mk.py ROOT DIRS PER`：建立 DIRS 個目錄、每個 PER 個空檔案 | 可當量測工具；回歸測試應改用 Rust 在 `tempfile` 中產生，或用 `ferry fixture generate`，不要依賴 Python |
| `repro-fifo.sh` | SRC-01、SRC-05：copy 開始 0.4 s 後把一個需要的檔案換成 FIFO，75 s 後送 SIGQUIT 取得快照 | 只適合手動重現（需 30 萬個檔案、約 90 s）。回歸測試請用 SRC-01 的單元測試 |
| `repro-scan-after-failure.sh` | SRC-02：Destination 在根目錄就失敗，再從 `--profile-file` 看 Source 掃描了多少 | 是整合測試的雛形；改寫成 `tests/copy.rs` 時要縮小目錄樹並加 `--paced-bytes` |
| `repro-backlog.sh` | SRC-04：一百萬個空檔案、100 個目錄 × 10,000，讀出 `needed files` gauge 的峰值 | 是驗收量測的基礎；太大，不適合放進 `cargo test`。小規模版本見 SRC-04 的測試 |
| `repro-depth-fds.sh` | SRC-07：400 層深、每層有兄弟目錄的目錄樹，在 `ulimit -n 256` 下 dry run | 可改寫成 `tests/copy.rs` 的整合測試（見 SRC-07） |
| `poolpanic/` | 一個依賴 `ferry`（path）的小 crate：`pool::ordered(8 thread, 容量 64)`，worker 在第 3 項 panic，5 s watchdog | 只在選擇 SRC-03 的選項 A（`catch_unwind`）時可直接改成 `pool.rs` 的單元測試；採用建議的選項 B 時，回歸測試改為整合測試（見 SRC-03） |
| `records/`：`fail.json`、`wide.json`、`deep.err`、`fail.err`、`fifo.err` | 上述實驗的 profile 與 stderr 原始輸出（各約 500 KB 的 `small-*.err` 沒有保留） | 只是紀錄 |

## 總覽

| ID | 標題 | 嚴重度 | 驗證 |
|---|---|---|---|
| SRC-11 | 大於 1 MiB 的檔案全由 file sender 一個 thread 讀取並雜湊 | 低 | 已核對程式碼 |

---

## 較小的項目

### SRC-11：大於 1 MiB 的檔案全由 file sender 一個 thread 讀取並雜湊
- 嚴重度：低（效能）。驗證狀態：已核對程式碼，沒有量測。
- 位置：`src/source.rs:716-723`（`size > SMALL` 時回傳 `Ready::Large`）、`src/source.rs:670-679`（sender 自己呼叫 `write_record`）。
- 說明：一棵全是 2 到 10 MiB 檔案的目錄樹（例如照片），16 個 reader 只負責開啟檔案，讀取與 BLAKE3 雜湊全在 file sender 這一個 thread 上依序進行。在 NFS 上，每次 256 KiB 的讀取都要等一次延遲。這與 pipeline.md 中「雜湊因平行化而受益大」的說法不符。
- 修法方向，需先量測：讓 reader 也預先讀取大檔案的前幾個區塊；或者把「1 MiB」改為「最多 N 個、總共 M MiB 的預讀」，例如以 8 個 2 MiB 的區塊為上限，用 bounded 的 per-file channel 交給 sender，sender 只負責依序把區塊放進串流。

## 給記憶筆記的更正

- `Known` 不再是永不修剪：4086576 之後，以 `Needs::Through` 修剪。
- Destination 的 decoded-data channel 已有上限（f3f6653，`DECODED = 8`），報告的 channel 也已有上限（099b1d1，16）。
- 掃描已依待送檔案數節流（343aae7），但那是軟上限，見 SRC-04。
- 8659e16 對有界的 pool 是正確的：項目數絕不超過上限，所以輸入的 `sync_channel` 不會阻塞；collector 被丟掉時，等待中的 submitter 會被放開；計數不會變成負數。唯一的缺口是 worker panic 時，那個項目永遠不會離開，見 SRC-03。
- `ordered_unbounded` 實際上受限於約 77 段，因為 Destination 的主 thread 是它唯一的上游。而這些段正是 SRC-04 超出上限的主要原因。
