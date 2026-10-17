# 架構改動提案：交接文件

- 對象：HEAD `3c17a2f`（version 0.14.4）。行號都以這個 commit 為準。
- 這份文件提的是架構改動的方案，不是缺陷清單。缺陷在其他五份檔案裡；這裡的每個方案，會一併吸收其中一部分項目，寫在各方案的「吸收的項目」。
- 結論先說：**不需要大翻修**。不換語言、不換成 async、不換 SSH 模型，也不拿掉 client relay。有四個結構性改動值得做（ARC-01 到 ARC-04），另有幾個小調整（ARC-05）。
- 每個方案都會改變某些已寫進設計文件的決定，或增加依賴。動手前，先請使用者確認 [README](README.md) 第 7 節列出的對應決定（D14 到 D18）。
- 這些方案最後收斂成的長期設計，寫在 [target-architecture.md](target-architecture.md)。它的第 15.2 節說明 ARC-01 到 ARC-04 在那條路上的位置。

## 總表

| ID | 方案 | 依據 | 建議時機 |
|---|---|---|---|
| ARC-01 | 寫入 stage 要配合檔案系統的目錄鎖：Linux 改用 `O_TMPFILE` 加 `linkat`；改名做成常駐的 stage；fsync 獨立成一個 stage | 已量測：小檔情境的系統時間是瓶頸，同一目錄的鎖競爭很明顯 | 第三波，在 ARC-03 之後 |
| ARC-02 | thread 的組合方式：加一層 supervised stage，channel 換成 crossbeam-channel | 報告中最大的一類 bug（PRO-04、PRO-05、SRC-02、SRC-03）都出自這裡 | 第二波，和 ARC-03 一起做 |
| ARC-03 | Destination 的狀態改成單一擁有者，並加一層兩邊共用的目錄 handle | `destination.rs` 有 2,643 行、25 處上鎖；DST-09、PRO-07 | 第二波，和 ARC-02 一起做 |
| ARC-04 | 大檔案的 record 改成可以分段：平行讀寫、可續傳，也是 sparse 與 delta 的基礎 | 已量測：單一大檔受限於一條 writer thread | 先量測工作負載，再決定 |
| ARC-05 | 小的結構調整：本機複製不壓縮、修正 profile 的失真、限制 CPU 用量 | 已量測 | 隨時 |

---

## 量測

### 環境與方法

- 機器：Linux 5.15，x86_64，96 核心。目錄樹放在 `/home`（md RAID 上的 ext4）。release build。兩側都在本機。
- 每次執行前，先把來源讀進 page cache，所以量到的是 pipeline 本身，不含讀取磁碟的時間。
- 各 stage 的時間取自 `--profile-file` 的 JSON，用附錄 A 的 `prof.py` 彙整。原始的 JSON 在 [`materials/arch/profiles/`](materials/arch/profiles/)。「busy」是不在等待的時間，多條 thread 的時間相加。
- 換成 NVMe、xfs、btrfs、NFS 或 macOS（APFS），數字會不同。每個方案在動手前後都要重新量測（見各方案的「量測計畫」）。

### 結果

| 情境 | `cp -a` | `rsync -a` | ferry `--no-fsync` | ferry（預設，含 fsync） |
|---|---|---|---|---|
| 20 萬個 4 KiB 小檔，200 個目錄，每個目錄 1,000 個 | 4.7 s（user 0.3 s，sys 4.5 s） | 10.9 s（user 1.6 s，sys 11.2 s） | 10.1 s（user 11.6 s，**sys 84.6 s**） | 38.7 s（user 19.0 s，sys 70.3 s） |
| 1 個 4 GiB 的隨機資料檔 | 2.5 s（sys 2.5 s） | — | 3.6 s（user 13.0 s，sys 17.1 s） | — |

**20 萬個小檔，`--no-fsync`，Destination 的 profile（10.05 s）：**

| role | thread 數 | busy | 說明 |
|---|---|---|---|
| writer | 16 | 69.7 s，其中 write files 69.5 s | 每個 4 KiB 的檔案約 350 µs |
| rename | 6,256 | 29.2 s | `rename_batch` 每批 256 個檔案就開 8 條 thread：782 批 × 8 |
| parser | 1 | 7.17 s（72%），其中 rename files 4.87 s | parser 有將近一半的時間在等批次改名 |
| finisher | 1 | 10.03 s（100%） | **失真**：它其實在等 channel，見 ARC-05b |
| receive | 1 | 0.74 s | |
| decompress | 8 | 0.44 s | |

同一次執行，Source 端的 16 個 reader 合計 11.6 s（每個檔案約 58 µs，見 SRC-06），compress 合計 1.9 s，其他 role 都很閒。

**20 萬個小檔，含 fsync（38.7 s）：** writer 有 16 條，合計 busy 491 s，等於每條 80%，是瓶頸，主要時間都在 fsync。rename 合計 63.8 s。parser 的 rename files 為 9.5 s。

**4 GiB 單一檔案，`--no-fsync`（3.57 s）：**

| 側 | role | busy | 說明 |
|---|---|---|---|
| Destination | writer | 3.45 s | 只有一條 writer 在寫這個檔案，忙了 97% 的時間 |
| Source | file sender | 2.24 s，其中 read files 2.20 s | 大於 1 MiB 的檔案由這一條 thread 讀取並雜湊（SRC-11） |
| Source | compress | 4.38 s，8 條合計 | 本機複製，而且是壓不下去的隨機資料 |
| client | relay from the Source | 0.07 s | relay 不是瓶頸 |

### 目錄鎖的微基準

用附錄 B 的 `dirbench.py`：每種情況寫 4 萬個 4 KiB 的檔案，量 wall time 與 process 的系統時間。這是 Python 寫的，有 GIL，吞吐量大概卡在每秒 5 萬個左右，所以要看的是**系統時間**的差距。

