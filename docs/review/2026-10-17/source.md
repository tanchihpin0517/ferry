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
| SRC-09 | 沒有 one-file-system（`-x`），`fstatat` 也沒帶 `AT_NO_AUTOMOUNT` | 低 | 已核對程式碼 |
| SRC-11 | 大於 1 MiB 的檔案全由 file sender 一個 thread 讀取並雜湊 | 低 | 已核對程式碼 |
| SRC-12 | `getent` 與 `curl` 繼承了被封鎖的訊號 | 低 | 已核對程式碼 |
| SRC-17 | 收到訊號時，終端機還原可能與儀表板的繪製交錯 | 低 | 推測 |

---

## 較小的項目

### SRC-09：沒有 one-file-system（`-x`），`fstatat` 也沒帶 `AT_NO_AUTOMOUNT`
- 嚴重度：低。驗證狀態：已核對程式碼。
- 位置：`src/fsys.rs:195-201`（`stat`）、`src/scan.rs:215-223`（`is_directory`）、`src/scan.rs:142-167`（`list_tree` 一律進入子目錄）。
- 說明：掃描會跨越掛載點，例如複製 `/` 時會進入 `/proc`、`/sys`、`/dev`；以 root 執行時，`/proc/kcore` 的 `st_size` 是 128 TiB，會被當成大檔案串流。在 Linux 上，`fstatat` 不帶 `AT_NO_AUTOMOUNT` 會觸發 autofs 掛載點，掃描一個 autofs 的 `/home` 就會把每個使用者的 home 都掛起來。README 中「not built yet」那一列也沒有列出 `-x`。
- 修法：
  - stat 一律加上 `AT_NO_AUTOMOUNT`（Linux 才有；macOS 沒有對應的 flag，也沒有這個問題）。
  - 新增 `--one-file-system` / `-x`：記住根的 `st_dev`；子目錄的 `st_dev` 不同時，照常送出該目錄的 entry，但不列出其內容。可以沿用「無法列出的目錄」的處理方式，讓 Destination 不刪除其中任何東西，但不要回報為問題。
  - 文件要改 `docs/cli.md` 的「Options」與 `cli.zh-TW.md` 的「選項」、README 的 rsync 對照表。

> 2026-10-22 補記（WP-20，D10）：使用者選 A，照修法做，並以 rsync 3.2.7 實測對照：加 `-x` 時，掛載點以目錄送出（帶它自己的權限，也就是掛上去那個檔案系統根目錄的權限，與 rsync 相同），其中的內容不送；Destination 那邊因此收不到它的內容，裡面已有的東西保持原樣，不必另加協定訊息。`Hello` 與兩端的 `Options` 加 `one_file_system`；`scan` 的 `avoid` 改成 `Bounds { avoid, one_file_system }`。`fsys::stat` 在 Linux 上一律帶 `AT_NO_AUTOMOUNT`；Destination 用的也是同一個函式，所以兩端都不會因為 stat 觸發 autofs。與修法不同之處：(1) 判斷掛載點的方式，是在開啟子目錄之前，以這個 `stat` 比較 `st_dev`，所以 `-x` 時不會為了判斷而掛起 autofs；但比 128 層更深、從根重新開啟的目錄，改為開啟之後才以 `fstat` 比較，這種目錄若是 autofs 的掛載點，仍會被掛起。(2) 不送 `Problem`，也不算問題，直接不列出。(3) rsync 的 `-xx`（連掛載點本身也不送）沒有做。不加 `-x` 時照舊跨過掛載點，與 rsync 相同；README 的對照表加了 `-x` 一列。測試：`tests/owners.rs` 的 `one_file_system_copies_a_mount_point_but_not_what_it_holds`（root，掛 tmpfs；本機以 `unshare -rm` 跑過）。

