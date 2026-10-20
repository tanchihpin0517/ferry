# Source 端審查：交接文件

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
| SRC-01 | 需要的檔案在掃描後被換成 FIFO，傳輸永遠卡住 | 高 | 已實測重現 |
| SRC-02 | 傳輸失敗後，Source 仍把整棵目錄樹掃完，且不再讀取輸入 | 中 | 已實測重現 |
| SRC-03 | 任何 panic 都變成無聲的永久停滯；file sender 的錯誤從不告知 Destination | 中 | pool 部分已實測，其餘已核對程式碼 |
| SRC-04 | 待送檔案的上限 100,000 被超出約 8 倍 | 中 | 已實測重現 |
| SRC-05 | 「60 秒沒有東西經過」的快照在真正停滯時從不觸發 | 低 | 已實測重現 |
| SRC-06 | 每個需要的檔案都重新以路徑解析根目錄並逐層開啟 | 低 | 已核對程式碼 |
| SRC-07 | 掃描開著的 fd 隨目錄樹深度增加，且無人考慮 `RLIMIT_NOFILE` | 低 | 已實測重現 |
| SRC-08 | 儀表板顯示時，Ctrl-\ 不會產生快照 | 低 | 已核對程式碼 |
| SRC-09 | 沒有 one-file-system（`-x`），`fstatat` 也沒帶 `AT_NO_AUTOMOUNT` | 低 | 已核對程式碼 |
| SRC-10 | 讀取來源檔案沒有用 `O_NOATIME` | 低 | 已核對程式碼 |
| SRC-11 | 大於 1 MiB 的檔案全由 file sender 一個 thread 讀取並雜湊 | 低 | 已核對程式碼 |
| SRC-12 | `getent` 與 `curl` 繼承了被封鎖的訊號 | 低 | 已核對程式碼 |
| SRC-13 | 讀取期間就地改寫、mtime 落在同一秒的檔案，下次執行不會重送 | 低 | 已核對程式碼 |
| SRC-14 | 掃描期間消失的項目被算成「無法讀取」，結束狀態為 1 | 低 | 已核對程式碼 |
| SRC-15 | `send_found` 的 `expected` 與 lister 的 `waiting` 各存一份待列出目錄的路徑 | 低 | 已核對程式碼 |
| SRC-16 | `Known::compared_through` 的 `id + 1` 可能溢位 | 低 | 已核對程式碼 |
| SRC-17 | 收到訊號時，終端機還原可能與儀表板的繪製交錯 | 低 | 推測 |

---

### SRC-01：需要的檔案在掃描後被換成 FIFO，傳輸永遠卡住

- 嚴重度：高
- 驗證狀態：已實測重現。來源目錄樹有 `a/` 下 300,000 個 1 位元組的檔案，以及 `b/target`。`ferry copy --no-ui` 開始 0.4 s 後執行 `rm b/target && mkfifo b/target`。`a/` 的檔案都送完之後，傳輸就不再前進，直到 `timeout` 殺掉它（結束狀態 124）。在第 75 s 以 SIGQUIT 取得的快照中有 `the Source on this machine: reader ×1: read files, for 29.58 s`；另一次在第 125 s 取得的快照則是 `for 71.43 s`。見 `repro-fifo.sh`。
- 位置：
  - `src/fsys.rs:314-329`：`FILE_READ_FLAGS` 與 `open_file`，沒有 `O_NONBLOCK`，開啟後也不檢查類型
  - `src/source.rs:707-734`：`prepare` 開啟後只取 `metadata.len()`，不檢查 `is_file()`
  - `src/source.rs:795-805`：`open_source_file`；根目錄本身就是單一檔案時，`:797` 用 `File::open(root)`，同樣會阻塞
  - `src/source.rs:633-644`：file sender 要等 `in_flight == 0` 才結束
- 現象：一次執行若在掃描與讀取之間（最長可達 backlog 的 100,000 個檔案、數分鐘甚至數小時），有個需要的一般檔案被換成 FIFO，傳輸就永遠卡住，不報錯，也不結束。cron 等無人看管的執行會一直掛著。被換成字元裝置時（需要 root），`open` 可能等待（例如沒有 carrier 的序列埠），也可能有副作用（磁帶機在 close 時倒帶）。
- 根本原因：
  1. 掃描時 stat 到的是一般檔案，manifest 裡是 `Kind::File`，所以 Destination 會需要它。
  2. reader 依路徑重新開啟它。`openat` 只帶 `O_RDONLY | O_NOFOLLOW | O_CLOEXEC`；對 FIFO 而言，沒有 `O_NONBLOCK` 的唯讀開啟會一直阻塞，直到有寫入端出現。
     ```rust
     // src/fsys.rs:314-329
     const FILE_READ_FLAGS: OFlags = OFlags::RDONLY
         .union(OFlags::NOFOLLOW)
         .union(OFlags::CLOEXEC);

     pub fn open_file(directory: impl AsFd, name: &[u8]) -> io::Result<File> {
         one_name(name)?;
         let fd = rustix::fs::openat(
             directory,
             OsStr::from_bytes(name),
             FILE_READ_FLAGS,
             Mode::empty(),
         )?;
         Ok(File::from(fd))
     }
     ```
  3. 那個 reader thread 永遠卡在 `prepare` 裡，它的項目永遠不會以 `Job::Ready` 回來，`in_flight` 永遠不會歸零：
     ```rust
     // src/source.rs:642-644
     if ending && in_flight == 0 && backlog.is_empty() {
         break;
     }
     ```
  4. 所以 record 串流永遠不會結束，Destination 永遠不會送出 `Control::Done`，Source 的主 thread 永遠在 `receive` 中等待。
  5. 就算 `open` 沒有阻塞，`prepare` 也不檢查類型：檔案被換成目錄時，`read` 會得到 `EISDIR`，結果以零補到目錄的 `st_size` 並標記為未讀完。這個結果正確，只是浪費頻寬。
- 重現方式：`FERRY=<ferry> sh docs/review/2026-10-17/materials/source/repro-fifo.sh 75`。腳本最後印出結束狀態 124，以及 `reader ×1: read files, for …`。
- 修法：
  1. 在 `src/fsys.rs` 新增 `open_regular_file(directory, name) -> io::Result<(File, Stat)>`，取代 Source 對 `open_file` 的使用（Destination 若也用 `open_file` 讀取既有檔案，同樣改用它）：
     - 開啟前先 `fstatat(..., AT_SYMLINK_NOFOLLOW)`；不是 `S_ISREG` 就不開，直接回傳錯誤。這樣大部分情況下連裝置都不會被開啟。
     - 再以 `O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK | O_NOCTTY` 開啟，之後 `fstat`。不是 `S_ISREG` 就關掉並回傳錯誤。這一步堵住 stat 與 open 之間的競態。
     - 最後以 `fcntl(F_SETFL)` 清掉 `O_NONBLOCK`。對一般檔案，Linux 與 macOS 都忽略 `O_NONBLOCK`，但在有強制鎖的檔案上它會讓 `read` 得到 `EAGAIN`，所以要清掉。
     ```rust
     pub fn open_regular_file(directory: impl AsFd, name: &[u8]) -> io::Result<(File, Stat)> {
         one_name(name)?;
         let not_regular = || io::Error::other("it is no longer a regular file");
         if file_type(&stat(&directory, name)?) != FileType::RegularFile {
             return Err(not_regular());
         }
         let flags = FILE_READ_FLAGS | OFlags::NONBLOCK | OFlags::NOCTTY;
         let fd = rustix::fs::openat(&directory, OsStr::from_bytes(name), flags, Mode::empty())?;
         let opened = rustix::fs::fstat(&fd)?;
         if file_type(&opened) != FileType::RegularFile {
             return Err(not_regular());
         }
         rustix::fs::fcntl_setfl(&fd, rustix::fs::fcntl_getfl(&fd)? - OFlags::NONBLOCK)?;
         Ok((File::from(fd), opened))
     }
     ```
  2. 根目錄就是單一檔案時（`open_source_file` 的 `path.is_empty()` 分支），改用同樣邏輯的 `open_regular_root(root)`：`openat(CWD, root, ...)`，不帶 `O_NOFOLLOW`，因為使用者所指名的根可以是符號連結（與 `stat_root` 一致）。
  3. `prepare` 改用回傳的 `Stat` 取得大小與 mtime（`stat.st_size`、`fsys::mtime(&stat)`），不再呼叫 `file.metadata()`。
  4. 不是一般檔案時走現有的 `Ready::Unreadable` 路徑：回報 `cannot read X: it is no longer a regular file`，並送出 `Manifest::Problem { file: Some(id) }`，讓 Destination 收尾該目錄時不必等它（fc53112 的行為）。
  5. 邊界情況：
     - 開啟前後的 stat 都是一般檔案、但 inode 不同（被另一個一般檔案取代）：照常讀取開啟到的那個，與現在的語意相同。
     - Linux 與 macOS：`O_NOCTTY` 在 macOS 上沒有作用，但無害；`O_NONBLOCK` 在兩邊都讓 FIFO 的唯讀開啟立刻回傳。
     - socket 會得到 `ENXIO`，現在就已經是 Unreadable。