| 寫法 | 1 thread，同一目錄 | 16 threads，同一目錄 | 16 threads，分散到 16 個目錄 |
|---|---|---|---|
| 暫存名稱，再 `rename`（Ferry 現在的做法） | 1.27 s，sys 1.21 s，31.5k 檔/秒 | 1.37 s，**sys 18.73 s**，29.2k 檔/秒 | 0.77 s，sys 2.58 s，52.1k 檔/秒 |
| `O_TMPFILE`，再 `linkat` | 0.82 s，sys 0.77 s，48.9k 檔/秒 | 0.77 s，sys 2.92 s，52.2k 檔/秒 | 0.78 s，sys 2.27 s，51.5k 檔/秒 |

解讀：

- 在同一個目錄中，用暫存名稱再改名，開 16 條 thread 並不比 1 條快，系統時間卻是 15 倍。每個檔案要做兩次目錄操作：建立暫存名稱、改名。在 Linux 上，兩者都要獨占該目錄 inode 的 `i_rwsem`，16 條 thread 就在 kernel 裡搶這把鎖。
- 改用 `O_TMPFILE` 之後，建立檔案時不會新增目錄項目，每個檔案只剩 `linkat` 一次目錄操作，競爭幾乎消失。
- Ferry 的 manifest 依目錄順序前進，需要的檔案也是一個目錄接著一個目錄送來，所以 16 個 writer 與 8 個 renamer 幾乎總是擠在同一個目錄裡，正好落在上表最差的那一格。

---

## ARC-01：寫入 stage 要配合檔案系統的目錄鎖

- 嚴重度：效能，高。小檔是最常見的工作負載，而這裡用了 `cp` 的 20 倍 CPU，速度卻只有一半。
- 依據：上面的量測與微基準。
- 吸收的項目：DST-02 在 Linux 上的大部分情形（`O_TMPFILE` 的檔案沒有名稱，不會殘留）、WP-17 中與寫入相關的效能部分。
- 需要使用者決定：**D14**（Linux 上以沒有名稱的暫存檔取代 `.name.ferry-XXXX`，這會改到 overview 裡已是「Decided」的設計）。

### 現在的寫入路徑

- 每個需要的檔案由 parser 交給 16 個 writer 之一（`src/destination.rs:1348-1518` 的 `write_files`）。
- writer 在 `write_file`（`:1627-1708`）中依序：
  1. 以 `fsys::create_temporary`（`src/fsys.rs:332-348`）建立 `.name.ferry-XXXX`（`O_CREAT|O_EXCL`）；
  2. 寫入並雜湊；
  3. 驗證；
  4. 設定屬性；
  5. fsync；
  6. 回傳 `Ready`。
- parser 收到後把它放進批次，滿 256 個就呼叫 `rename_batch`（`:1525-1570`）。這個函式在 macOS 上先設屏障，再**每批新開最多 8 條 scoped thread** 平行改名，期間 parser 一直等著它。

### 01a. Linux 改用 `O_TMPFILE` 加 `linkat`

做法：

1. 新增 `fsys::create_unnamed(directory) -> io::Result<Option<File>>`：
   - 呼叫 `openat(directory, ".", O_TMPFILE | O_WRONLY | O_CLOEXEC, 0o600)`。rustix 有 `OFlags::TMPFILE`。
   - **不要**加 `O_EXCL`：加了之後，這個檔案就永遠不能被 link。
   - 回傳 `EOPNOTSUPP`、`EISDIR`、`ENOTSUP` 或 `EINVAL` 時，表示這個檔案系統不支援。回傳 `Ok(None)`，並以 `st_dev` 為鍵記住，之後同一個 device 直接走舊路徑。舊 kernel 不認得 `O_TMPFILE`，會把它當成 `O_DIRECTORY` 處理，所以也要涵蓋 `EISDIR`。
2. 新增 `fsys::link_unnamed(file, directory, name) -> io::Result<()>`：
   - 呼叫 `linkat(AT_FDCWD, "/proc/self/fd/<N>", directory, name, AT_SYMLINK_FOLLOW)`。
   - 不用 `AT_EMPTY_PATH`：在不少 kernel 上，它需要 `CAP_DAC_READ_SEARCH`。
   - 沒有 `/proc` 時會得到 `ENOENT`，這時改走舊路徑。這個檔案就丟掉；它沒有名稱，關掉 fd 就會消失。
3. writer 的流程改成：
   - 先用 `create_unnamed` 建立檔案，再寫入、雜湊、驗證，接著 fchown、fchmod、futimens。這些都對 fd 操作，對沒有名稱的檔案一樣有效。
   - 然後 fsync，再交給改名的 stage（01b）。
4. 改名的 stage 依目的地原本有沒有這個名稱，分兩種：
   - **目的地原本沒有這個名稱**（比較時就知道，`Target` 多帶一個 `exists: bool`）：直接 `link_unnamed` 到最終名稱，只需要**一次**目錄操作。若得到 `EEXIST`（比較之後有人建立了同名檔案），改走下一種。
   - **目的地已有這個名稱**：先 `link_unnamed` 到暫存名稱 `.name.ferry-XXXX`，再 `renameat` 蓋過去。和現在一樣是兩次目錄操作。暫存名稱只存在於 link 與 rename 之間的極短時間，仍要向 `interrupt` 登記。
   - `clear_the_way`（`:1877-1890`）照舊：最終名稱是目錄，而且是 `sync` 時，先刪除它。
5. 持久性：
   - 檔案的 fsync 在 link 之前，與現在的「資料先於名稱」相同。
   - link 之後的目錄 fsync，照舊由目錄收尾負責。
   - link 後的 inode mtime 不變（link 只改 ctime），所以 futimens 可以在 link 之前做。
6. 中斷與失敗：
   - 沒有名稱的檔案不需要向 `interrupt` 登記。行程無論怎麼結束，包括 `SIGKILL`，它都會隨著最後一個 fd 關閉而消失。
   - DST-02 的 guard 仍然需要，用在舊路徑與暫存名稱那一小段時間。