### SRC-11：大於 1 MiB 的檔案全由 file sender 一個 thread 讀取並雜湊
- 嚴重度：低（效能）。驗證狀態：已核對程式碼，沒有量測。
- 位置：`src/source.rs:716-723`（`size > SMALL` 時回傳 `Ready::Large`）、`src/source.rs:670-679`（sender 自己呼叫 `write_record`）。
- 說明：一棵全是 2 到 10 MiB 檔案的目錄樹（例如照片），16 個 reader 只負責開啟檔案，讀取與 BLAKE3 雜湊全在 file sender 這一個 thread 上依序進行。在 NFS 上，每次 256 KiB 的讀取都要等一次延遲。這與 pipeline.md 中「雜湊因平行化而受益大」的說法不符。
- 修法方向，需先量測：讓 reader 也預先讀取大檔案的前幾個區塊；或者把「1 MiB」改為「最多 N 個、總共 M MiB 的預讀」，例如以 8 個 2 MiB 的區塊為上限，用 bounded 的 per-file channel 交給 sender，sender 只負責依序把區塊放進串流。

### SRC-12：`getent` 與 `curl` 繼承了被封鎖的訊號
- 嚴重度：低。驗證狀態：已核對程式碼。
- 位置：`src/names.rs:75-81`（`getent`，在 Source 的 manifest thread 上執行）、`src/remote/release.rs:100`（`curl`）。對照 `src/interrupt.rs:177-187`（`unblock_in_child`）。
- 說明：`interrupt::install` 在所有 thread 封鎖了 SIGINT、SIGTERM、SIGHUP、SIGQUIT，子 process 會繼承這個 signal mask。這也是 `unblock_in_child` 存在的原因，ssh 與本機 server 都經過它。`getent` 與 `curl` 沒有，所以它們不會被 Ctrl-C 結束。Ferry 本身會照常結束；`getent` 若卡在無法連線的 LDAP 或 SSSD，會繼續留著，直到寫 pipe 失敗為止。
- 修法：這兩處也套用 `interrupt::unblock_in_child`。

> 2026-10-20 補記：WP-11 只需改 `getent`：`curl` 在先前的修正中已經套用 `unblock_in_child`。沒有加測試：`getent` 在測試中不會卡住，訊號的遮罩只能靠核對程式碼。

### SRC-17：收到訊號時，終端機還原可能與儀表板的繪製交錯
- 嚴重度：低（外觀）。驗證狀態：推測，未實測。
- 位置：`src/interrupt.rs:93-97`（訊號 thread 呼叫 `before_exit`，也就是儀表板的 `restore`）、`src/dashboard.rs:161-165`（儀表板 thread 照常 `screen.draw`）。
- 說明：收到 SIGTERM 或 SIGHUP 時，訊號 thread 離開 alternate screen，同時儀表板 thread 可能正在畫一個 frame，或在 `exit` 之前又畫一次，主畫面上就會留下殘影。Ctrl-C 從儀表板按下時，是儀表板 thread 自己先 `restore`，所以不受影響。
- 修法：在 `restore` 之後設一個旗標，儀表板 thread 每次繪製前檢查它；或在 `before_exit` 中先取得儀表板繪製用的鎖。SRC-03 的 panic hook 也要用同一套機制。

> 2026-10-20 補記：WP-14 以修法的第二種做法處理：儀表板繪製時持有 `DRAWING` 鎖，而且只在 `ACTIVE` 時才繪製；`restore` 先取得同一把鎖，再離開 alternate screen。`restore` 也會在 panic 的 hook 中執行，那時繪製的 thread 可能正持有鎖，所以只嘗試 100 ms，取不到就照樣還原。沒有測試：這是時序的問題，只核對了程式碼。

## 給記憶筆記的更正

- `Known` 不再是永不修剪：4086576 之後，以 `Needs::Through` 修剪。
- Destination 的 decoded-data channel 已有上限（f3f6653，`DECODED = 8`），報告的 channel 也已有上限（099b1d1，16）。
- 掃描已依待送檔案數節流（343aae7），但那是軟上限，見 SRC-04。
- 8659e16 對有界的 pool 是正確的：項目數絕不超過上限，所以輸入的 `sync_channel` 不會阻塞；collector 被丟掉時，等待中的 submitter 會被放開；計數不會變成負數。唯一的缺口是 worker panic 時，那個項目永遠不會離開，見 SRC-03。
- `ordered_unbounded` 實際上受限於約 77 段，因為 Destination 的主 thread 是它唯一的上游。而這些段正是 SRC-04 超出上限的主要原因。