- 要加的測試：
  - `src/source.rs` 的 `mod tests`：在 tempdir 中 `libc::mkfifo` 一個 `f`，在另一個 thread 呼叫 `prepare(root, 1, b"f".to_vec())`，以 `recv_timeout(5 s)` 等待結果，斷言得到 `Ready::Unreadable`。若逾時，先以 `O_WRONLY | O_NONBLOCK` 開一次那個 FIFO 放開卡住的 reader，再讓測試失敗。這個測試在修正前會失敗，而不是卡住整個測試程式。
  - 同一處再加一個測試：`f` 是目錄時，`prepare` 回傳 `Ready::Unreadable`，不再產生以零補滿的 record。
  - `src/fsys.rs` 的 `mod tests`：`open_regular_file` 對一般檔案、FIFO、目錄、符號連結（`ELOOP`）、socket 的結果。
- 驗收條件：
  - [ ] FIFO 的單元測試在 5 s 內通過，並在修正前的程式碼上失敗
  - [ ] `repro-fifo.sh` 的結束狀態為 1（不是 124），stderr 有 `cannot read b/target: it is no longer a regular file`，`a/` 的 300,000 個檔案都在
  - [ ] 改成目錄的檔案不再傳送以零補滿的內容
  - [ ] `cargo test` 在 Linux 與 macOS 上都通過
- 文件要同步的地方：
  - `docs/design/pipeline.md` 的「3. Read and hash」與 `pipeline.zh-TW.md` 的「3. 讀取與雜湊」：加上「只開啟仍是一般檔案的東西，開啟絕不會阻塞；已不是一般檔案的，回報為無法讀取」。
  - `docs/design/pipeline.md` 的「7. Results」與 `pipeline.zh-TW.md` 的「7. 結果」：「a file that could not be opened」的說明要包含這種情況。
- 相關項目：SRC-05（這次停滯正是 60 秒快照應該觸發、卻沒有觸發的情況）、SRC-10（同一個開啟呼叫）、SRC-06（同一個函式）。

---

### SRC-02：傳輸失敗後，Source 仍把整棵目錄樹掃完，且不再讀取輸入

- 嚴重度：中
- 驗證狀態：已實測重現。以 `mk.py` 建立 1,000 個目錄 × 1,000 個空檔案，複製到不可寫的父目錄底下（`ro/dst`，`ro` 為 `chmod 555`）。Destination 一收到 Root 就失敗（`create …/ro/dst: Permission denied`），但 Source 的 `--profile-file` 顯示：`scan` 的 `list directories` 有 2,001 次、`stat` 的 `stat entries` 有 1,001 次、`manifest` 的 `run` 有 1,004 次，等於整棵目錄樹掃完、manifest 全部送出後才結束。見 `repro-scan-after-failure.sh`。「Source 不再讀取輸入」這部分是 protocol 審查者發現的，我已核對程式碼，沒有實測。
- 位置：
  - `src/source.rs:306-313`：`receive` 回傳後，只 `backlog.close()` 並送出 `Job::Stop`，接著在 `scanner.join()` 等掃描自然結束
  - `src/source.rs:443-453`：`Backlog::wait_for_room` 在 `closed` 之後直接返回，掃描全速繼續
  - `src/source.rs:492-524`：`ManifestSink::manifest` 不檢查任何停止條件
  - `src/scan.rs:142-167`：`list_tree` 只在 `submit` 失敗時停止
  - `src/relay.rs:199-210`：另一側的 server 結束後，client 的 relay 仍持續讀取並丟棄 Source 送來的東西，所以 Source 的 pump 永遠不會阻塞，也不會失敗
  - `src/peer.rs:125-146`：`inward` thread 只有在寫入 peer 的 pipe 得到 `EPIPE` 時才改為 drain
- 現象：
  - Destination 一失敗（例如寫不進根目錄、manifest 檢查失敗），使用者要等 Source 把整棵目錄樹掃完才看到錯誤。一億個項目、放在 NFS 上的目錄樹，這可能是好幾個小時，而這段期間 Source 對共用的 NFS server 持續施加完整的掃描負載。
  - Source 自己的 `receive` 失敗時（例如 `the Destination needs unknown ID`，或 connection 讀取錯誤），Destination 還活著，就繼續比較並送出 needs。Source 不再讀取，這些 needs 在 Destination 的 mux 中以無界的 `messages` 佇列堆積，一直持續到 Source 掃完為止。
- 根本原因：
  1. `receive` 一回傳，主 thread 就去 join 掃描，但沒有任何東西叫掃描停下：
     ```rust
     // src/source.rs:306-313
     let mut summary = Summary::default();
     let stop = jobs.clone();
     let received = receive(reader, known, jobs, backlog, &mut summary);
     backlog.close();
     if let Some(stop) = stop {
         let _ = stop.send(Job::Stop);
     }
     let problems = scanner.join().expect("the scan panicked");
     ```
  2. `backlog.close()` 的設計是「傳輸結束或失敗後不再等待」（`source.rs:398-404` 的註解，以及 pipeline.md 的同一句），所以關閉之後掃描反而以全速進行：
     ```rust
     // src/source.rs:443-447
     fn wait_for_room(&self) {
         let state = self.state.lock().expect("not poisoned");
         if state.0 <= self.limit || state.1 {
             return;
         }
     ```
  3. 掃描唯一的停止途徑是 `mux.message_paced` 失敗，也就是 pump 死掉。但 relay 在另一側消失後仍讀取並丟棄（`relay.rs:203-210` 的 `to = None` 之後迴圈照跑），所以 pump 永遠寫得出去。
  4. 主 thread 在 join 期間不讀 `reader`。Source server 的 `inward` thread（`peer.rs:128-138`）把收到的 packet 寫進通往 peer 的 pipe；pipe 滿了（64 KiB）之後 `protocol::forward` 就阻塞，而不是得到 `EPIPE`，因為讀取端 `BufReader::new(peer_input)` 要到 `run` 回傳才會關閉。接著 striped transport 的接收緩衝滿了，client 中「relay from the Destination」thread 的 `sending.send(packet)` 跟著阻塞，不再讀取 Destination；Destination 的 pump 寫不出去，它的 `mux.message` 佇列（無界）便開始堆積。
  5. 正常流程中，`receive` 只在 Destination 處理完 `Manifest::End` 之後才回傳 `Ok`（dry run 是 `Needs::End`，否則是 `Control::Done`）。所以 `receive` 回傳時，掃描若還沒結束，就一定是失敗。這可以當成停止的條件。
- 重現方式：`FERRY=<ferry> sh docs/review/2026-10-17/materials/source/repro-scan-after-failure.sh`。修正前印出 `scan {… 'list directories': 2001 …}` 與 `stat {… 'stat entries': 1001 …}`。
- 修法：
  1. 新增一個停止旗標，與 `Backlog` 共用同一個 Mutex 與 Condvar，好讓等待中的掃描立刻醒來。建議把 `Backlog` 改名為 `Pacing`（SRC-04 也要擴充它），狀態改成具名欄位：
     ```rust
     struct PacingState {
         waiting: usize,   // 需要、尚未送出的檔案
         closed: bool,     // sender 已結束：不再等待
         halted: bool,     // 傳輸已結束或失敗：掃描應停止
         // SRC-04 會加上 through
     }
     /// The transfer stopped before the scan did: what the scan returns then.
     #[derive(Debug)]
     struct Halted;
     impl std::fmt::Display for Halted { /* "the transfer stopped" */ }
     impl std::error::Error for Halted {}
     ```
  2. 設定 `halted` 的地方：
     - `run` 中 `receive` 一回傳（無論 `Ok` 或 `Err`）就呼叫 `pacing.halt()`，再 `backlog.close()`、送出 `Job::Stop`。
     - file sender thread 失敗時也呼叫（見 SRC-03）。
  3. 檢查 `halted` 的地方：
     - `ManifestSink::manifest` 與 `ManifestSink::problem` 一開始就檢查，已停止時回傳 `Err(anyhow::Error::new(Halted))`。
     - `Pacing::wait_for_room`（SRC-04 改名為 `wait`）的等待條件加上 `!halted`，醒來後若 `halted` 就回傳 `Err(Halted)`。
     - `scan::scan_in_parts` 多一個參數 `stop: &AtomicBool`（或 `&dyn Fn() -> bool`），交給 `list_tree`，在每次 `waiting.pop()` 與 `fsys::open_directory` 之前檢查，已停止就 `return`。`Pacing` 內同時維護一個 `AtomicBool`，讓 lister thread 不必取 Mutex。`scan::scan` 與測試傳一個永遠為 false 的旗標。
     - `write_record` 的讀取迴圈每讀一塊（`READ_CHUNK`）檢查一次，讓正在串流的大檔案也能停下；已停止時回傳 `Err(Halted)`。
  4. `scanner` thread 在掃描回傳 `Halted` 時，不送出 `Control::Error`（`source.rs:248-253`）。這不是掃描自己的失敗，送出去只會蓋掉真正的原因。
  5. `run` 的錯誤優先順序（`source.rs:319-322`）：目前是先 `problems?`，掃描的錯誤優先。改為依「掃描、sender、`receive`、pump」的順序，取第一個不是後果的錯誤；後果指 `Halted`，以及 mux 的 `closed()`（`the connection's writer stopped`）。全部都是後果時，才回傳第一個錯誤。可以為 mux 的 closed 錯誤也定義一個型別，以便用 `error.is::<…>()` 判斷。
  6. 有了以上幾步，`run` 在 `receive` 失敗後很快就回傳，`peer.rs` 隨即丟掉 `reader`，`inward` 得到 `EPIPE` 並開始 drain，relay 也就不再阻塞，不需要另外在主 thread drain。若要更保險，也可以在 `halt()` 之後另開一個 thread，以 `io::copy(reader, io::sink())` 讀到結束；但 `reader` 是 `&mut impl Read`，跨 thread 使用需要 scoped thread，而且要在 join 完其他 thread 之後才能放手，所以不建議。
  7. 不需要分 Linux 與 macOS。