7. 平台：
   - macOS 沒有 `O_TMPFILE`，維持現在的路徑。
   - NFS 不支援，會自動退回舊路徑。
   - ext4（3.11 起）、xfs（3.15 起）、btrfs（3.16 起）、tmpfs、f2fs 都支援。overlayfs 與 FUSE 視情況而定，由第 1 步的偵測處理。

取捨：

- 好處：在 Linux 上，第一次複製時每個檔案的目錄操作從 2 次減為 1 次；同一目錄的競爭幾乎消失（微基準：sys 從 18.7 s 降到 2.9 s）；`kill -9` 也不會留下暫存檔。
- 代價：
  - 寫入路徑分成兩條（Linux 支援時，以及退回時），兩條都要測。
  - `/proc/self/fd` 是另一個依賴。
  - overview 寫的「暫存名稱 `.foo.ferry-XXXX`」要改寫成「Linux 上是沒有名稱的檔案，寫完後才給名稱；其他情況用暫存名稱」。

### 01b. 改名或 link 做成常駐的 stage，不讓 parser 等

- 現在：`rename_batch` 每批新開 8 條 thread，parser 同步等它，在 20 萬個檔案的情境中等了 4.87 s。
- 改成：
  - 一個常駐的「改名」pool，例如 `pool::threads(8)`，在 `write_files` 開始時建立。
  - parser 把滿了的批次交給它，就繼續讀 record 串流。
  - 改名結果經 channel 回到 parser 的 `settle` 迴圈，在那裡送 `Results`、更新計數、呼叫 `settled`。
- 在途的批次數要有上限，例如 2，否則 fd 用量會無限增長，見 DST-03。
- 順序：
  - 一個檔案的 `Results` 仍在它改名之後才送。
  - `Header::End` 之後，parser 等所有在途批次都完成，才 flush 最後一批，然後結束。`Done` 的時機不變。
- macOS 的屏障：仍在一批的改名之前，對每個 device 設一次，由改名 stage 在處理該批時執行。

### 01c. 同一目錄內的並行數：先量測，再決定

- 01a 做完之後，Linux 上的競爭大多已經消失（微基準中，16 threads 在同一目錄時 sys 為 2.9 s）。若量測顯示仍有競爭，或 macOS（APFS）有類似的問題，再加這一步：在本機檔案系統上，限制同一目錄同時寫入的檔案數，例如 2 個。網路檔案系統受延遲限制，維持寬的並行。以 `statfs` 的 `f_type` 判斷。
- 注意：parser 是以串流方式把 record 交給 writer 的。若某個 writer 因為目錄的上限而等待，parser 就會跟著停住。所以要做這一步，小檔案（例如 ≤ 1 MiB）的 record 要先在 parser 裡讀成完整的緩衝區，再交給 writer，讓 parser 不必等。Source 端本來就是這樣處理小檔案的（`src/source.rs:577` 的 `SMALL`）。
- 在 NFS 上，平行建立檔案可能正是現在這個設計的理由。沒有 NFS 的量測之前，不要縮減網路檔案系統上的並行。

### 01d. fsync 獨立成一個 stage

- 現在：每個 writer 自己 fsync。開 fsync 時，16 個 writer 有 80% 的時間都忙，是瓶頸。fsync 受延遲限制，不需要 CPU。同時發出的 fsync 愈多，檔案系統愈能把它們合併成一次 journal commit。
- 改成：writer 驗證並設定屬性之後，把 fd 交給 fsync pool，例如 32 到 64 條 thread，可以設定；完成後再交給改名 stage。writer 隨即回去寫下一個檔案。
- 另一種做法是 Linux 本機檔案系統上，每批呼叫一次 `syncfs(2)`，取代每個檔案各自 fsync。它會順帶寫出同一檔案系統上其他 process 的髒資料，所以只在量測後確定值得時才考慮，網路檔案系統上絕不使用。建議先做加大 fsync 並行數的做法。
- 並行數的上限要納入 fd 預算（DST-03）：每個等待 fsync 的檔案都占一個 fd。

### 量測計畫

每一步前後，都用同一組情境量測，把數字寫進 commit 訊息：

```sh
cargo build --release && F=$PWD/target/release/ferry
# 產生 20 萬個 4 KiB 檔案：用附錄 C 的腳本，或 ferry test --scale 1G --kind small
$F copy --no-ui --no-fsync --profile-file a.json SRC DST   # 每次執行前刪掉 DST
$F copy --no-ui --profile-file b.json SRC DST
python3 -I prof.py a.json   # 附錄 A
```

- 至少比較：wall time、user 與 sys time（用 shell 的 `time`）、writer、rename、parser 的 busy 時間。
- 另外跑 `ferry test /tmp /tmp --scale 1G --kind small` 與 `--kind mixed`。
- 有條件的話，再量 xfs 與 btrfs（`make filesystem-test FS=xfs`）、macOS，以及一台 NFS。

### 要加的測試

- `src/fsys.rs` 的 `mod tests`（Linux）：
  - `create_unnamed` 加 `link_unnamed` 會產生內容正確的檔案；
  - link 到已存在的名稱會得到 `EEXIST`；
  - 在不支援的檔案系統上回傳 `None`。這可以用一個測試用的強制開關模擬，或在 CI 的 filesystem job 中實測。
- `src/destination.rs` 的 `mod tests`：
  - 新檔案走 link 路徑，已存在的檔案走 link 再 rename 的路徑，兩者結果與現在相同；
  - 強制走舊路徑時，結果也相同。
- `tests/copy.rs`（Linux）：傳輸進行中 `kill -9` **Destination** 的 server 之後，目的地沒有任何 `.ferry-` 名稱。只有沒有名稱的暫存檔做得到這一點。
- 既有的 `ferry test` 情境，以及 `make filesystem-test` 的三種檔案系統，全部通過。