- 要加的測試：
  - `src/source.rs` 的 `mod tests`：在 tempdir 建 2,000 個空目錄。`reader` 是只含一個 `protocol::encode(&Control::Error { message: "x".into() })` 的 `&[u8]`；`writer` 是每次 `write` 都 sleep 1 ms 的 `Vec<u8>` 包裝（`Write + Send`）；`Options` 設 `paced_bytes: 4096`、`dry_run: true`。斷言 `run` 在 2 s 內回傳 `Err`，訊息含 `the Destination failed: x`；從 writer 收到的 bytes 以 `protocol::receive` 解出的 `Manifest::Directory` packet 少於 200 個。修正前，掃描會跑完全部 2,000 個目錄。
  - `src/scan.rs` 的 `mod tests`：以一個停止旗標已設的 `scan_in_parts` 掃描 `big_tree`，斷言 sink 收到的 `Directory` 訊息少於全部。
  - 可選的 `tests/copy.rs` 整合測試：Destination 的父目錄 `chmod 555`（以 root 執行時略過，比照 `an_unreadable_directory_is_left_out_and_the_scan_goes_on`），來源 3,000 個目錄，帶 `--paced-bytes=4096 --profile-file`，斷言失敗訊息，以及 Source 的 `stat entries` 次數遠少於 3,000。這個測試對時間敏感，以前一個單元測試為主。
- 驗收條件：
  - [ ] `repro-scan-after-failure.sh` 中 Source 的 `stat entries` 只有個位數到數十，而不是 1,001
  - [ ] 失敗訊息仍是 Destination 的原因（`create …: Permission denied`），沒有被 `the transfer stopped` 蓋掉
  - [ ] 正常的 `copy`、`sync`、`--dry-run` 結果不變（`cargo test` 全部通過）
  - [ ] Source 自己的 `receive` 失敗時（可在測試中讓 `reader` 送一個未知 ID 的 `Needs::Files`），`run` 很快回傳，不會等到掃描結束
- 文件要同步的地方：
  - `docs/design/pipeline.md` 的「1. Scan」與 `pipeline.zh-TW.md` 的「1. 掃描」：
    - 「The scan waits while too many needed files wait to be sent」最後一句「a transfer that ends, or fails, has it wait no more」改為：傳輸結束時不再等待，傳輸失敗時掃描立刻停止。中文的對應句是「傳輸結束或失敗時，它就不再等待」。
    - 「A scan that fails stops the transfer at once」補上反方向：傳輸失敗也會立刻停止掃描。中文的對應句是「掃描失敗時，傳輸會立刻停止」。
- 相關項目：SRC-03（共用停止旗標與錯誤優先順序）、SRC-04（共用 `Pacing`）。

---

### SRC-03：任何 panic 都變成無聲的永久停滯；file sender 的錯誤從不告知 Destination

- 嚴重度：中（需要先有 bug 或 panic 才會觸發，但觸發後就是無聲且永久的停滯）
- 驗證狀態：pool 部分已實測重現：`poolpanic` crate 以 `pool::ordered(scope, 8, 64, …)` 送入 1,000 項，worker 在第 3 項 panic；印出 panic 訊息後整個 pool 卡住，5 s 的 watchdog 觸發。Source 各 thread 的 panic 與 file sender 的錯誤路徑已核對程式碼，沒有實測。
- 位置：
  - `src/pool.rs:247-262`：worker 在 `work(item)` 中 panic 時，那個序號的結果永遠不會送出
  - `src/pool.rs:124-134`：`Collector::next` 等待 `self.next`；`:131` 在全部 worker 結束時回傳 `None`，會把 `waiting` 裡較後面的結果靜靜丟掉
  - `src/pool.rs:48-65`：`InPool::enter` 在項目數達上限時等待，而 panic 的項目永遠不會離開
  - `src/source.rs:289-301`：file sender thread 失敗時只 `backlog.close()`，然後回傳錯誤
  - `src/source.rs:338-344`：註解說「The sender stops only on an error, which it reports itself」，實際上沒有回報
  - `src/source.rs:313-317`：scan 與 sender 的 panic，要等 `receive` 回傳後才在 `join().expect(…)` 被看到
  - `Cargo.toml`：沒有 `[profile.release]`，panic 策略是預設的 unwind
  - `src/dashboard.rs:105-109`：唯一的 panic hook，只還原終端機
- 現象：程式中只要有一處 panic（stat、壓縮、解壓縮的 worker、scan、file sender、reader、Destination 的 parser 或 writer……），server 的 stderr 會出現一則 panic 訊息（遠端時使用者通常看不到），之後傳輸永遠停住，結束狀態也不會出現。file sender 在 connection 仍正常時失敗，結果相同：例如 `frame::encode` 回傳錯誤（zstd 失敗），frame sender 結束，file sender 得到 `the workers stopped`。
- 根本原因：
  1. pool 的 worker 直接執行工作，panic 時 thread 結束，該序號的結果從不送出：
     ```rust
     // src/pool.rs:249-261
     loop {
         let item =
             profile::wait(State::Work, || input.lock().expect("not poisoned").recv());
         let Ok((sequence, item)) = item else {
             return;
         };
         if let Some(gauge) = &taken {
             gauge.down();
         }
         let result = profile::wait(profiled.work, || work(item));
         if output.send((sequence, result)).is_err() {
             return;
         }
     }
     ```
  2. collector 依序取結果，缺了那個序號就停在 `results.recv()`。其他 worker 還活著（它們持有 `output`），所以 recv 永遠不會因斷線而結束。8659e16 之後，未取走的項目要計數到被取走為止；那個項目永遠不會被取走，submitter 便在 `enter` 等待容量，worker 則在 `input.recv()` 等工作，形成三方互等。`std::thread::scope` 要等所有 thread 結束才會把 panic 傳出去，所以 panic 永遠不會傳出。
  3. pool 之外的 thread 也一樣。Source 的 `run` 先在 `receive` 中等待 Destination，等它結束了才 join 其他 thread：
     ```rust
     // src/source.rs:308-314
     let received = receive(reader, known, jobs, backlog, &mut summary);
     backlog.close();
     if let Some(stop) = stop {
         let _ = stop.send(Job::Stop);
     }
     let problems = scanner.join().expect("the scan panicked");
     let sent = sender.map(|sender| sender.join().expect("the file sender panicked"));
     ```
     scan thread panic 時，Destination 永遠等不到 manifest 的其餘部分；file sender 或 reader panic 時，Destination 永遠等不到 record 串流的結尾。兩種情況下 Destination 都不會送出 Done，`receive` 永遠不回傳。
  4. file sender 的錯誤也是同樣的結果，因為它從不告訴 Destination：
     ```rust
     // src/source.rs:289-301
     let sender = scope.spawn(move || -> Result<u64> {
         let _thread = profile::thread("file sender");
         let problems =
             send_files(root, backlog, &mux, frames, readers_jobs, queue, &reporter);
         // Nothing more is sent: the scan waits for it no more.
         backlog.close();
         let sent = sending.join().expect("the frame sender panicked");
         let problems = problems?;
         sent?;
         Ok(problems)
     });
     ```
     另外，frame sender 的錯誤才是原因，`problems`（`the workers stopped`）只是後果，但上面先回傳後者。
- 重現方式：
  - pool：在 repo 根目錄執行 `CARGO_TARGET_DIR=$PWD/.scratch/review-source/target cargo run --release -q --manifest-path docs/review/2026-10-17/materials/source/poolpanic/Cargo.toml`，輸出 `HANG: the pool did not finish within 5 s after a worker panicked`。
  - Source 的 thread：沒有現成的注入點。可暫時在 `scan::stat_part` 開頭加 `panic!()`，再執行任何 `ferry copy`，傳輸就會卡住。
- 修法：先決定 panic 政策，再補上 file sender 的錯誤回報。
  - 選項比較：

    | | A：pool worker 內 `catch_unwind` | B：global panic hook 做緊急清理後結束 process，並在 `[profile.release]`、`[profile.dev]` 設 `panic = "abort"` | A＋B |
    |---|---|---|---|
    | 涵蓋範圍 | 只有 pool 的 worker（stat、compress、decompress、compare）。scan、file sender、reader、frame sender、pump、Destination 的 parser、writer、finisher、relay 的 thread 都不涵蓋，而它們的 panic 同樣會停滯 | 每一個 thread，包括 client | 同 B |
    | 改動 | 要改 `pool.rs` 的介面（結果變成 `Result<U, Panicked>`，或讓 pool 自行毒化並使 `Collector` 回報錯誤），每個呼叫端都要處理；`work` 要包 `AssertUnwindSafe` | `Cargo.toml` 兩行、`cli::run` 安裝一個 hook、`interrupt.rs` 一個不阻塞的清理函式 | 兩者都要 |
    | panic 後的狀態 | 錯誤要正確傳到 Source、Destination 與 client，路徑多、容易漏；Mutex 會被毒化，其餘程式到處都是 `expect("not poisoned")` | process 立刻結束；client 看到 server 的 connection 結束，報告失敗；暫存檔由 hook 移除 | 在 abort 之下 `catch_unwind` 什麼也接不到，只在測試的 unwind build 中有作用 |
    | 測試 | `cargo test` 永遠是 unwind，能以單元測試驗證 | 單元測試不受影響（hook 只在 `cli::run` 中安裝，測試 harness 本來就強制 unwind）；以整合測試驗證 binary 的行為 | — |

  - 建議選 B，不加 A。理由：會停滯的不只是 pool，而是每一個「主 thread 先在讀取中阻塞、之後才 join」的結構，逐一加 `catch_unwind` 既侵入又必然有遺漏。由 hook 結束 process，則對每個 thread 都成立。在 abort 之下，A 毫無作用。
  - 實作 B：
    1. `Cargo.toml`：
       ```toml
       [profile.release]
       panic = "abort"

       [profile.dev]
       panic = "abort"
       ```
       `[profile.dev]` 也要設，因為從原始碼安裝到 host 的 dev build 走的是 debug profile（`remote/install.rs` 的腳本執行 `cargo build`）。Cargo 對測試 harness 會忽略這個設定，所以 `cargo test` 不受影響。整合測試用的 binary 是否也是 abort，取決於 Cargo，所以不要依賴它：下一步的 hook 自己結束 process，行為才與 panic 策略無關。
    2. `cli::run` 在 `interrupt::install()` 之後立刻安裝 hook。順序是：還原終端機、印出訊息、server 盡量告知 client、移除暫存檔、`exit(101)`。
       ```rust
       std::panic::set_hook(Box::new(|info| {
           interrupt::before_exit_now(); // 不阻塞地還原終端機（儀表板、sudo 的 echo）
           let role = profile::current_role().unwrap_or("main");
           let _ = writeln!(io::stderr(), "ferry: internal error in {role}: {info}");
           peer::try_tell_failure(&format!("internal error in {role}: {info}")); // 僅 server；try_lock CONTROL
           interrupt::exit_now(101); // try 版本的 gate 與 TEMPORARY，移除暫存檔後 exit
       }));
       ```
       - hook 中的每一把鎖都必須用 `try_lock` 或 `try_write`，並加上短暫重試（例如最多 100 ms）。panic 可能就發生在持有 `GATE.read()` 時（`interrupt::create` 的 closure 中），這時呼叫阻塞的 `GATE.write()` 會等自己而死鎖；`TEMPORARY`、`BEFORE_EXIT`、`peer::CONTROL` 也一樣。所以在 `interrupt.rs` 新增 `exit_now(code)`：`GATE.try_write()`（重試失敗就跳過 gate）、`TEMPORARY.try_lock()`、unlink、以 try 版本執行 `before_exit`，再 `std::process::exit(code)`。
       - 以 `exit(101)` 結束，而不是讓 abort 發生（abort 的結束狀態是 134，看起來像「被訊號 6 中斷」）。101 是 Rust 慣用的 panic 結束狀態。只想保留 core dump 時可改用 `abort`，但要在文件中寫明。
       - `profile::current_role()` 是新的小函式，從 `SLOT` 讀出目前 thread 的 role，讓訊息能指出是哪個 stage；profiling 關閉時回傳 `None`。
       - client 這一側：本機的 server 是子 process，它們的 stdin 關閉後會呼叫 `interrupt::stop(1)` 自行清理；遠端的 server 則在 ssh 結束後同樣如此。
    3. 儀表板已有的 hook（`dashboard.rs:105-109`）先還原終端機，再呼叫先前的 hook。它是後安裝的，所以會先執行。保留也無害（`restore` 以 `ACTIVE.swap` 保證只做一次）；若想簡化，也可以刪掉它，改由 global hook 經 `before_exit` 還原。不論哪一種，都要確保訊息印在已還原的主畫面上。
    4. server 如何告知 client：`peer.rs` 的 `tell` 走 control connection。可以新增 `Report::Said` 或專用的 `Report::Failed`，或直接沿用 `Control::Error`。後者要確認 `relay.rs` 的 `listen` 會處理 control connection 上的 `Control::Error`；目前那裡只處理 metrics、snapshot 一類的 report。做不到時，至少保證 server 的 stderr 會出現在 client 的 stderr 上。
  - file sender 的錯誤，不論採哪個 panic 政策都要修：
    1. sender thread 中，`send_files` 或 frame sender 任一失敗時：`let _ = mux.message(&Control::Error { message: format!("{error:#}") });`，再呼叫 SRC-02 的 `pacing.halt()`。
    2. Destination 收到後 bail（`the Source failed: …`），它的 server 結束，relay 關閉 Source 的輸入，Source 的 `receive` 隨即回傳。relay 記下的第一個失敗就是 Source 自己的 `Control::Error`（`relay.rs:197-201` 記錄它，`:156-158` 的 `fail` 用 `get_or_insert`，只留第一個），所以使用者看到的是真正的原因。
    3. sender thread 內，frame sender 的錯誤（`sent`）優先於 `problems`；`run` 的優先順序見 SRC-02 修法第 5 點。
    4. 修正 `source.rs:338` 的註解。
  - 若仍想讓 pool 在測試中不停滯（可選）：`Collector::next` 在 `recv()` 斷線、`waiting` 卻非空時（`pool.rs:131`），代表有 worker 死了，應該 panic 或回報錯誤，而不是回傳 `None`；這可在 unwind 的測試 build 中當作保險。
- 要加的測試：
  - 注入點：新增僅在 debug build 有效的環境變數 `FERRY_TEST_PANIC=<role>` 與 `FERRY_TEST_FAIL=<point>`，以 `cfg!(debug_assertions)` 與一個 `OnceLock<Option<String>>` 實作，成本可以忽略。至少放在 `scan::stat_part`（`stat`）、`source::prepare`（`reader`）、frame sender（`frame-encode` 時讓 `frame::encode` 回傳 `Err`）。本機 server 經 `sh -c` 啟動，會繼承 client 的環境變數。
  - `tests/copy.rs` 整合測試，以 `spawn` 加 `try_wait` 輪詢，設 30 s 上限：
    - `FERRY_TEST_PANIC=stat`：結束狀態為 101，stderr 含 `internal error in stat`，目的地沒有 `.*.ferry-*` 暫存檔。
    - `FERRY_TEST_PANIC=reader`：同上。
    - `FERRY_TEST_FAIL=frame-encode`：結束狀態為 1，stderr 含注入的錯誤，而且不是 `the workers stopped`。
  - `poolpanic` 只有在選 A，或加了上面的 `Collector` 保險時，才值得改寫成 `src/pool.rs` 的單元測試（預期結果改為「collector 回報錯誤」，而不是逾時）。
- 驗收條件：
  - [ ] 三個注入情境都在 30 s 內結束，結束狀態與訊息如上
  - [ ] 本機與遠端的 server 在 client 因 panic 結束後都會退出，不留下暫存檔
  - [ ] 在儀表板開著時 panic，終端機被還原，訊息看得見
  - [ ] `cargo test`（unwind）與 release build（abort）都通過
- 文件要同步的地方：
  - `docs/cli.md` 的「Exit status」與 `cli.zh-TW.md` 的「結束狀態」：新增一列，101 表示 Ferry 內部錯誤（bug），訊息以 `ferry: internal error` 開頭。
  - `docs/design/architecture.md` 的「Client and servers」與 `architecture.zh-TW.md` 的「Client 與 server」：補一句 server 內部錯誤時如何結束、如何告知 client。
  - `src/source.rs:338` 的註解。
- 相關項目：SRC-02（停止旗標與錯誤優先順序）、SRC-17（hook 中的終端機還原）。

---

### SRC-04：待送檔案的上限 100,000 被超出約 8 倍

- 嚴重度：中（記憶體仍有上限，但比設計說的大一個數量級；兩側的簿記，即 Source 的 `jobs` 與 `backlog`、Destination 的 `targets`，都跟著放大）
- 驗證狀態：已實測重現。以 `mk.py` 建立一百萬個空檔案，100 個目錄 × 10,000，執行 `ferry copy --no-ui --no-fsync --profile-file`。Source 的 `needed files` gauge 峰值是 **825,925，上限是 100,000**。以 `repro-backlog.sh` 在同一台機器上再跑一次，峰值是 438,175：數值隨時機變動，但都是上限的 4 到 8 倍。記憶筆記中量到的 24 萬到 45 萬，是在較小的目錄上；目錄越接近 10,000 個項目（`scan::PART`），可能超出越多。
- 位置：
  - `src/source.rs:493-496`：只在送出每個 `Manifest::Directory` 之前檢查 backlog
  - `src/source.rs:395-454`：`BACKLOG` 與 `Backlog`
  - `src/mux.rs:45-47`：`PACED_BYTES` 為 8 MiB，以 30 到 40 B 一個項目計算，約 20 萬到 28 萬個項目
  - `src/destination.rs:441-444`：比較的 pool 是 `ordered_unbounded(…, 64, …)`；`:640` 是 `PARTS = 4`
  - `src/destination.rs:505-521`：比較完的部分，只在每次收到 packet 之前以 `try_next` 取走
  - `src/destination.rs:118`、`:761-768`：`THROUGH_EVERY = 4096`；`Needs::Through` 只在目錄的最後一段套用後送出
  - `src/source.rs:372`：Source 收到 `Needs::Through` 時，只用來修剪 `Known`