### 驗收條件

- [ ] 20 萬個小檔、`--no-fsync`：sys time 至少減半，wall time 不變慢。前後數字寫進 commit。
- [ ] 含 fsync 的情境：wall time 明顯下降。
- [ ] `rename` 的 thread 數不再隨批次增加（profile 中為固定數量）。
- [ ] parser 不再等批次改名（profile 中沒有 `rename files`，或趨近 0）。
- [ ] Linux 上 `kill -9` Destination 之後，不留下暫存檔。
- [ ] macOS 與 NFS 走舊路徑，所有測試通過。

### 文件要同步的地方

- `docs/design/overview.md` 的「Every file is complete, correct, and durable」第 1、3 點，與 `overview.zh-TW.md` 的對應段落：暫存檔的形式、批次 fsync 的方式。
- `docs/design/pipeline.md` 的「6. Write」，與 `pipeline.zh-TW.md` 的「6. 寫入」：stage 的拆分（writer、fsync、改名）與各自的 thread 數；同時更新 `docs/assets/pipeline-workers.svg`。
- `docs/cli.md` 中 Ctrl-C 與暫存檔的說明（約 `:114`），與 `cli.zh-TW.md` 的對應段落（約 `:54`）。

---

## ARC-02：thread 的組合方式

- 嚴重度：正確性，高。這是一類 bug 的共同根因。
- 吸收的項目：PRO-00、PRO-04、PRO-05、PRO-08、PRO-14、SRC-02、SRC-03（與 D4 的 panic 政策互相配合）、TRN-18；也讓 PRO-07 有乾淨的解法。
- 需要使用者決定：**D15**（新增依賴 crossbeam-channel）、**D4**（panic 政策）。

### 現在的寫法

- 每個 stage 各自用 `std::thread::scope` 加 `spawn`，最後 `join().expect("... panicked")`。
- 錯誤的傳遞只靠「channel 被關掉，下一個去讀寫它的人跟著失敗」。
- 唯一區分前因後果的是 `destination.rs` 的單向旗標 `reading_failed`（`:300-305`、`:398-413`）。
- 多個 worker 共用一個接收端時，用 `Arc<Mutex<Receiver>>` 模擬 MPMC。這出現在三個地方：`src/pool.rs:240`、`src/destination.rs:1369`（writers）、`src/source.rs:608`（readers）。閒置的 worker 都排隊等這把 mutex。
- std 的 `mpsc` 沒有 `select`。所以一條 thread 不能同時等「工作」和「停止」，也不能同時等「packet」和「比較結果」，這就是 PRO-07 的成因。

### 提案

1. **`src/stop.rs`**：照 PRO-00 的設計。
   - 它記下第一個原因，保證 `Control::Error` 只送一次，`Done` 絕不在 `Error` 之後送。
   - 「後果」型別化成 `Stopped`，永遠不會被當成原因。
   - 另外提供 `signal() -> crossbeam_channel::Receiver<Never>`。停止時，丟掉對應的 `Sender`，所有在 `select!` 中等它的 thread 都會被喚醒。這是 crossbeam 的慣用寫法。
2. **`src/stage.rs`**：包一層 `std::thread::Scope`。

   ```rust
   pub struct Stages<'scope, 'env> {
       scope: &'scope std::thread::Scope<'scope, 'env>,
       stop: &'env Stop,
   }

   impl<'scope, 'env> Stages<'scope, 'env> {
       /// Runs `work` on a thread of its own, under `role` for the profile:
       /// an error it returns, or a panic, stops every stage, and is kept as
       /// the cause if it is the first.
       pub fn spawn(&self, role: &'static str, work: impl FnOnce(&Stop) -> Result<()> + Send + 'scope) {
           let stop = self.stop;
           self.scope.spawn(move || {
               let _thread = profile::thread(role);
               let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(stop)));
               match outcome {
                   Ok(Ok(())) => {}
                   Ok(Err(error)) => stop.fail(error),
                   Err(panic) => stop.fail(anyhow!("internal error in {role}: {}", panic_message(&panic))),
               }
           });
       }
   }
   ```

   - 有了這一層，所有 thread 都經過同一個地方，`catch_unwind` 只需要寫一次，不必在每個 pool 或 thread 中各寫一次。這化解了 SRC-03 對選項 A 的主要疑慮。
   - 若使用者在 D4 選了 `panic = "abort"`，`catch_unwind` 在 release build 中不會起作用，但在測試（一律 unwind）中仍能避免停滯。兩者並不衝突。
   - `join().expect("... panicked")` 全部消失，由 `Stages` 統一處理。
3. **channel 換成 crossbeam-channel。**
   - 背景：Rust 1.67 起，std 的 `mpsc` 內部已經是 crossbeam-channel 的實作，所以行為相同，換掉的風險很低。
   - 用 `bounded(n)` 與 `unbounded()` 取代 `sync_channel` 與 `channel`。`Receiver` 可以 `clone`，所以三處 `Arc<Mutex<Receiver>>` 改成每個 worker 各自持有一份。
   - 會阻塞的等待，改成同時等工作與停止：

     ```rust
     crossbeam_channel::select! {
         recv(work) -> job => { /* … */ }
         recv(stop.signal()) -> _ => return Err(Stopped.into()),
     }
     ```

   - 用 `after(duration)` 與 `tick(duration)` 取代手寫的逾時迴圈，例如 heartbeat 與 watchdog。
   - `profile::wait` 的包裝照舊，只是包在 `select!` 外面。
   - 讀寫 socket 的阻塞呼叫，無法用 `select!` 打斷。停止時沿用 transport 現有的做法，對 socket 呼叫 `shutdown(2)`，喚醒阻塞中的讀寫。
4. **不換成 async（tokio）。** 工作都是阻塞的檔案系統呼叫。Linux 沒有成熟的非同步檔案系統 API（io_uring 另當別論），async 只會把它們丟到 blocking pool，帶不來好處，還要大改。

### 步驟（每步一個 commit，`make check` 保持通過）

1. 加入 `crossbeam-channel` 依賴，並在 cargo-deny 的允許清單中登記（若 CI-02 已經做了）。
2. `pool.rs` 改用 crossbeam：拿掉 `Arc<Mutex<Receiver>>`，保留 `InPool` 的計數語意（8659e16）。
3. 新增 `stop.rs` 與 `stage.rs`，並寫好它們的單元測試。
4. Destination 改用 `Stages` 與 `Stop`，與 ARC-03 一起做。
5. Source 改用 `Stages` 與 `Stop`。這同時完成 SRC-02 的停止旗標，以及 SRC-03 中 file sender 送出 `Control::Error` 的部分。
6. client 的 relay 改用 `Stages` 與 `Stop`。這同時完成 PRO-14 與 TRN-18。
7. 把其餘的 `mpsc` 逐步換掉。不急，可以分散在之後的工作裡。

### 要加的測試

- `src/stop.rs`：
  - 兩次 `fail` 只留第一個，而且只送一則 `Control::Error`；
  - `Stopped` 不會成為原因；
  - 停止之後，`done()` 不送 `Done`；
  - 停止會喚醒在 `select!` 中等 `signal()` 的 thread。
- `src/stage.rs`：
  - 一個 stage 回傳錯誤，其他 stage 在 `select!` 中被喚醒，scope 正常結束；
  - 一個 stage panic（unwind build），scope 正常結束，原因是 `internal error in <role>`。
- 回歸測試：PRO-04、PRO-05、PRO-06、PRO-08 的測試，以及 SRC-02、SRC-03 的注入情境。

### 驗收條件

- [ ] `src/` 中除了 `stage.rs`，沒有 `join().expect(` 這種寫法。
- [ ] 沒有 `Arc<Mutex<Receiver` 或 `Arc<Mutex<mpsc::Receiver`。
- [ ] `reading_failed` 已移除。
- [ ] PRO-00 的驗收條件全部達成。
- [ ] 任何一個 stage panic，在測試中都不會停滯，而是在數秒內以 `internal error in <role>` 結束。
- [ ] 20 萬個小檔的量測沒有變慢。

### 文件要同步的地方

- `docs/design/pipeline.md` 的「Principles」，與 `pipeline.zh-TW.md` 的「原則」：加一條「任一 stage 失敗，整個 peer 立刻停下，只回報第一個原因」。
- `docs/design/profiling.md`，與 `profiling.zh-TW.md`：thread 都由 `Stages` 建立，role 在那裡登記。

---

## ARC-03：Destination 的狀態改成單一擁有者

- 嚴重度：可維護性與效能，中高。
- 吸收的項目：DST-09、DST-16、DST-17、PRO-07、DST-11、DST-03 的第 2 部分（共用目錄 fd）、SRC-06（Source 端也用同一層目錄 handle）。
- 前提：與 ARC-02 一起做。PRO-00 直接寫在新結構裡，不要先在舊結構上做一次（也就是不要單獨做 WP-06a）。

### 現在的狀態與誰碰它

`State`（`src/destination.rs:72-96`）由一個 `Mutex` 保護，有 25 處上鎖：

| 欄位 | 寫入者 | 讀取者 |
|---|---|---|
| `ownership` | 主 thread（`Manifest::Names`，每次複製整份對照表，DST-16） | writers、finisher、套用 |
| `targets` | 主 thread（送出 needs 時加入） | parser（依 ID 取出） |
| `open`、`stack` | 主 thread（`announce`、`enter`、`leave`） | writers 與 parser（經 `settled` 呼叫 `file_settled`） |
| `finishing` | 主 thread、parser | finisher（接收端） |
| `warnings`、`failures`、`source_problems` | 各處 | 結束時 |
| `root_directory`、`reporter`、`delete` | 開始時 | 各處 |

問題：

- 這把鎖在路徑走訪（`open_under`，`:1835-1857`）、套用目錄、執行 `getent`（`:529-531`）時都被持有，16 個 writer 的路徑走訪因此互相排隊（DST-09）。
- 主 thread 同時負責讀 packet 和套用比較結果，所以比較完的目錄要等下一個 packet 到了才套用（PRO-07）。
- 每個檔案都由 `target_directory` 從根目錄重走一次路徑，開一個新的目錄 fd（DST-03、DST-09）。Source 的 `open_source_file`（`src/source.rs:795-806`）也是，而且每次都重新以路徑解析根目錄（SRC-06）。

### 提案：每份狀態只有一個擁有者

```text
receive ──manifest parts──▶ compare ─▶ stat pool ─▶ applier ──needs──▶ mux
   │                                                  │  ▲
   │                                (id, Target) ─────┘  │ Settled{dir}, Failed{id}
   │                                       ▼             │
   └──data──▶ decompress pool ─▶ feeder ─▶ parser ─▶ writers ─▶ fsync ─▶ link/rename
                                                                        │
                                              applier ◀── Settled ──────┘
                                              applier ──finish jobs──▶ finisher
```

1. **receive**：只讀 packet，然後分派：manifest 的 part 交給 compare，data 交給解壓縮 pool，Source 的 `Control::Error` 交給 `stop.peer_failed`。它不再套用任何東西，所以 PRO-07 自然消失。
2. **compare 與 stat pool**：與現在相同。
3. **applier**（新的 thread，也就是「目錄進度的管理者」）：
   - 擁有 `open`、`stack`、目錄收尾的判斷，以及 `ownership`。
   - 依 manifest 的順序套用比較結果：先刪除多餘的項目，再建立目錄、symlink、特殊檔，設定屬性。
   - 決定 needs。送出 `Needs::Files` **之前**，先把每個 `(id, Target)` 送進 parser 的 channel。
   - 從 writers 與改名 stage 接收 `Settled { directory }`、`Failed { id, message }` 事件，更新各目錄的計數，把可以收尾的目錄交給 finisher。
   - 用 `select!` 同時等比較結果、事件與停止（ARC-02）。