- 現象：設計說待送的檔案超過 100,000 個時掃描就等待，但實際峰值可以到 80 萬以上。
- 根本原因：
  1. 掃描在送出每一段之前檢查「需要、尚未送出」的檔案數：
     ```rust
     // src/source.rs:493-496
     fn manifest(&mut self, message: Manifest) -> Result<()> {
         if matches!(message, Manifest::Directory { .. }) {
             self.backlog.wait_for_room();
         }
     ```
  2. 但 needs 要等那些段被 Destination 比較並套用之後才回來。在檢查與 needs 回來之間，還可能有：
     - Source 的 mux 裡最多 8 MiB 已排隊、還沒寫出的 manifest；
     - 兩側 transport 與 relay 的緩衝；
     - Destination 比較管線中的段：`PARTS`（4）＋ pool 輸入佇列（64）＋ 正在比較的（8），每段最多 10,000 個項目，約 76 萬個。
  3. 這些全部變成 needs 之後，就疊在已經接近 100,000 的 backlog 上，所以上限是軟的：100,000 加上整條途中的 manifest。
  4. 另外，Destination 套用比較結果、送出 needs 與 `Through` 的時機，只在主 thread 每次讀到 packet 之前：
     ```rust
     // src/destination.rs:505-521（節錄）
     loop {
         while let Some(done) = compared.try_next() {
             profile::wait(profile::State::Apply, || {
                 take_compared(done, &mut waiting, &mut through, mux, root, state, options, reporter)
             })?;
         }
         let Some(packet) =
             profile::wait(profile::State::ReadConnection, || protocol::receive(reader))?
         else {
             return Ok(());
         };
     ```
     比較完成時，若主 thread 已經在 `receive` 中阻塞，就要等下一個 packet 到了才套用。這一點今天只造成延遲，但會讓下面的修法死鎖，所以必須一併修改。
- 重現方式：`FERRY=<ferry> sh docs/review/2026-10-17/materials/source/repro-backlog.sh`（預設 100 × 10,000），印出例如 `needed files: peak 438175 bound 100000`。每次的數字不同，兩次實測分別是 825,925 與 438,175。
- 修法：以「途中的 ID」節流，也就是已送出、而 Destination 尚未以 `Needs::Through` 確認比較過的項目數。這個修法分兩部分，第二部分是 Destination 的改動，必須與 Destination 審查者協調。
  - 第一部分：Source 端（`src/source.rs`）。
    1. 要計數什麼：
       - `sent`：已交給 mux 的最後一個項目的 ID，由 `ManifestSink` 維護。送出 Root 後為 0；送出每個 `Directory` 之後，為該段最後一個 entry 的 `id`。
       - `through`：收到的最大 `Needs::Through(id)`，初值為 0。
       - 途中的 ID 就是 `sent - through`。`Known` 保留的也正是這些項目，所以它的大小同時有了上限。
    2. 在哪裡等待：只在一個目錄的第一段之前等待。`ManifestSink` 記住上一個 `Directory` 訊息的 `more`；`more == true` 表示下一段是同一個目錄的延續，這時只檢查 backlog（它會自行消化），不檢查途中的 ID。原因是 Destination 要到目錄的最後一段才套用、才可能送出 `Through`；在目錄中途等 `Through`，大於上限的目錄就會死鎖。
    3. 等待條件，在擴充後的 `Pacing`（SRC-02）中：
       ```rust
       struct PacingState { waiting: usize, through: FileId, closed: bool, halted: bool }

       /// Waits before the next part: while too many needed files wait to be sent, or,
       /// before a directory's first part, while too many entries wait to be compared.
       fn wait(&self, starts_directory: bool, sent: FileId) -> Result<()> {
           let busy = |s: &PacingState| {
               s.waiting > self.backlog_limit
                   || (starts_directory && sent.saturating_sub(s.through) > self.in_flight_limit)
           };
           let state = self.state.lock().expect("not poisoned");
           let state = profile::wait(State::InFlight, || {
               self.changed.wait_while(state, |s| busy(s) && !s.closed && !s.halted)
           }).expect("not poisoned");
           if state.halted { return Err(Halted.into()); }
           Ok(())
       }
       fn through(&self, id: FileId) { /* through = max(through, id); notify_all */ }
       ```
       profile 的狀態可以依實際等待原因選擇：backlog 超過時是 `State::Backlog`，否則是新的 `State::InFlight`。
    4. `receive` 處理 `Needs::Through(id)` 時，除了 `known.compared_through(id)`，也呼叫 `pacing.through(id)`。
    5. 上限：
       - 新增 `IN_FLIGHT_ENTRIES`，預設 262,144，即 64 × `THROUGH_EVERY`。它不比今天 8 MiB 的 paced manifest 更緊，但把 Destination 比較佇列也算進上限。
       - 新增隱藏選項 `--in-flight-entries N`，比照 `--backlog-files`：`cli.rs` 的 `Cli`、`protocol::Hello` 的新欄位 `in_flight_entries: u32`、`session.rs:131`、`peer.rs:276-286`、`source::Options`，以及 `tests/server.rs:64` 中直接建構 `Hello` 的地方都要更新。
    6. 新增 gauge：`profile::Gauge::new("entries in flight", in_flight_limit)`，在 `wait` 與 `through` 時以 `set(sent - through)` 更新，讓 metrics 與 `--profile-file` 看得到。
    7. 新增 profile 狀態 `State::InFlight`，名稱 `wait for the Destination to compare`：加到 `profile.rs` 的 `State`、`State::ALL`、`name()`、`is_wait()`。狀態以編號傳送，client 與 server 是同一個 build，所以沒有相容問題。
  - 與 `Needs::Through`、`THROUGH_EVERY` 的互動，以及為什麼不會死鎖：
    - Destination 只在套用一個目錄的最後一段之後、且 `last >= through + THROUGH_EVERY` 時，才送出 `Through(last)`（`destination.rs:761-768`）。所以 Source 看到的 `through`，最多落後已完整送出部分的最後一個 ID `THROUGH_EVERY - 1`，再加上 Destination 還在比較的目錄。
    - 活性：Source 只在目錄邊界、且 `sent - through > L` 時等待。這時 `sent` 之前的每一段都已送出，Destination 終究會套用到 `sent` 為止。設那時它內部的 `through'`：若 `sent - through' >= THROUGH_EVERY`，它就送出 `Through(sent)`；否則 `through'` 早已送出、正在途中，到達後 `sent - through < THROUGH_EVERY`。只要 `L >= THROUGH_EVERY`，兩種情況下 Source 都會醒來。
    - 為了讓測試能用很小的上限，改成雙方都從 `Hello.in_flight_entries` 推得 `through_every = (in_flight / 2).clamp(1, 4096)`：Destination 用它取代常數 `THROUGH_EVERY`，Source 在 `wait` 中也用同一個值，並把 `L` 固定為 `max(in_flight, 2 * through_every)`。把 `THROUGH_EVERY` 移到 `protocol.rs` 成為 `pub const`，作為上限，兩側共用。
    - dry run 中 Destination 同樣會送 `Through`（`take_compared` 不分 dry run），所以 dry run 也受這個上限節流，Destination 的比較記憶體因此也有了上限。
    - 被 Source 略過的目錄（`send_found` 中 `sending == false`）不配 ID，也不計入 `sent`。Destination 不進入、整個需要的目錄，仍會經過比較並套用，所以仍會推進 `through`。
  - 第二部分：Destination 端，屬於 Destination 審查者的範圍，但這個修法依賴它。
    - 必要條件：Destination 必須在比較一完成時就套用，並送出 needs 與 `Through`，不能等下一個 packet。否則在 dry run，或沒有檔案要傳的 `sync` 中，Source 停下等 `Through`，Destination 的主 thread 卻在 `protocol::receive` 等 packet，兩邊互等。
    - 建議做法：把 `take_compared` 移到一個新的 scoped thread（例如 role 為 `apply`），以阻塞的 `compared.next()` 迭代，擁有 `waiting` 與 `through`。主 thread 只讀 connection、分派 packet；處理 `Manifest::End` 時，先 `drop(parts_in)`，再 join apply thread，取得它的 `Result`，之後才 `leave_all()` 並送出 `Needs::End`。這樣「collector 同時也是 submitter 的上游」的循環就不存在了，比較的 pool 可以從 `ordered_unbounded` 改成有界的 `pool::ordered`，順帶解決記憶筆記中「比較 pool 無界」的那一項。
    - apply thread 失敗時：回傳錯誤；`compared` 被丟掉，比較 thread 的 `submit` 失敗，`parts_out` 關閉，主 thread 下一次 `parts_in.send` 失敗時，再 join apply thread 取得真正的錯誤。若要更快結束，apply thread 可以先 `mux.message(&Control::Error { … })`。
    - 不建議的替代做法：Source 在等待時定期送出空的 packet，叫醒 Destination。這是輪詢，而且仍有延遲。
  - 其他做法的取捨：
    - 只把 Destination 的比較佇列改成以項目數計（例如最多 4 萬個項目），不改協定：仍然沒有算到 Source mux 裡 8 MiB 的 manifest，上限依舊是軟的，大約 10 萬加 30 萬。
    - 降低 `PACED_BYTES`：會降低高延遲連線上 manifest 的吞吐量。
    - 建議用上面的「途中的 ID」，它把上限變成精確的：需要、尚未送出的檔案數 ≤ `BACKLOG` ＋ `IN_FLIGHT_ENTRIES` ＋ 最大目錄的項目數。