4. **parser**：擁有 `targets`，是自己的 `HashMap`。讀到 record 的 header 時，若 ID 不在 map 裡，就從 channel 取 `(id, Target)`，直到找到為止，可被停止打斷。

   因果保證：applier 先把 Target 放進 channel，才把 needs 交給 mux；Source 收到 needs 才送 record；record 經 receive、解壓縮、feeder 才到 parser。所以 parser 讀到 header 時，對應的 Target 一定已經在 channel 裡。

   若 channel 裡找不到，例如 Source 送了沒要求的 ID，channel 一路取到空就是協定違規，與現在的「the Source sent unneeded ID」相同。
5. **writers、fsync、改名**：照 ARC-01 的分工。結果以事件送回 applier，不再上鎖。
6. **計數與警告**：計數改用 `AtomicU64`。警告經 channel 交給一個收集者；或保留一把只有寫警告時才用的 `Mutex`，它幾乎不會有人搶。
7. **ownership**：applier 擁有它。更新時建立新的 `Arc<Ownership>`，交給 writers 使用。只複製新增的部分（persistent map），或改用 `RwLock` 以讀為主。這解決 DST-16。

### 提案：兩邊共用一層目錄 handle

- 新增 `src/dirs.rs`。`DirCache` 以相對路徑為鍵，存 `Weak<OwnedFd>`，或存 `Arc<OwnedFd>` 並以 LRU 限制大小。
- 所有 handle 都從**開始時開啟一次的根目錄 fd** 出發，以 `openat(parent, name, O_DIRECTORY|O_NOFOLLOW)` 逐層開啟，絕不再以路徑解析根目錄。
- 使用者：
  - Destination 的 applier、writers、finisher；
  - Source 的 readers。它們取代 `open_source_file` 每個檔案重走路徑的做法（SRC-06）。
- 大小上限納入 fd 預算（DST-03 的第 1 部分：啟動時調高 `RLIMIT_NOFILE`；之後依軟上限決定 cache 能用多少個 fd）。
- 不變式：與比較時開啟的目錄 fd 是同一個目錄，所以「刪除的對象，就是列出的那個目錄」（`d2d29ce`）在寫入與收尾時也成立。

### 模組的拆分

```text
src/destination/mod.rs      run：建立 Stages、channel，把各 stage 接起來
src/destination/receive.rs  讀 packet、分派
src/destination/apply.rs    applier：套用比較結果、目錄進度、needs、收尾的判斷
src/destination/write.rs    parser、writers
src/destination/durable.rs  fsync stage、link/rename stage、macOS 屏障
src/destination/finish.rs   finisher、make_durable
src/destination/warnings.rs Warning、Warnings
```

`compare.rs` 維持獨立。測試跟著各自的模組移動。

### 步驟（每步一個 commit，`make check` 與 `ferry test /tmp /tmp` 保持通過）

1. **純搬移**：把 `destination.rs` 拆成上面的模組，不改任何行為。這一步之後 diff 的審查會容易很多。
2. 接上 ARC-02 的 `Stages` 與 `Stop`，拿掉 `reading_failed`。這同時完成 PRO-00、PRO-04、PRO-05、PRO-08。
3. `targets` 改由 parser 擁有，applier 送出 `(id, Target)`。
4. 新增 applier thread 與事件，把 `open`、`stack`、收尾判斷從 `State` 移出。這同時完成 PRO-07。
5. 計數改用 atomic，警告集中處理，ownership 改成 Arc 快照（DST-16）。
6. 新增 `DirCache`，Destination 與 Source 都改用它（DST-03 的第 2 部分、DST-09、SRC-06）。
7. 拿掉 `State` 的 `Mutex`。

### 必須保住的行為

逐項對照 README 第 9 節：

- 一個目錄的多餘項目，在建立它的任何東西之前就刪除。
- 一個目錄要在三件事都成立後才收尾：manifest 已離開它與它底下的一切、其中需要的檔案都已 settled、其中的目錄都已完成。
- 由深到淺收尾。
- dry run 沒有 parser，也沒有 writer，只有 receive、compare、applier。
- `Done` 在所有目錄收尾、`make_durable` 之後才送出，而且絕不在 `Error` 之後。

### 要加的測試

- `apply.rs` 的單元測試：以合成的事件序列驅動 applier，例如「進入 a、需要 a/f、離開 a、`Settled{a}`」，斷言收尾的時機與順序。
- PRO-07 的回歸測試：比較完成後，needs 立即送出，不必等下一個 packet。
- 既有的 `destination.rs` 測試全部搬到新模組後，維持通過，不改斷言。

### 驗收條件

- [ ] `destination` 模組中沒有共用的 `Mutex<State>`。
- [ ] 路徑走訪與 `getent` 都不在任何共用鎖之內。
- [ ] Source 與 Destination 都不再每個檔案以路徑重新解析根目錄。
- [ ] PRO-00、PRO-07、DST-09、DST-16 的驗收條件全部達成。
- [ ] 20 萬個小檔與 4 GiB 檔的量測沒有變慢，最好變快。

### 文件要同步的地方

- `docs/design/pipeline.md`，與 `pipeline.zh-TW.md`：Destination 的 thread 與它們之間的 channel，以及「2. Compare」與「6. Write」中誰做什麼。
- 同時更新 `docs/assets/pipeline-workers.svg`。

---

## ARC-04：大檔案的 record 改成可以分段（看工作負載決定）