- 要加的測試：
  - `src/source.rs` 的 `mod tests`：`Pacing::wait` 的單元測試，比照 `the_scan_waits_while_too_many_files_wait_to_be_sent`：
    - 延續段（`starts_directory == false`）不因途中的 ID 而等待；
    - `through()` 會叫醒等待者；
    - `halt()` 會讓 `wait` 回傳 `Err(Halted)`。
  - `tests/sync.rs`，比照 `:790-830` 那個用 profile JSON 斷言「the scan did wait」的測試：以 `--in-flight-entries=200 --backlog-files=3 --part-entries=50 --profile-file`，在 40 個目錄 × 200 個檔案上執行 copy、sync，以及一次 `--dry-run`，斷言結果正確，`wait for the Destination to compare` 出現在 profile 中，而且 `queues` 裡 `needed files` 的峰值 ≤ 3 ＋ 200 ＋ 200（最大目錄）＋ 少量餘裕。
  - Destination apply thread 的測試由 Destination 審查者負責：一次 dry run，Source 每送一段就等 `Through`（以極小的 `--in-flight-entries`），必須在時限內完成。
- 驗收條件：
  - [ ] `repro-backlog.sh` 的峰值 ≤ 100,000 ＋ 262,144 ＋ 10,000
  - [ ] 1,010 萬個項目的 dry run（profiling.md 的基準，目前約 2.8 s）與 10 萬個 1 KiB 檔案的 copy，時間沒有可量測的退步；經 SSH 的 `ferry test --scale 1G --kind small` 也沒有退步
  - [ ] 以 `--in-flight-entries` 設最小值時，copy、sync、dry run 都不會停滯
  - [ ] `Known` 的大小在 profile 或測試中可以看出有上限
- 文件要同步的地方：
  - `docs/design/pipeline.md` 的「1. Scan」與 `pipeline.zh-TW.md` 的「1. 掃描」：
    - 「The scan waits while too many needed files wait to be sent」：補上以途中的 ID 等待，並寫出精確的上限。中文的對應句是「待送出的檔案太多時，掃描會等待」。
    - 「Each entry gets an ID in manifest order」：`Through` 也用來節流掃描。中文的對應句是「每個項目依 manifest 順序取得一個 ID」。
  - `docs/design/pipeline.md` 的「2. Compare」與 `pipeline.zh-TW.md` 的「2. 比較」：比較完就套用，不等下一個 packet；比較 pool 改為有界。
  - `docs/design/profiling.md` 的「Where a thread changes state」與 `profiling.zh-TW.md` 的「Thread 在哪裡切換狀態」：Source 那一列加上新的等待狀態。
  - `docs/design/profiling.md` 的「Following a transfer as it goes」與 `profiling.zh-TW.md` 的「邊傳輸邊追蹤」：佇列清單加上 `entries in flight`。
- 相關項目：SRC-02（`Pacing` 與停止旗標）、Destination 審查中關於比較 pool 與 `targets` 記憶體的項目、記憶筆記的計畫第 2 點（以待送檔案數節流掃描，已在 343aae7 完成，但正是這一項所說的軟上限）。

---

### SRC-05：「60 秒沒有東西經過」的快照在真正停滯時從不觸發

- 嚴重度：低（不影響資料，但讓 profiling.md「傳輸卡住時每條 thread 在哪裡」這個承諾在最需要時失效）
- 驗證狀態：已實測重現。在 SRC-01 的停滯中，reader 卡了 71 s，stderr 裡始終沒有 `nothing has passed between the servers for 60 s`；只有手動送 SIGQUIT 才得到快照。
- 位置：
  - `src/relay.rs:161-165`：每收到一個 packet（包括 `Report`）就把 `passed` 加一
  - `src/relay.rs:176-197`：`Report` 被 client 自己消化，不轉送，但已經算成經過
  - `src/report.rs:28`、`:269-283`、`:386-403`：`watch` 開啟的 `report_queues` thread，每 250 ms 送一次 `Report::Queues`
  - `src/relay.rs:36`、`:301-317`：`STILL = 60 s` 的判斷
- 現象：傳輸停住時，client 不會自動說明每條 thread 在哪裡。
- 根本原因：
  1. relay 把收到的每個 packet 都當成「經過」：
     ```rust
     // src/relay.rs:161-165
     let packet = match from.recv() {
         Ok(Some(packet)) => {
             // Only this thread adds: no read-modify-write.
             passed.store(passed.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
             packet
         }
     ```
  2. 只要 reporter 監看任何佇列（非 dry run 的 Source 從 `source.rs:263` 開始），每個 server 每 250 ms 都會從資料 connection 送出一個 `Report::Queues`。
  3. 所以 `moved` 每 250 ms 就被重設，60 s 的條件永遠不成立。
- 重現方式：`FERRY=<ferry> sh docs/review/2026-10-17/materials/source/repro-fifo.sh 75`。輸出中只有 `where every thread is, as asked:`（SIGQUIT 觸發的），沒有 `nothing has passed`。要確認自動快照，把參數設成 130，讓停滯超過 60 s。
- 修法：
  1. 在 `pass` 中，只有轉送給另一個 server 的 packet 才算經過：把 `passed.store(...)` 從 `Ok(Some(packet))` 分支移到迴圈末尾 `sending.send(packet)` 之前（`Report` 分支在那之前就 `continue` 了）。
  2. `reports.try_send` 滿了時那個迴圈中的加一（`relay.rs:190`）保留：那是顯示跟不上，不是 server 停住，註解已說明這個用意。
  3. 為了可測試，把判斷抽成一個函式，例如 `fn moves(kind: PacketType) -> bool { kind != PacketType::Report }`。
  4. 讓 `STILL` 在 debug build 可由環境變數覆寫（例如 `FERRY_TEST_STILL_SECS`），以便整合測試。
  5. 預期的副作用：Source 在 NFS 上花好幾分鐘列出一個巨大目錄時，沒有 packet 經過，60 s 後會印出一次快照（`list directories`）。這正是設計想要的行為，而且每次停滯只說一次。
- 要加的測試：
  - `src/relay.rs` 的 `mod tests`：`moves()` 的單元測試。
  - `tests/copy.rs` 整合測試（debug build）：`FERRY_TEST_STILL_SECS=2`，再加一個讓 reader 停 5 s 的注入點（沿用 SRC-03 的機制，例如 `FERRY_TEST_STALL=reader`），斷言 stderr 含 `nothing has passed between the servers`，而且傳輸照常完成。
- 驗收條件：
  - [ ] `repro-fifo.sh 130` 在沒有 SIGQUIT 的情況下，也印出 `nothing has passed between the servers for 60 s`（SRC-01 修好之前）
  - [ ] 正常傳輸中不會誤報
  - [ ] 整合測試通過
- 文件要同步的地方：`docs/design/profiling.md` 的「A snapshot of every thread」與 `profiling.zh-TW.md` 的「每條 thread 的快照」：寫明「經過」指的是兩個 server 之間轉送的 packet，不包括它們給 client 的 report。`docs/cli.md` 的「Output」與 `cli.zh-TW.md` 的「輸出」中同一句（60 s）可以不改，或補上同樣的說明。
- 相關項目：SRC-01（實測時的停滯）、SRC-08（另一個取得快照的途徑）。

---

### SRC-06：每個需要的檔案都重新以路徑解析根目錄並逐層開啟

- 嚴重度：低
- 驗證狀態：已核對程式碼（沒有量測系統呼叫數或 NFS RPC 數）
- 位置：`src/source.rs:795-805`（`open_source_file`）、`src/fsys.rs:35-43`（`open_root`，跟隨符號連結）、`src/fsys.rs:172-187`（`open_path`）
- 現象：
  - 效能：每讀一個檔案，都要 `openat(CWD, root)` 一次、`dup` 一次，再對路徑的每一層 `openat` 一次。深度為 d 的檔案要 d + 2 次 open 與 close。在 NFS 與 Lustre 上，每次開啟目錄都可能是一次重新驗證的 RPC，這與 pipeline.md「以相對於父目錄的 fd 開啟，省下路徑查找」的理由相反。
  - 一致性：使用者所指名的根，或它的任一祖先，若在傳輸途中被改成指向別處（例如 `current -> release-42` 被切換成 `release-43`），之後的檔案就從另一棵目錄樹讀取，卻用第一棵的 ID 與名稱寫到 Destination，結果混合了兩棵目錄樹。
- 根本原因：
  ```rust
  // src/source.rs:795-805
  fn open_source_file(root: &OsStr, path: &[u8]) -> io::Result<File> {
      if path.is_empty() {
          return File::open(root);
      }
      let (directory, name) = match path.iter().rposition(|&byte| byte == b'/') {
          Some(slash) => (&path[..slash], &path[slash + 1..]),
          None => (&b""[..], path),
      };
      let directory = fsys::open_path(fsys::open_root(root)?, directory)?;
      fsys::open_file(directory, name)
  }
  ```
  掃描用的根 fd（`scan.rs` 的 `fsys::open_root`）只活在掃描期間，reader 拿不到。