- 嚴重度：效能與功能，中。是否值得做，取決於「快速網路或本機 NVMe 上的大檔」是不是主要的工作負載。
- 依據：4 GiB 的檔案，Destination 唯一的 writer 忙了 97% 的時間，Source 的 file sender 讀了 2.2 s，每個檔案大約卡在 1.2 GB/s。`cp` 是 1.7 GB/s。
- 吸收的項目：SRC-11；也解決 DOC-07 提到的「中斷的大檔要從頭重送」；並為 sparse 與 delta 打底（README 第 10 節）。
- 需要使用者決定：**D16**（要不要做；要不要支援續傳，因為續傳需要有名稱的暫存檔，與 ARC-01 的沒有名稱的暫存檔衝突）。

### 現在

- 一個檔案是一個 record：header、全部內容、trailer（含 BLAKE3）。
- 大於 1 MiB 的檔案，由 Source 的 file sender 這一條 thread 依序讀取、雜湊，寫進 record 串流。
- Destination 由一個 writer 接收它的全部內容。
- 所以一個大檔的速度，上限是一條 thread 的「讀加雜湊」，以及一條 thread 的「寫加雜湊」。

### 提案

1. 大於門檻的檔案（例如 64 MiB），切成固定大小的段，例如 16 MiB 或 64 MiB，量測後決定。新增一種 record header，例如 `Header::Segment { id, index, offset, length, size, mtime }`，每段帶自己的 BLAKE3 trailer。
2. **Source**：
   - 開檔一次，以 `Arc<File>` 共用，只 fstat 一次，大小與 mtime 以這次為準。
   - 每段是 reader pool 的一個工作，用 `pread` 讀取，所以不再由 file sender 讀大檔（SRC-11）。
   - 任何一段讀不完整，就把整個檔案標為 unread，與現在的語意相同。
3. **Destination**：
   - 第一段到達時，建立暫存檔（ARC-01 的沒有名稱的檔案，或暫存名稱），並 `ftruncate` 到完整大小。
   - 每段由任一 writer 以 `pwrite` 寫到自己的 offset，並驗證自己的雜湊。
   - 每個檔案有一個計數，記錄已驗證的段數。全部到齊，就設定屬性、fsync，交給改名 stage。任何一段失敗，整個檔案失敗，暫存檔移除。
4. **完整性**：每一段各自驗證；再加上段的配置與 Source 宣告的大小一致，等同整個檔案驗證過。不需要合併 BLAKE3 的樹。
5. **串流順序**：不同檔案的段可以交錯，parser 本來就依 ID 分派。同一個檔案的不同段由不同的 writer 處理，所以可以平行寫入。
6. **記憶體**：上限由在途的段數決定，每個 writer channel 的容量也要限制。

### 延伸（各自另做，不在第一步）

- **續傳**：要能續傳，暫存檔必須有名稱，並在旁邊記下已驗證的段，以及 Source 的大小與 mtime，以確認是同一個版本。下次執行時只要求缺少的段。這與 ARC-01 的沒有名稱的檔案互斥，所以只對大檔使用有名稱的暫存檔（D16）。
- **sparse**：Source 以 `SEEK_DATA` 與 `SEEK_HOLE` 找出資料區段，只為資料區段產生段，洞由 Destination 的 `ftruncate` 自然形成。
- **粗粒度的 delta**：Destination 對已存在的同名檔案，先送出各段的雜湊；Source 對雜湊相同的段，只回「沿用」。這對就地修改的大檔有效，例如 VM 映像與資料庫檔案；對插入資料則無效（那需要 rsync 的滾動 checksum）。這是邁向 delta transfer 比較容易的第一步。

### 量測計畫（決定做不做之前）

- 用 `ferry test /tmp /tmp --scale 20G --kind large`（每個檔案 64 到 256 MiB）與 `--kind mixed`，在本機與一條快速網路上量測，記錄 Source 的 file sender 與 Destination 的 writer 的 busy 時間。
- 若 writer 或 file sender 是瓶頸，而且網路與磁碟都還有餘裕，就值得做。

### 驗收條件（若決定做）

- [ ] 單一 4 GiB 檔的本機複製，吞吐量隨 writer 數量上升，直到磁碟的上限。
- [ ] 段讀不完整、段的雜湊不符、中途中斷，都不會讓錯誤的內容就位。這三種情況都要有測試。
- [ ] 小檔的量測沒有變慢。

### 文件要同步的地方

- `docs/design/overview.md` 的「Terms」（record 的定義）與「Every file is complete, correct, and durable」第 2 點，與 `overview.zh-TW.md` 的對應段落。
- `docs/design/pipeline.md` 的「3. Read and hash」與「6. Write」，與 `pipeline.zh-TW.md` 的對應段落。

---

## ARC-05：小的結構調整

### 05a. 本機複製不壓縮，壓不下去的資料也不要一直試

- 依據：4 GiB 隨機資料的本機複製，8 個 compress thread 合計花了 4.38 s CPU，而每個 frame 最後都原樣送出。20 萬個隨機小檔則花了 1.9 s。
- 做法：
  1. 兩個 server 都在本機時（兩側都是 `Location::Local`），預設等同 `--no-compress`。`TODO.md` 已經列了「Whether a local copy compresses at all」。
  2. 連續 N 個 frame（例如 8 個）都壓不下去時，接下來 M 個 frame（例如 64 個）直接原樣送出，之後再試一次。
- 需要使用者決定：**D17**（本機複製是否預設不壓縮）。

### 05b. 修正 profile 的失真

- finisher 以 `for (path, entry) in done` 等 channel（`src/destination.rs:1745`），這段等待沒有包 `profile::wait`，所以 profile 顯示它 100% 忙。dashboard 依 profile 判斷瓶頸，可能因此指錯地方。
- `rename` 的 thread 每批重開，profile 顯示有 6,256 條。ARC-01b 之後會自然消失。
- 做法：
  - 所有會阻塞的 `recv`、`join`、`wait`，都要包在 `profile::wait` 中，並標成等待狀態。
  - 加一個測試：一次小規模的 copy 之後，profile 中沒有任何 role 在「完全閒置」時被標為 100% run。

### 05c. 共用主機上的 CPU 用量