- 重現方式：以 `strace -f -e trace=openat -c` 計算 Source server 在 copy 時開啟目錄的次數（這台機器沒有 strace，所以沒有實測）。一致性問題：根是符號連結，copy 進行到一半時把它改指向另一棵結構相同的目錄樹，Destination 會得到混合的內容。
- 修法：
  1. 在 `source::run` 開始時開一次根，`Arc<OwnedFd>`；是單一檔案時則不需要。這個 fd 交給掃描（`scan_in_parts` 改為接受已開啟的根，或另外提供一個變體）與 readers 共用，不再以路徑重新解析。
  2. readers 共用一個小的目錄 fd 快取：`Mutex<Vec<(Vec<u8>, Arc<OwnedFd>)>>`，或簡單的 LRU，容量 32。依目錄路徑查找；沒有的，就從最長的已快取祖先（至少是根）以 `fsys::open_path` 開啟，再放進快取。manifest 依目錄依序送出，needs 也大致依序到達，命中率會很高。fd 數量的上限就是快取容量，所以不會把 `Known` 的每一段都留著 fd（那會造成 EMFILE）。
  3. 語意：快取的目錄 fd 指向它被開啟時的那個目錄。之後若該目錄被改名或取代，檔案仍從原來的 inode 讀取，比現在更接近「掃描當時」的快照，與 Destination 端「刪除的對象就是列出的那個目錄」的原則一致。
  4. 單一檔案的根，見 SRC-01 的 `open_regular_root`。
  5. Linux 與 macOS 沒有差別。
- 要加的測試：
  - `src/source.rs` 的 `mod tests`：快取的單元測試（命中、逐出、從祖先開啟）。
  - `tests/copy.rs`：根是符號連結時照常複製（現有的行為）；另一個測試在第一個檔案讀完後切換根的符號連結，斷言 Destination 只有第一棵目錄樹的內容。後者需要注入點來控制時機（沿用 SRC-03 的機制，例如讓 reader 讀完第一個檔案後停住）。
- 驗收條件：
  - [ ] 每個檔案的 `openat` 次數，從 d + 2 降到平均約 1（命中時）
  - [ ] 根的符號連結在傳輸途中被切換時，不會混合兩棵目錄樹
  - [ ] `cargo test` 通過
- 文件要同步的地方：`docs/design/pipeline.md` 的「1. Scan」中「Directories are opened relative to their parent's file descriptor」與 `pipeline.zh-TW.md` 的「1. 掃描」中「目錄以相對於其父目錄檔案描述子的方式開啟」：補上讀取時也是如此，根只解析一次。
- 相關項目：SRC-01（同一個函式）、SRC-07（fd 數量）。

---

### SRC-07：掃描開著的 fd 隨目錄樹深度增加，且無人考慮 `RLIMIT_NOFILE`

- 嚴重度：低（要很深的目錄樹才會觸發：Linux 預設的 soft limit 1024 約要 900 層以上，macOS 預設的 256 約 230 層）
- 驗證狀態：已實測重現。400 層深、每層都有一個兄弟目錄 `z` 的目錄樹，在 `ulimit -n 256` 下執行 `ferry copy --dry-run`：在約 230 到 240 層處出現 `cannot read a/a/…/a: Too many open files (os error 24)`，其下的一切都被略過，結束狀態為 1。見 `repro-depth-fds.sh`。
- 位置：
  - `src/scan.rs:132-137`：`Waiting.parent` 是 `Arc<OwnedFd>`
  - `src/scan.rs:203-209`：每個子目錄都 clone 一份父目錄的 `Arc`
  - `src/scan.rs:39-40`：`WAITING = 64`，每一段 `Job::Part` 都持有其目錄的 `Arc<OwnedFd>`
  - 整個 `src/` 沒有任何 `setrlimit`
- 現象：很深的目錄樹在 fd 上限較低的 host 上，部分子樹無法讀取並被略過。會回報，所以不是靜默的資料遺失，但這次執行不完整。
- 根本原因：
  1. 一個目錄的所有子目錄共用父目錄 fd 的 `Arc`，所以要等最後一個兄弟被取出，父目錄才會關閉：
     ```rust
     // src/scan.rs:203-209
     for name in subdirectories.into_iter().rev() {
         waiting.push(Waiting {
             parent: Arc::clone(&directory),
             path: manifest::join(&path, &name),
             name,
         });
     }
     ```
  2. 深度優先時，目前路徑上每一個「還有兄弟沒走」的祖先都開著，所以 fd 數約等於深度 ＋ 64 段 ＋ readers 的 fd。
  3. 沒有程式把 soft limit 提高到 hard limit。Rust std 不會這麼做，Go 才會。
- 重現方式：`FERRY=<ferry> sh docs/review/2026-10-17/materials/source/repro-depth-fds.sh 400 256`，印出 `exit status: 1` 與 `fails at depth …`。
- 修法：
  1. 共用的修法：啟動時提高 `RLIMIT_NOFILE`。這與 Destination 審查中「批次中每個檔案佔 3 個 fd」那一項是同一個修法，請以那一項為主，只實作一次：在 `cli::run` 一開始把 soft limit 設為 hard limit。
     - macOS 的 hard limit 常是 `RLIM_INFINITY`，但 `setrlimit` 超過 `OPEN_MAX`（10240）或 `kern.maxfilesperproc` 會得到 `EINVAL`，所以要取 `min(hard, OPEN_MAX)`，失敗時退回原值。
     - Linux 直接取 hard limit（systemd 底下常是 524288 或更大）。
     - client 與 server 都會執行 `cli::run`，所以兩邊都會生效。
  2. 掃描本身再加一道保險，在 hard limit 很低時也不失敗：限制同時開著的祖先數。
     - `Waiting` 改成 `enum Parent { Open(Arc<OwnedFd>), Reopen }`。目前路徑上開著的父目錄數超過 `MAX_OPEN_PARENTS`（例如 128）時，新推入的子目錄改用 `Reopen`。
     - 取出 `Reopen` 時，以 `fsys::open_path(&root, parent_path)` 從根逐層開啟父目錄，再 `open_directory(name)`。根的 fd 由 lister 保留。`open_path` 每一層都帶 `O_NOFOLLOW` 並拒絕 `.` 與 `..`，安全性不變。
     - 代價是很深的目錄樹每個目錄多 O(深度) 次 `openat`，只發生在超過門檻之後。
     - 語意上，以路徑重新開啟可能進到已被改名或取代的目錄，與 reader 現在的行為相同，而且只發生在極深的目錄樹。
- 要加的測試：
  - `tests/copy.rs`：以 `sh -c 'ulimit -S -n 128 && exec "$0" "$@"' <ferry> copy --dry-run SRC DST` 執行 300 層深的目錄樹，只降 soft limit，斷言成功（驗證第 1 步）。不要用不帶 `-S` 的 `ulimit -n`：bash 會連 hard limit 一起降。
  - `src/scan.rs` 的 `mod tests`，只在 Linux：掃描 300 層深的目錄樹，sink 每收到一個訊息就計算 `/proc/self/fd` 的項目數，斷言最大值 ≤ `MAX_OPEN_PARENTS` ＋ `WAITING` ＋ 某個小常數（驗證第 2 步）。
- 驗收條件：
  - [ ] `repro-depth-fds.sh 400 256`（bash 的 `ulimit -n` 同時降 hard limit）在第 2 步之後成功
  - [ ] 只降 soft limit 時，靠第 1 步成功
  - [ ] 一般目錄樹的掃描速度不變（`MAX_OPEN_PARENTS` 以下沒有額外的 `openat`）
- 文件要同步的地方：`docs/design/pipeline.md` 的「1. Scan」與 `pipeline.zh-TW.md` 的「1. 掃描」：在「目錄以相對於其父目錄檔案描述子的方式開啟」之後補一句：同時開著的祖先有上限，更深的從根重新開啟。`RLIMIT_NOFILE` 的說明由 Destination 那一項負責（`docs/design/architecture.md` 或 `docs/cli.md` 的「Environment」）。
- 相關項目：Destination 審查中的 `RLIMIT_NOFILE` 項目（共用的修法）、SRC-06（readers 的 fd 快取）。

---

### SRC-08：儀表板顯示時，Ctrl-\ 不會產生快照

- 嚴重度：低
- 驗證狀態：已核對程式碼。crossterm 的 `enable_raw_mode` 以 `cfmakeraw` 清掉 `ISIG`，所以終端機不會因 Ctrl-\ 送出 SIGQUIT；儀表板也只處理 Ctrl-C 與 `t`。
- 位置：
  - `src/dashboard.rs:101`：`enable_raw_mode`
  - `src/dashboard.rs:144-160`：按鍵處理
  - `src/interrupt.rs:148-151`：`quit_asked`
  - `src/relay.rs:308`：relay 依 `quit_asked()` 決定是否印出快照
- 現象：在終端機上（預設會顯示儀表板），文件說的「按 Ctrl-\ 說明每條 thread 在哪裡」沒有作用；只有從另一個終端機 `kill -QUIT` 才行。
- 根本原因：
  ```rust
  // src/dashboard.rs:148-156
  if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
      // Raw, the terminal sends no signal for Ctrl-C: the process
      // sends itself one, which cancels the transfer as usual.
      restore();
      // SAFETY: `kill` with this process's own pid and a signal.
      unsafe {
          libc::kill(libc::getpid(), libc::SIGINT);
      }
  } else if key.code == KeyCode::Char('t') {
  ```
  Ctrl-C 有特別處理，Ctrl-\ 則沒有。
- 重現方式：在終端機上執行一個較長的 `ferry copy`，按 Ctrl-\。沒有任何反應；儀表板結束後的輸出中也沒有快照。
- 修法：
  1. 在 `interrupt.rs` 新增 `pub fn ask_for_snapshot() { QUIT.store(true, Ordering::Relaxed); }`。不需要真的送訊號，`quit_asked` 讀的就是這個旗標。
  2. 儀表板的按鍵處理呼叫它。crossterm 在一般終端機上，把 Ctrl-\（0x1C）解析成 `KeyCode::Char('4')` 加 `CONTROL`；在支援 kitty 鍵盤協定的終端機上，則可能是 `Char('\\')` 加 `CONTROL`。兩種都要接受。
  3. 快照的行是 `Report::Said`，儀表板會把它們放進「最新的警告」，結束後再寫到 stderr。這與沒有儀表板時「立刻寫到 stderr」不同，要在文件中說明，或讓儀表板把它們另外存起來。
  4. 順便確認一個相同性質的問題：Ctrl-Z 在 raw mode 也不會暫停。這可以不處理，但要在文件中說明。
- 要加的測試：`src/dashboard.rs` 的 `mod tests`：把按鍵對應抽成 `fn action(key: &KeyEvent) -> Option<Action>`，以單元測試斷言 Ctrl-C 對應取消，Ctrl-4 與 Ctrl-\ 對應快照。
- 驗收條件：
  - [ ] 在儀表板上按 Ctrl-\，快照出現在儀表板的訊息中，以及結束後的 stderr
  - [ ] 單元測試通過
- 文件要同步的地方：
  - `docs/design/dashboard.md` 的「How it behaves」與 `dashboard.zh-TW.md` 的「行為方式」：「No other key does」要加上 Ctrl-\（不取消，只要求快照）。
  - `docs/design/profiling.md` 的「A snapshot of every thread」與 `profiling.zh-TW.md` 的「每條 thread 的快照」：儀表板上同樣有效，並說明快照顯示在哪裡。
- 相關項目：SRC-05。

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

### SRC-10：讀取來源檔案沒有用 `O_NOATIME`
- 嚴重度：低。驗證狀態：已核對程式碼。位置：`src/fsys.rs:314-316`。
- 說明：在 Linux 上，每次讀取都可能更新 atime；`relatime` 會緩和，但在 `strictatime` 的掛載上，每個檔案都多一次中繼資料寫入。rsync 為此有 `--open-noatime`。
- 修法：在 Linux 上加 `O_NOATIME`。它只對檔案擁有者或有 `CAP_FOWNER` 的 process 有效，否則 `open` 會得到 `EPERM`，所以遇到 `EPERM` 就不帶它重試一次，並記住，之後不再嘗試。macOS 沒有這個 flag。應與 SRC-01 的 `open_regular_file` 一起實作。

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

### SRC-13：讀取期間就地改寫、mtime 落在同一秒的檔案，下次執行不會重送
- 嚴重度：低。驗證狀態：已核對程式碼（推論，未實測）。
- 位置：`src/source.rs:707-711`（大小與 mtime 取自開啟時的 `fstat`）、`src/source.rs:741-783`（讀完之後不再檢查）。
- 說明：一個檔案在讀取期間被就地改寫，大小不變（例如資料庫或 VM 映像檔），內容就是新舊混合，而 record 帶的是開啟時的 mtime。若改寫的 mtime 與開啟時的 mtime 在同一秒內，在預設以整秒比較的 quick check 下，下次執行會認為兩邊相同，永遠不會修正。rsync 也是如此，所以這不是退步，但與 overview 中「每個檔案都完整正確」的目標有落差。
- 修法：讀完之後再 `fstat` 一次；`st_mtime`、`st_ctime` 或 `st_size` 任一改變，就在 trailer 標記 `unread = "it changed while it was read"`，讓 Destination 丟棄。下次執行就會重送。

### SRC-14：掃描期間消失的項目被算成「無法讀取」，結束狀態為 1
- 嚴重度：低。驗證狀態：已核對程式碼。
- 位置：`src/scan.rs:237-246`（stat 失敗一律記為 problem）、`src/source.rs:526-541`（`problem`）、`docs/cli.md` 的「Exit status」。
- 說明：在一棵會變動的目錄樹上（例如正在寫 log 的目錄），列出之後、stat 之前被刪掉的檔案，會被回報為 `cannot read X: No such file or directory`，並讓結束狀態變成 1。rsync 把這種情況另外算作「vanished」（結束狀態 24），腳本通常把它當成成功。在這種目錄樹上，cron 備份會一直「失敗」。
- 修法：
  - `stat_part` 中 stat 得到 `ENOENT` 時，視為已消失：不放進 `entries`，改記為「vanished」，以警告（例如 `vanished X`）回報，而不是 problem。
  - 不影響結束狀態，或比照 rsync 另給一個結束狀態（例如 24），兩者擇一並寫進文件。
  - 對 `sync` 而言，不送出這個 entry，代表 Destination 會把同名的項目當成多餘的刪除，這正是 Source 已經沒有它時的正確結果；rsync 也是如此。
  - 其他錯誤（`EACCES`、`EIO` 等）維持現狀：送出為 Unreadable，Destination 保留原樣。
  - ID 在 `send_found` 中才指派，所以略過一個 entry 不影響 ID 的連續性。

> 2026-10-20 補記：WP-07 照修法做：`stat_part` 得到 `ENOENT` 時不送出這個 entry，記為 vanished，`Sink::vanished` 讓 Source 說「warning: X vanished before it could be read」並計入新的 `Count::Vanished`；client 結束時說「N paths in SOURCE vanished before they could be read」。結束狀態：D7 決定比照 rsync，vanished 用 24，這在 WP-13 實作；在那之前，vanished 不影響結束狀態（0）。掃描之後、讀取檔案時才消失的檔案，仍照舊算成無法讀取，這不在這一項的範圍內。測試：`a_name_gone_before_its_stat_has_vanished`，另把原本的 `a_name_that_cannot_be_statted_is_sent_as_unreadable` 改成以不能搜尋的目錄觸發 `EACCES`。

### SRC-15：`send_found` 的 `expected` 與 lister 的 `waiting` 各存一份待列出目錄的路徑
- 嚴重度：低（記憶體）。驗證狀態：已核對程式碼。
- 位置：`src/scan.rs:293`、`:323`（`expected`）、`src/scan.rs:143`、`:203-209`（`waiting`）。
- 說明：一個有幾百萬個子目錄的目錄，兩邊會各存一份全部子目錄的完整路徑。
- 修法：`expected` 只需要判斷「這個目錄是否被父目錄以目錄的身分送出」，可以改成記錄被送出的目錄 entry 的 ID，或由 lister 在 `Job::Part` 中帶上該目錄在父目錄中的 entry 位置，`send_found` 再以 ID 查證，不必複製路徑。

### SRC-16：`Known::compared_through` 的 `id + 1` 可能溢位
- 嚴重度：低。驗證狀態：已核對程式碼。位置：`src/source.rs:129-133`。
- 說明：`Needs::Through(u64::MAX)`（Destination 的 bug 或惡意）在 debug build 會 panic，進而依 SRC-03 停滯；在 release build 會 wrap，什麼都不修剪。
- 修法：`part.end() <= id.saturating_add(1)`，並在 `receive` 中拒絕大於已送出最大 ID 的 `Through`，以錯誤結束傳輸。

### SRC-17：收到訊號時，終端機還原可能與儀表板的繪製交錯
- 嚴重度：低（外觀）。驗證狀態：推測，未實測。
- 位置：`src/interrupt.rs:93-97`（訊號 thread 呼叫 `before_exit`，也就是儀表板的 `restore`）、`src/dashboard.rs:161-165`（儀表板 thread 照常 `screen.draw`）。
- 說明：收到 SIGTERM 或 SIGHUP 時，訊號 thread 離開 alternate screen，同時儀表板 thread 可能正在畫一個 frame，或在 `exit` 之前又畫一次，主畫面上就會留下殘影。Ctrl-C 從儀表板按下時，是儀表板 thread 自己先 `restore`，所以不受影響。
- 修法：在 `restore` 之後設一個旗標，儀表板 thread 每次繪製前檢查它；或在 `before_exit` 中先取得儀表板繪製用的鎖。SRC-03 的 panic hook 也要用同一套機制。

## 給記憶筆記的更正

- `Known` 不再是永不修剪：4086576 之後，以 `Needs::Through` 修剪。
- Destination 的 decoded-data channel 已有上限（f3f6653，`DECODED = 8`），報告的 channel 也已有上限（099b1d1，16）。
- 掃描已依待送檔案數節流（343aae7），但那是軟上限，見 SRC-04。
- 8659e16 對有界的 pool 是正確的：項目數絕不超過上限，所以輸入的 `sync_channel` 不會阻塞；collector 被丟掉時，等待中的 submitter 會被放開；計數不會變成負數。唯一的缺口是 worker panic 時，那個項目永遠不會離開，見 SRC-03。
- `ordered_unbounded` 實際上受限於約 77 段，因為 Destination 的主 thread 是它唯一的上游。而這些段正是 SRC-04 超出上限的主要原因。