- 依據：20 萬個小檔，`cp` 用了 4.7 s 的 CPU，Ferry 用了 96 s，大約 10 個核心。在 HPC 的 login node 這類共用主機上，這會影響其他使用者。
- 做法：加一個選項限制 thread 數，例如 `--threads N`，作用在各個 pool 的 `pool::threads(…)`。或者在偵測到 cgroup 的 CPU 配額時，依配額決定。ARC-01 做完之後，系統時間應該會大幅下降，所以先量測，再決定需不需要。
- 需要使用者決定：**D18**。

---

## 不建議改的地方

| 不改的部分 | 理由 |
|---|---|
| Rust 加同步 thread，不換成 async | 工作都是阻塞的檔案系統呼叫，async 帶不來好處。io_uring 以後可以只用在寫入 stage，先量測再說 |
| 使用系統的 `ssh` | `~/.ssh/config`、ProxyJump、ssh-agent、OTP、硬體金鑰都直接能用。換成 SSH library，就得逐一重做這些 |
| client relay 兩個 server 之間的資料 | 量測顯示它不是瓶頸：4 GiB 那次，relay thread 只忙了 0.07 s。remote-to-remote 時資料經過 client 兩次，是設計文件寫明的取捨：兩台主機不必互通，也不必在任一台放憑證 |
| client 與 server 必須是同一個 build | 協定因此可以自由修改，不必相容舊版。ARC-04 這種格式改動也因此便宜 |
| 一個 lister 加 8 個 stat | 這是在 NFS 上量測後的選擇。量測的檔案樹中，Source 端的 scan 與 stat 都不是瓶頸 |
| 安裝模型：上傳同一個 build、並存多個版本 | 合理。要補強的是驗證與順序（TRN-01、TRN-02、TRN-03），不是模型本身 |
| 拆成多 crate 的 workspace | 目前不值得。等 fuzz 的建置，或遠端 dev build 的編譯時間變成問題時再考慮 |

---

## 附錄 A：`prof.py`（彙整 `--profile-file` 的 JSON）

```python
import json, sys
d = json.load(open(sys.argv[1]))
print(f"elapsed {d['elapsed_nanos'] / 1e9:.2f}s")
for side in d['sides']:
    print('==', side['side'])
    for r in side['roles']:
        total = sum(s['nanos'] for s in r['states'])
        busy = [(s['state'], s['nanos']) for s in r['states'] if not s['wait']]
        waits = sorted([(s['state'], s['nanos']) for s in r['states'] if s['wait']],
                       key=lambda x: -x[1])[:2]
        b = sum(ns for _, ns in busy)
        print(f"  {r['role']:<28} x{r['threads']:<5} busy {b / 1e9:7.2f}s ({b / max(total, 1) * 100:4.0f}%)  "
              + ", ".join(f"{s}:{ns / 1e9:.2f}s" for s, ns in busy if ns > 5e7)
              + " | waits " + ", ".join(f"{s}:{ns / 1e9:.2f}s" for s, ns in waits))
```

用法：`python3 -I prof.py profile.json`。

## 附錄 B：`dirbench.py`（目錄鎖的微基準，Linux）

```python
import os, sys, time, threading, resource, shutil
root = sys.argv[1]; N = 40000; data = os.urandom(4096)

def run(threads, spread, tmpfile):
    shutil.rmtree(root, ignore_errors=True); os.makedirs(root)
    dirs = [os.path.join(root, 'd%02d' % i) for i in range(threads if spread else 1)]
    for d in dirs: os.makedirs(d)
    fds = [os.open(d, os.O_DIRECTORY) for d in dirs]
    per = N // threads
    def work(t):
        dfd = fds[t % len(fds)]
        for i in range(per):
            name = f'f{t}_{i}'
            if tmpfile:
                fd = os.open('.', os.O_TMPFILE | os.O_WRONLY, 0o600, dir_fd=dfd)
                os.write(fd, data)
                os.link(f'/proc/self/fd/{fd}', name, dst_dir_fd=dfd, follow_symlinks=True)
                os.close(fd)
            else:
                tmp = '.' + name + '.ferry-x'
                fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600, dir_fd=dfd)
                os.write(fd, data); os.close(fd)
                os.rename(tmp, name, src_dir_fd=dfd, dst_dir_fd=dfd)
    r0 = resource.getrusage(resource.RUSAGE_SELF); t0 = time.time()
    ts = [threading.Thread(target=work, args=(t,)) for t in range(threads)]
    [t.start() for t in ts]; [t.join() for t in ts]
    t1 = time.time(); r1 = resource.getrusage(resource.RUSAGE_SELF)
    print(f"threads={threads:2} {'spread' if spread else 'one dir'} "
          f"{'O_TMPFILE+linkat' if tmpfile else 'temp+rename'}: {t1 - t0:5.2f}s wall, "
          f"{r1.ru_stime - r0.ru_stime:6.2f}s sys, {N / (t1 - t0):7.0f} files/s")
    for fd in fds: os.close(fd)

for tmpfile in (False, True):
    for threads, spread in ((1, False), (16, False), (16, True)):
        run(threads, spread, tmpfile)
shutil.rmtree(root)
```

用法：`python3 -I dirbench.py <要測的檔案系統上的一個新目錄>`。

## 附錄 C：產生 20 萬個 4 KiB 檔案

```sh
python3 -I -c "
import os
for d in range(200):
    p = os.path.join('small', 'd%03d' % d); os.makedirs(p, exist_ok=True)
    for i in range(1000):
        with open(os.path.join(p, 'f%04d' % i), 'wb') as f: f.write(os.urandom(4096))
"
find small -type f -exec cat {} + > /dev/null   # 先讀進 page cache
```

這次量測的 profile JSON（`big.json`、`small.json`、`small-fsync.json`）放在 [`materials/arch/profiles/`](materials/arch/profiles/)，兩個腳本放在 [`materials/arch/`](materials/arch/)。
