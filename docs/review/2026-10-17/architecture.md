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

### 進度

> 2026-10-18 補記。這一節是實作時加上的，每完成一步就更新。D14 照建議決定；使用者決定先做 01b，再做 01a。

- **01b**：改名改由一個常駐的 stage 負責（`destination/durable.rs`），parser 把滿了的批次交給它，就繼續讀 record 串流。改名的結果經 channel 回到 parser，在那裡送 `Results`、更新計數、送出 `Settled`，所以一個檔案的結果仍在它改名之後才送。最多 2 批在途，`Header::End` 之後，parser 等它們都改完才結束。
  - **與建議不同：只有一個改名 thread，不是 `pool::threads(8)`。** 先照建議做了 8 個常駐的 renamer，含 fsync 的情境反而變慢：20 萬個小檔從 18.9–19.2 s 變成 20.8–21.1 s。writer 每個檔案的寫入加 fsync 從約 1.08 ms 增為 1.64 ms，每次改名從約 0.18 ms 增為 0.62 ms。我推測是改名和 fsync 同時進行，在 ext4 的 journal 上互相等待；舊版改名時 parser 停下來，writer 常閒著，兩者比較少重疊。以環境變數切換 renamer 的數量與在途批次數，量測如下，各 2 到 3 次：
    - ext4，含 fsync：舊版 18.9–19.4 s；8 個 20.8–21.1 s；8 個、一次只改一批 21.0–21.1 s；2 個 18.6–19.0 s；1 個 18.1–18.2 s。
    - ext4，`--no-fsync`：舊版 7.6–8.0 s；8 個 5.7–5.9 s；4 個 5.5–5.8 s；2 個 5.5–5.6 s；1 個 5.2–5.3 s。
    - NFS（fatcat，NFSv4.1，`nconnect=8`，從本機的 `/tmp` 複製 2 萬個 4 KiB 的檔案到 NFS 上）：`--no-fsync` 時，舊版 12.0–12.6 s，8 個 10.0–10.8 s，1 個 9.6–10.0 s；含 fsync 時，舊版 12.2–12.3 s，8 個 11.2–12.9 s，1 個 9.9–10.0 s。
    - 所以只用一個。這只在一台 LAN 上的 NFS 量過；延遲高的網路檔案系統上，一個改名 thread 可能成為瓶頸，那時再依 01c 以 `statfs` 區分。
  - 每個批次的屏障由改名 thread 在改名前設定，與以前相同。
  - fd 預算：正在填的一批，加上在途的兩批，同時最多三批的檔案在等，所以 `batch_size` 改為可用的 fd 除以 3。限制低時（例如 256），一批從 96 個降為 32 個。
  - profile 新增「wait for the renamer」（parser 等改名騰出空間）；「rename files」改為改名 thread 的工作。
  - 量測：與上一版（`05eea4b`）交錯各 3 次，見下表。驗收條件中「parser 不再等批次改名」只部分達成：parser 不再自己改名，以前 `rename files` 花 3.9 s，但 `--no-fsync` 時，改名 thread 幾乎一直在忙（4.9 s 的傳輸中忙了 4.85 s），parser 仍要等它騰出空間 1.3 s；含 fsync 時是 0.56 s。這要等 01a 減少目錄操作。「`rename` 的 thread 數固定」已達成，以前是每批新開，一次執行 6,256 條。
  - 測試：`durable.rs` 新增一個單元測試，把 10 個檔案分成每批 3 個交給改名 thread，每個檔案都改名、回報一次，沒有留下暫存名稱。

| 情境 | `05eea4b` | 01b |
|---|---|---|
| 20 萬個小檔，`--no-fsync` | 7.1–7.2 s，sys 69–72 s | 4.9–5.0 s，sys 63–65 s |
| 20 萬個小檔，含 fsync | 18.5–19.0 s，sys 37–38 s | 17.9–18.1 s，sys 29–30 s |
| 4 GiB，`--no-fsync` | 3.5–3.6 s | 3.4–3.5 s |

- **01b 的修正：照目錄分配給 4 個改名 thread**（2026-10-20 補記）。上面「只用一個」的結論，在高延遲的檔案系統上不成立，改為照目錄分配：一批進來後先按目錄分組，最多 `pool::threads(4)` 個常駐的改名 thread；第一個設定屏障並改名檔案最多的目錄，其他的各拿一個其餘的目錄。同一個目錄同一時間只由一個 thread 改名，所以一批只在一個目錄裡時，等於 01b 的單一 renamer。barrier 仍是每批一次，在任何改名之前；批次仍一次一批，fd 預算不變。
  - **為什麼 ext4 上多個 renamer 會輸**：本機的量測資料是 200 個目錄、每個 1,000 個檔案，一批 256 個幾乎都在同一個目錄。把一批切給多個 thread，等於它們搶同一個目錄的鎖：Linux 在一個目錄裡改名時，VFS 獨占地鎖住那個目錄（`i_rwsem`），所以同目錄的改名本來就一個接一個，與檔案系統無關。實驗版把每批對半切給 N 條 thread（`EXP_RENAMERS`），本機 ext4，20 萬個 4 KiB 檔案，各 3 次：
    - 每目錄 1,000 個：1 條 `--no-fsync` 4.50–4.77 s，2 條 5.38–5.61 s，4 條 5.41–5.56 s；含 fsync 1 條 16.2–17.5 s，2 條 18.5–19.0 s，4 條 20.2–20.6 s。改名本身變慢，renamer 合計的改名時間從 4.58 s 增為 5.39 s。
    - 每目錄 100 個：`--no-fsync` 1 條 3.03–3.11 s，2 條 2.98–3.03 s，4 條 3.03–3.12 s；含 fsync 11.8–12.6、12.2–12.5、12.3–12.8 s。改名本身變快（2.56 → 1.92 → 1.67 s），但這組資料的 renamer 不是瓶頸，總時間持平。
  - **高延遲的檔案系統上，一個 renamer 是瓶頸**：nano4 的 WekaFS（`/work`，從計算節點的 `/dev/shm` 複製 2 萬個 4 KiB 檔案，200 個目錄、每個 100 個，各 3 次）：舊版 `05eea4b` 19.4–20.3 s；01b 17.6–18.7 s；實驗版 2 條 12.3–13.7 s，8 條 12.5–13.5 s，16 條 12.6–13.8 s；含 fsync 時相同。單一 renamer 每秒約 1,100–1,200 次改名，每次約 0.85 ms，是一趟 metadata 往返。fatcat 的 NFS（200 個目錄）上，profile 量到 1 條每秒約 3,200 次、2 條與 4 條約 6,100 次，但總時間只快 3–7%（6.4–6.6 s → 6.1–6.2 s），因為 16 個 writer 也已經忙滿；全部在一個目錄時，1、2、4 條都是每秒約 1,550 次，12.7–13.1 s，證實同目錄的改名無法並行。
  - **最後的量測**（照目錄分配對 01b，交錯）：
    - WekaFS，每目錄 100 個：`--no-fsync` 16.8–22.5 s → 12.6–16.7 s，含 fsync 16.9–21.9 s → 12.5–16.1 s，約快 26%；parser 等 renamer 從約 6 s 降為 0.1 s。第 3 輪叢集負載變重，所有版本都慢了約 5 s，所以範圍較寬。
    - WekaFS，每目錄 1,000 個：兩版相同（23.6–35.2 s），parser 仍等 renamer 約 13 s：一批只在一個目錄，無從並行，而且 renamer 與在同一目錄建暫存檔的 writer 搶鎖，每次改名約 1.4 ms。
    - ext4，每目錄 100 個：`--no-fsync` 2.70–2.72 s → 2.50–2.67 s，含 fsync 12.50–12.77 s → 12.52–12.71 s。
    - ext4，每目錄 1,000 個，5 輪：`--no-fsync` 4.01–4.28 s（平均 4.14）→ 4.29–4.57 s（平均 4.46），**慢約 7%**；含 fsync 16.66–17.48 s → 17.16–18.04 s，慢約 2%。
    - fatcat NFS：這一輪雜訊太大（同一版本相差 1.5–2.5 s），看不出差別；單一目錄兩版相同。
  - **剩下的損失，原因未證實**：每 4 批約有 1 批跨兩個目錄，第二個目錄交給另一個 thread 時，writer 很可能還在那個目錄建檔，兩者搶鎖。即使只有一個 renamer，大目錄裡 renamer 也與 writer 搶鎖：ext4 上每目錄 1,000 個時，改名每秒約 4.4 萬次，100 個時約 7.8 萬次；含 fsync 時 1.2 萬對 4.8 萬。01a 讓 writer 在 ext4 上不必鎖目錄，之後要重量。
  - **試過並拿掉：「落後才並行」**。只在下一批已在排隊時才把其他目錄交出去。含 fsync 時 renamer 跟得上，損失從 2% 降到約 1%；`--no-fsync` 時 renamer 落後，照樣並行，仍慢 7%。它只是「沒好處就別做」的保護，處理不了 renamer 與 writer 搶鎖，使用者決定拿掉。
  - 不需要 01c 的 `statfs` 判斷：並行數由一批跨幾個目錄決定，不依檔案系統猜。上限 4，因為 2、4、8、16 條在 WekaFS 和 NFS 上一樣快。
  - profile 新增「wait for the other renamers」：第一個改名 thread 改完自己的目錄後，等其他的。
  - 測試：`durable.rs` 的單元測試改為 30 個檔案輪流放在 3 個目錄，每批 5 個，所以每批都跨目錄，交給其他 thread 的路徑一定會跑到；每個檔案都在自己的目錄改名、回報一次，沒有留下暫存名稱。
- **01a**（2026-10-20 補記）：Linux 上 writer 先以 `fsys::create_unnamed` 建立沒有名稱的檔案（`O_TMPFILE`），寫入、驗證、設定屬性、fsync 都對 fd 操作；改名 stage 依 `Target.exists` 決定：比較時目錄裡沒有這個名稱，就以 `fsys::link_unnamed`（`linkat` 經 `/proc/self/fd`，`AT_SYMLINK_FOLLOW`）直接給它名稱，一次目錄操作；有這個名稱，或 link 得到 `EEXIST`，就以 `fsys::link_temporary` link 到暫存名稱（向 `interrupt` 登記），再 `clear_the_way` 並改名蓋過去。檔案系統回傳 `EOPNOTSUPP`、`EISDIR` 或 `EINVAL` 時，以 `st_dev` 記住，之後同一個 device 直接走舊路徑；沒有被拒絕過的 device 時，不必 `fstat` 目錄。`exists` 取自比較時的目錄列表（`Prepared.present`，經 `Compared.present` 傳到 `apply_directory`）；根目錄本身是檔案時一律當作存在。
  - **與建議不同之處**：
    - 沒有 `/proc` 時，建議是在 link 得到 `ENOENT` 時改走舊路徑、丟掉檔案。但到了 link 時，資料已經讀完、無法重寫，所以改為建立時就檢查 `/proc/self/fd`（每個行程一次），沒有就直接走舊路徑。
    - 目的地原本沒有這個名稱時，link 之前不呼叫 `clear_the_way`，也就不 `stat`：沒有名稱就沒有東西要清；比較之後才出現的東西，會讓 `linkat` 得到 `EEXIST`，改走會清除的舊路徑。這符合「出錯時才處理」的原則。
    - fd 預算：沒有名稱的檔案在等待批次時要保持開啟，每個檔案最多占自己和目錄兩個 fd，所以 Linux 上 `batch_size` 再除以 2（限制 1024 時一批從 256 個降為 144 個；256 時從 32 降為 16）。建議沒有提到這一點。
    - 強制走舊路徑的開關用 `FERRY_TEST_FAIL=unnamed`（只在 debug build 有效）。環境變數每個行程只讀一次，所以「強制舊路徑結果相同」的測試放在 `tests/copy.rs`，不在 `destination` 的單元測試。
    - 既有的 Ctrl-C 測試（`an_interrupted_copy_leaves_no_temporary_files`）是在等暫存名稱出現，改為以 `FERRY_TEST_FAIL=unnamed` 走舊路徑，因為它驗證的正是舊路徑上的清除。
  - **測試環境**：這台機器的 `/tmp` 是 container 的 overlayfs，不支援 `O_TMPFILE`（`EOPNOTSUPP`），所以預設的測試都走舊路徑。新路徑的測試改在 `target/` 底下（ext4）建暫存目錄（`test_support::unnamed_tempdir`、`tests/copy.rs` 的 `unnamed_tempdir`），找不到支援的目錄時說明並跳過。完整的測試另外以 `TMPDIR=target/tmpdir` 跑一次，兩次都 327 個全過。
  - **新增的測試**：`fsys`：建立加 link 內容正確、權限為 0600、link 到已存在的名稱得到 `EEXIST`、`link_temporary` 給出 `.f.ferry-` 名稱；`durable`：沒有名稱的檔案在三種情況都取得名稱（原本沒有、比較時已有、比較後才出現），批次測試一半走舊路徑；`tests/copy.rs`：`kill -9` 正在寫檔案的 Destination server 後不留下 `.ferry-` 名稱（以 `/proc/<pid>/fd` 找出開著「(deleted)」檔案的 server），沒有名稱與強制舊路徑兩種情況的複製與取代。手動對照：同樣 `kill -9`，舊路徑留下 `.big.ferry-22ddd10400000000`，新路徑什麼都不留。
  - **NFS**：fatcat 的 NFSv4.1 回傳 `EOPNOTSUPP`，自動走舊路徑；2 萬個檔案複製 2 輪都正確、沒有暫存名稱，改一個檔案後再複製內容一致；時間與 `8c36526` 在雜訊之內（6.5–8.4 s）。WekaFS 與 macOS 尚未驗證，macOS 交給 CI。
  - **量測**（本機 ext4，20 萬個 4 KiB 檔案，與 `8c36526` 交錯各 3 次）：

| 情境 | `8c36526` | 01a |
|---|---|---|
| 每目錄 1,000 個，`--no-fsync` | 3.91–4.22 s，sys 48.7–51.3 s | **2.71–2.87 s**，sys 15.7–25.5 s |
| 每目錄 1,000 個，含 fsync | 16.74–17.30 s，sys 26.1–27.9 s | **12.30–12.40 s**，sys 28.1–29.8 s |
| 每目錄 100 個，`--no-fsync` | 2.51–2.65 s，sys 42.5–45.2 s | **1.75–2.00 s**，sys 14.5–18.9 s |
| 每目錄 100 個，含 fsync | 12.75–12.84 s | 11.71–12.06 s |
| 4 GiB，`--no-fsync` | 3.37–3.50 s | 3.34–3.48 s |

  - writer 合計的寫入時間：每目錄 1,000 個、`--no-fsync` 從 46.1 s 降為 14.6 s，含 fsync 從 269 s 降為 195 s：writer 不再和 renamer 搶目錄鎖。
  - **驗收條件**：
    - sys time 至少減半：平均 50.1 s → 20.0 s；逐輪 51.3 → 15.7、48.7 → 25.5、50.4 → 18.7，第 2 輪只降到 52%。wall time 變快。
    - 含 fsync 的 wall time 明顯下降：約 −27%。
    - `rename` 的 thread 數固定：01b 已達成。
    - parser 不再等批次改名：只部分達成。每目錄 1,000 個、`--no-fsync` 時，parser 等 renamer 從 1.14 s 降為 0.71 s，沒有趨近 0；含 fsync 時為 0。
    - Linux 上 `kill -9` 不留暫存檔：已達成，有測試。
    - macOS 與 NFS 走舊路徑、所有測試通過：NFS 已驗證；macOS 待 CI。
  - **01a 之後，renamer 1 個對 4 個**（本機 ext4，交錯 4 次）：每目錄 100 個、`--no-fsync` 時 4 個 1.64–1.96 s、1 個 2.15–2.49 s，快約 20%；每目錄 1,000 個、`--no-fsync` 時 4 個 2.77–3.15 s、1 個 2.22–3.27 s，範圍重疊，看不出差別；含 fsync 兩者相同。01b 之後 ext4 大目錄上「慢 2–7%」的損失，在這組量測裡已經分辨不出來。
- **01d**（2026-10-20 補記）：writer 不再自己 fsync，寫完、驗證、設定屬性後，把檔案交給專門 fsync 的 thread（stage「fsync」），最多 64 個在等，自己去寫下一個；fsync 完成後，由 fsync thread 把結果交給 parser。`--no-fsync` 時沒有這些 thread，writer 直接回報。暫存名稱的檔案為了 fsync 留在 `Ready.file`，fsync 後只在需要屏障的平台（macOS）保留；沒有名稱的檔案本來就開著。fsync 失敗時丟掉 `Ready`，暫存名稱由登記移除，沒有名稱的檔案隨 fd 關閉消失。profile 新增「fsync files」與「wait for the fsync threads」。
  - **thread 數**：使用者決定固定 64，不加 CLI 旗標（2026-10-20）。審查建議 32 到 64、可以設定。fd 預算：每個 fsync thread 與每個等待中的檔案各占一個 fd，所以 thread 數為 `(可用 fd − KEPT_OPEN) / 4`，介於 1 到 64；批次的大小再扣掉這 `2 × thread 數` 個 fd。限制 256 時是 24 條 thread、每批 8 個檔案。
  - **先量測**（實驗版以 `EXP_FSYNC=N` 切換，20 萬個 4 KiB 檔案、每目錄 1,000 個、含 fsync，各 3 次）：
    - 本機 96 核心：0（writer 自己 fsync）12.95–14.58 s，sys 28–31 s；16 條 13.0 s；32 條 9.5–9.7 s；64 條 7.5–9.7 s，sys 55–64 s；128 條 6.8–8.8 s，sys 71–79 s；256 條 7.2–7.3 s，sys 83–94 s。
    - 本機限制在 8 核心（`taskset -c 0-7`）：0 為 16.97–17.30 s；16 條 11.5–11.9 s；32 條 8.2–9.5 s；64 條 6.3–7.1 s；128 條 5.8–6.2 s；sys 一直是 17–19 s。所以 96 核心時多出的 sys，應該是大量 thread 在 kernel 裡等 journal 時空轉，不是多做了工作；這是推測。
    - fatcat NFS（2 萬個檔案，200 個目錄）：0、16、32、64 條都是 7.8–10.3 s，在雜訊之內。
    - nano4 WekaFS：照使用者的指示在 login node（`25a-lgn02`，216 核心）上直接量，不用 srun，因為實際上就是在 login node 上同步檔案。2 萬個檔案，0、16、64 條都約 6.2 s，沒有差別。之前 01b 與 renamer 的 WekaFS 數字是在計算節點 `25a-hgpn016` 上量的，不能直接比較。
  - **最後的量測**（與 `3a6402a` 交錯各 3 次，20 萬個 4 KiB 檔案）：

| 情境 | `3a6402a` | 01d |
|---|---|---|
| 8 核心，每目錄 1,000 個，含 fsync | 16.65–17.05 s，sys 16–18 s | **6.37–7.32 s**，sys 18 s |
| 8 核心，每目錄 100 個，含 fsync | 15.79–15.99 s | **5.44–6.07 s** |
| 96 核心，每目錄 1,000 個，含 fsync | 12.21–13.84 s，sys 27–30 s | **7.74–9.01 s**，sys 56–64 s |
| 96 核心，每目錄 100 個，含 fsync | 11.78–13.96 s | **7.46–9.56 s** |
| 不 fsync（兩種核心數、兩種目錄） | 1.65–2.91 s | 1.67–2.87 s |

  - 不 fsync 時，8 核心、每目錄 1,000 個那組第一次量到新版慢約 20%（2.04–2.28 對 2.47–2.70 s）；單獨交錯重跑 5 次是 2.07–2.46 對 2.04–2.16 s，兩版相同，前一次是機器負載（load 12–17）造成的。
  - **代價**：96 核心時，含 fsync 的 sys time 約加倍（28 s → 56–64 s）；8 核心時沒有增加。
  - 64 條時瓶頸仍在 fsync：writer 等 fsync 騰出空間合計 14 s（8 核心）到 82 s（96 核心）。
  - 測試：`the_fsync_threads_fit_the_open_file_limit`，以及 `a_batch_fits_the_open_file_limit` 加上 fsync 占用的 fd；既有的低 fd 限制整合測試（限制 256）照常通過。

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

### 進度（WP-ARC，ARC-02 與 ARC-03）

> 2026-10-18 補記。審查本身寫於 `3c17a2f`，這一節是之後實作時加上的，每完成一步就更新。

使用者同意的順序，是把 ARC-02 與 ARC-03 的步驟依相依關係交錯，每步一個 commit：

| # | 內容 | 解決的項目 | commit |
|---|---|---|---|
| 0 | 量基準：20 萬個 4 KiB 小檔（有無 fsync）與一個 4 GiB 檔，release、本機對本機、page cache 中，各 3 次 | — | 不 commit（`600be6b` 上量） |
| 1 | 加入 crossbeam-channel（D15） | — | `19a7fff` |
| 2 | `pool.rs` 改用 crossbeam，拿掉 `Arc<Mutex<Receiver>>` | — | `9d4ec7c` |
| 3 | `stop.rs`、`stage.rs` 與單元測試 | — | `5591d7d` |
| 4 | 只搬移：`destination.rs` 拆成 `destination/` | — | `664c10f` |
| 5 | Destination 接上 `Stages` 與 `Stop`；manifest 結束前的 EOF；放寬的權限還原 | PRO-00、PRO-04、PRO-05、PRO-08 第 1、2 部分、DST-11 | `f7fffd6`、`6fa1c49`、`663b04f` |
| 6 | `targets` 改由 parser 擁有；串流結束時對帳 | PRO-06 | `6ee0825` |
| 7a | apply thread：比較結果一出來就套用；比較用的 pool 改回有界 | PRO-07 | `a3eeda8` |
| 7b | applier 擁有 `open`、`stack` 與收尾判斷；writers 與 parser 以事件通知 | — | `09ec97c` |
| 8 | 計數改用 atomic、警告集中、ownership 就地學習 | DST-16、DST-17 | `14f2c80` |
| 9 | `DirCache` | DST-03 第 2 部分、DST-09、SRC-06 | `a5e3f07`、`9134745`、`dffce72` |
| 10 | 拿掉 `State` 的 `Mutex`，量測 | — | `c57a35b` |
| 11 | pool 的 worker 改成 stage；Source 改用 `Stages` 與 `Stop` | SRC-02、SRC-03 的 pool 與 file sender 部分 | `4888c7f`、`3049b49`、`083104c`（修正 11b 一個偶發失敗的測試） |
| 12 | relay 改用 `Stages` 與 `Stop` | PRO-14、TRN-18、PRO-08 第 3、4 部分 | `c4c5c0a`、`de11e26` |
| 13 | 其餘的 `mpsc`，量測 | — | `49db59f` |

之後是 WP-06c（panic 政策，D4），也在 main 上，見下一列的 commit。它補上了下面延後的三個測試：stage panic 的整合測試、壓縮失敗（SRC-03 的 `frame-encode`），以及傳輸途中切換根目錄的符號連結（SRC-06）。最後這個不需要注入點：測試扮演 Destination，等 manifest 送完、換掉符號連結之後，才要求那個檔案。

**與本文不同之處，以及延後的項目由哪一步接手：**

- 第 3 步：`Stages::spawn` 回傳 `Stage<T>`，它的 `join()` 給 `Option<T>`，因為部分 stage 要交回結果，而驗收條件不許 `stage.rs` 以外出現 `join().expect(`。`Stop::done()` 在已停止時回傳 `Err(Stopped)`。`Stop` 另有 `failing()` 與 `consequence()`：前者在放開別人等待的東西之前先記下失敗，後者把停止後碰到的錯誤當成後果。
- 第 1 步：cargo-deny 還沒設定（CI-02），沒有允許清單可以登記。
- 第 5 步：
  - **PRO-08 的第 3、4 部分移到第 12 步**：server 在 connection 失敗而 peer 沒發現時仍算失敗，以及 relay 把轉送失敗記為備用的原因。兩者都是跨 connection 的失敗回報。
  - **pool 的 worker（解壓縮、stat，以及 Source 的壓縮）在第 11 步改成 stage**，因為 pool 由兩個 peer 共用：屆時 `pool::ordered` 接受 `&Stages`。在那之前，pool worker 的 panic 仍會停滯。
  - **mux 的 `closed()` 與 pool 的 `submit` 現在回傳 `Stopped`。** Source 也用到它們，所以在第 11 步之前，Source 的訊息會是「stopped, as another part of this side failed」，不是原本的「the connection's writer stopped」或「the workers stopped」。
  - **`peer.rs` 的 `outward.join().expect` 在第 12 或 13 步處理。**
  - **不做：PRO-00 的邊界情況 (b)。** 停止時，已寫完、驗證過、等待批次改名的檔案，仍像以前一樣被刪除，不先改名就位。本文只是建議。
  - **不做：`report_transferring` 維持一般的 `scope.spawn`。** 它只負責回報，不是 pipeline 的一段。
- 第 6 步：
  - 主 thread 只留每個需要的檔案所在的目錄（`needed`），用來計算目錄何時收尾，所以 Source 回報打不開的檔案時，仍能立刻算作處理完；parser 擁有完整的 `Target`。目錄路徑改用 `Arc<[u8]>`，同一個目錄的檔案共用一份。
  - Source 回報打不開、卻又送來 record 的檔案，以前會被當成「unneeded ID」拒絕，現在會照常寫入並檢查雜湊。只有 Source 有 bug 時才會這樣。
- 第 7a 步：
  - **parser 找不到 `Target` 時會等待**，直到 apply thread 送來，或它結束時關閉 channel（這時才判定為「unneeded ID」），或停止為止。ARC-03 寫的是「一路取到空就是協定違規」，在真實的 Source 下成立；但測試裡的假 Source 會不等 needs 就送 record，等待讓兩者都正確。代價是 Source 送來沒要求的 ID 時，要等 manifest 套用完才會發現。因此 PRO-04 的情境 D 改用無法解碼的 record header 觸發 parser 失敗。
  - **放寬權限改在 compare thread 放寬的當下記錄**，不在套用時。否則在套用之前停止，那次放寬就不會還原。這是 7a 之前就有的空隙，7a 讓它更容易發生。
  - `a_manifest_with_a_directory_sent_twice_is_refused` 的第一個 `inside` part 改為與目的地一致。原本它是合法且完整的 part，同步模式下會刪掉 `inside/keep`；以前測試只是靠主 thread 通常先讀到後面那個無效的 part 才過。設計承諾的是每則訊息在採取行動之前先檢查，沒有承諾後面的訊息無效時，前面的也一併作廢。
  - 拿掉了 `pool::ordered_unbounded` 與 `Collector::try_next`，已經沒有人用。
  - 量測：20 萬個小檔的峰值 RSS 與基準相同（基準 434–440 MiB，7a 為 412–434 MiB）。
  - `pipeline-workers.svg` 與 `pipeline-scan.svg` 留到第 10 步，等 Destination 的 thread 結構定下來再重畫。
- 第 7b 步：
  - applier 擁有 `Directories`：`open`、`stack`、每個需要的檔案所在的目錄、送往 finisher 與送往 parser 的 channel。其他 stage 以 `Event` 通知它：主 thread 送 `Root` 與 `Unread`（Source 回報打不開），parser 送 `Settled` 與 `StreamEnded`。ARC-03 寫的是 `Settled { directory }`、`Failed { id, message }`；這裡事件只帶 ID，目錄由 applier 依 ID 查，檔案的失敗仍由 parser 回報（它有訊息），applier 只需要知道它處理完了。
  - **`Manifest::Root` 改由 applier 套用**，不在主 thread。applier 每次取比較結果之前，先處理已收到的事件，所以根目錄一定先於任何 part 套用。PRO-07 原本讓主 thread 照舊同步套用根目錄；但根目錄的進度也屬於 `Directories`，留在主 thread 就得共用。
  - **多了一個轉送用的 stage（role 為 `compared`）**：pool 的 `Collector` 是把結果依序排好的 iterator，不能放進 `select!`，所以由它依序轉進一個 crossbeam channel。
  - **串流結束時的對帳移到 applier**：parser 在 `StreamEnded` 附上還沒來的檔案，由 applier 判斷哪些是 Source 回報過打不開的，其餘以「never sent」失敗。
  - **最後的收尾也由 applier 負責**：處理完 `StreamEnded`，它把剩下的目錄由深到淺交給 finisher，再關掉 channel；parser 只等 finisher 結束。
  - **`frames` 改為移進主 thread 的讀取迴圈，迴圈一結束就放掉。** 原本要等 `read_packets` 回傳才放掉，現在會死結：`read_packets` 要等 applier，applier 等 parser 的 `StreamEnded`，parser 又等串流因 `frames` 放掉而結束。
  - 兩個目錄進度的單元測試搬到 `apply.rs`，改以 `Directories` 驅動，照 ARC-03 的要求。
- 第 8 步：
  - **ownership 不用 `Arc` 快照，改在內部用 `RwLock`**，這是 ARC-03 列的另一個做法：學新名稱時，先在鎖外查好本機 ID（`getent` 可能很慢），再短暫拿寫入鎖就地加入，完全不複製；每次查詢只在那一次拿讀取鎖。行為上的差別：比較中的 part 也看得到之後才學到的名稱。名稱只會增加，同一個 uid 的對應不會變，所以結果相同。DST-16 要求的「一萬則 `Names` 時間大致線性」改以單元測試驗證：持有 `Ownership` 的人看得到之後加入的對應，也就是沒有複製。這個測試沒辦法拿到舊程式上跑，因為舊的 `Ownership` 沒有就地加入的介面。
  - **警告改用 `State` 裡一把自己的 `Mutex`**，沒有另設收集者的 channel。寫警告的機會很少，幾乎不會有人搶。
  - **計數（`failures`、`source_problems`）改用 `AtomicU64`。** 在第 10 步拿掉 `State` 的 `Mutex` 之前，它們仍在那把鎖底下。
  - DST-17：以 ID 減去該段第一個 ID 直接取索引，並確認 ID 相符。
- 第 9 步，分三個 commit：Destination 的目錄（9a）、等待改名的檔案 fd（9b）、Source 的 readers（9c）。
  - **`DirCache` 存 `Arc<OwnedFd>`，以 LRU 保留最近用過的 32 個**，不是 DST-03 建議的 `Weak`。Source 的 readers 讀完就關檔，沒有人會一直持有目錄，用 `Weak` 的話幾乎永遠命中不了；SRC-06 本身也建議容量 32 的 LRU。Destination 這邊，等待批次的檔案本來就持有同一個 `Arc`。快取保留的 32 個 fd 算進批次預算的保留量（`KEPT_OPEN` 加上 `dirs::KEPT`）。
  - **找不到時從已快取的最長祖先往下開**，每一層用 `openat(O_DIRECTORY|O_NOFOLLOW)`；開檔時不持有快取的鎖。
  - **Destination 的根目錄**：比較時開啟的那個交給快取（`hold_root`），之後都從它出發。比較時根目錄還不存在（之後由 `apply_root` 建立）的話，第一次用到時才以路徑開一次；以前是每次都以路徑重新開。
  - **等待改名的檔案**：只有 macOS 開了 fsync 才保留檔案 fd，因為只有那裡的屏障需要；一批裡每台磁碟一個。所以每個等待的檔案最多占一個 fd，批次預算不再除以 2。
  - **Source**：根目錄由掃描與 readers 共用（掃描改從快取取得根目錄），readers 從快取開檔案所在的目錄，不再每個檔案都以路徑解析根目錄。
  - **沒做：SRC-06 要的「傳輸途中切換根目錄的符號連結」測試。** 它需要在讀完第一個檔案後停住的注入點，等 WP-06c 的 `FERRY_TEST_FAIL` 做好再補。已補上「根目錄是符號連結時照常複製」。
  - **沒做：Destination 的比較（`compare::Tree`）仍自己開目錄、每層保留一個 fd**，沒有改用 `DirCache`。它的 fd 用法跟寫入不同：要依深度優先保留整條祖先鏈。
  - DST-09：applier 在 `apply_directory` 期間仍持有 `State` 的鎖，這是已知的例外，第 10 步拿掉那把鎖時就會消失。
- 第 10 步：
  - `State` 不再有 `Mutex`：`reporter` 在建立 `State` 之前先建好；`refused` 改由 `apply_directory` 直接回傳，因為只有 applier 用到它；`let_in` 改用自己的 `Mutex`，compare thread、finisher 與還原權限都會用到它。其餘欄位不是不可變，就是在前幾步已改為 atomic 或各自的鎖。
  - ARC-03 的驗收條件：Destination 沒有共用的 `Mutex<State>`；路徑走訪與 `getent` 都不在共用鎖內（DST-09 在第 9 步記下的例外也隨之消失）；readers 與 writers 都不再每個檔案以路徑解析根目錄。
  - `pipeline-scan.svg` 加上 applier：比較結果送到它，由它送出 needs；主 thread 把根目錄交給它。圖只核對了座標，這台機器沒有轉圖的工具，沒有實際看過畫出來的樣子。`pipeline-workers.svg` 畫的是資料路徑（主 thread 接收、解壓縮、feeder、parser、writers、renamers），仍然正確，所以沒改。
- 第 11 步，分兩個 commit：
  - 11a：`pool::ordered` 與 `pool::gauged` 改接 `&Stages`，每個 worker 都是 stage；`Collector` 同時等結果與停止。worker panic 時，整個 peer 停下，原因是「internal error in <role>」，SRC-03 的 `poolpanic` 不再卡住。因為比較結果與掃描的 stat 結果都可能因停止而提早結束，applier 在送 `Needs::End` 之前、掃描在送 `Manifest::End` 之前，都會先檢查停止。
  - 11b：Source 的所有 thread 都在同一個 `Stop` 之下：pump、manifest、lister、stat、壓縮、frame sender、file sender、readers。
    - **SRC-02 不另設 `Halted` 旗標，改用 Source 自己的 `Stop`**：`ManifestSink` 在送出每則 manifest 與 problem 之前檢查；lister 在往下開目錄之前檢查；`write_record` 每讀一塊檢查；`Backlog` 在 `receive` 回傳後關閉，等待中的掃描隨即醒來，看到停止就結束。錯誤的優先順序交給 `stop.result()`：第一個不是 `Stopped` 的就是原因。
    - **拿掉 `Job::Stop`**：以前 file sender 收到它就照常寫入 `Header::End`，等於把失敗的串流當成完整的收尾。現在 file sender 同時等工作與停止，停止時回傳 `Stopped`，不寫串流的結尾。
    - **scanner 不再自己送 `Control::Error`**，由 `Stop` 送且只送一次；以前它送一次，`serve` 又送一次。
    - **readers 的 channel 換成 crossbeam**，拿掉 Source 最後一處 `Arc<Mutex<Receiver>>`。
    - 量測：`repro-scan-after-failure.sh`，Source 的 `stat entries` 從 1,001 降到 11，`list directories` 從 2,001 降到 23，失敗訊息仍是 Destination 的原因。
    - **沒做：SRC-03 要的 `FERRY_TEST_FAIL=frame-encode` 整合測試**，等 WP-06c 加注入點時補上。file sender 的失敗，現在與其他 stage 一樣由 `Stop` 告知 Destination。
    - lister 自己的停止檢查，測試分辨不出來：pool 的 `Collector` 一停止就不再交出結果，lister 下一次 submit 也會失敗。保留它，是照 SRC-02 的要求，最多省下一次目錄列表。
- 第 12 步，分兩個 commit：
  - 12a（TRN-18）：`Paths::wait` 最多等 10 秒，socket 用讀取逾時，子行程用 `try_wait` 輪詢，時間到就 kill，並以「a path did not end」失敗。採 TRN-18 列的第一個做法；第二個做法（等子行程都結束後才結束 heartbeat）沒做。另外照 TRN-11 的第 5 步，回聲測試會印出走到哪一步，回聲 server 的錯誤改為印出，不在沒人 join 的 thread 裡 `unwrap`；加上時限之後，TRN-11 的假設 3（EOF 一直不來）也會變成一個錯誤，不再卡住。
  - `c4c5c0a` 是在 `make check` 有一個測試失敗時 commit 的：11b 新加的 `a_scan_that_fails_says_so_once` 會偶發失敗（30 次中 7 次），原因是測試的 reader 立刻回傳 EOF，讓「connection 關閉」有時比掃描的失敗先被記下。`083104c` 改成 Destination 看到 `Control::Error` 才關閉，與真實的順序一致，之後連跑 40 次都通過。這是我的疏忽，之後每次都確認檢查通過才 commit。
  - 12b：relay 有自己的 `Stop`：兩個方向都是 stage，任一 server 送來的第一個錯誤就是傳輸的失敗。
    - PRO-14 採建議的做法：無法解碼的 report 改為顯示警告，不讓傳輸失敗。
    - PRO-08 第 4 部分：轉送給另一個 server 失敗時記下，只在沒有其他失敗、兩個 server 也都正常結束時，才以它失敗。
    - PRO-08 第 3 部分：server 的 inward 曾經失敗、peer 卻沒有送 `Control::Error` 時，outward 在 peer 結束後把 connection 的錯誤告訴 client，server 也以它失敗。
    - `peer.rs`、`cli.rs`（儀表板）、`transport/striped.rs` 的 `join().expect` 改成以錯誤回報。
    - **沒加測試**：PRO-14 與 PRO-08 第 3、4 部分，審查都沒有要求測試，或列為可選；要測，需要建出完整的 relay 與一對真的 connection，或者需要 WP-06c 的注入點。
- 第 13 步：std 的 `mpsc` 全部換成 crossbeam-channel（`channel` 換成 `unbounded`，`sync_channel(n)` 換成 `bounded(n)`，`SyncSender` 換成 `Sender`），共 19 個檔案，行為不變。testkit 的兩處 `join().expect` 也改成以錯誤回報。
  - ARC-02 的驗收條件：`stage.rs` 以外沒有 `join().expect(`；沒有 `Arc<Mutex<Receiver`；`reading_failed` 已移除；PRO-00 的驗收條件在第 5 步達成；任一 stage panic 在測試中都不會停滯，由 `stage.rs` 與 `pool.rs` 的測試驗證（整合測試要等 WP-06c 的 `FERRY_TEST_PANIC`）；20 萬個小檔沒有變慢（見下表）。
  - **user 時間增加**：20 萬個小檔，不含 fsync 時多了約 2.5 秒（第 10 步時還與基準相同）。sys 時間減少得更多，總 CPU 時間仍比基準少（約 87 秒對約 92 秒）。我推測原因是 readers 與 pool 的 worker 改為各持一份 crossbeam receiver 之後，閒置的 worker 會各自先短暫空轉再睡，以前只有拿到鎖的那一個在等；沒有實際查證。
  - **2026-10-18 補記：查證後，原因只在 readers，pool 的 worker 不是。** 這台機器沒有 perf 或 gdb，所以改從 `/proc` 取樣每條 thread 的 CPU 時間。實驗用的 build 把 role 設成 thread 名稱，不 commit。
    - 20 萬個小檔、`--no-fsync`，與基準交錯執行：Source readers 的 user 從 1.5–1.6 s 增為 3.5–4.1 s，sys 從 9–11 s 降為 2.4–2.6 s。sys 的下降是第 9c 步從共用的目錄 fd 開檔的效果。其他 role 的 user 都沒有增加，compress 還從 1.5 s 降為 1.0 s。
    - `DirCache` 的鎖不是原因：20 萬次 `open` 中只有 1–2% 遇到爭用；讓每條 reader 自己記住上次的目錄、幾乎不碰這把鎖之後，user 也沒有降。
    - readers 改回共用一個 receiver、放在 `Mutex` 裡之後，readers 的 user 降為 1.75–1.9 s。crossbeam 的 `recv` 遇到空的 channel 時，會先空轉約 127 次 pause、呼叫 4 次 `sched_yield`，才登記並睡著，`wait_until` 在睡前也一樣。readers 處理 4 KiB 檔案的速度比 file sender 補工作快，channel 常常是空的，所以 16 條 reader 每次都各自空轉一輪。共用一個 receiver 時，只有拿到鎖的那一條在 channel 上空轉，其他的睡在 mutex 上。
    - **修正，與 ARC-02 不同**：readers 改回共用一個 `Mutex<Receiver>`（`&Mutex`，不是 `Arc`），與 ARC-02 的驗收條件「沒有 `Arc<Mutex<Receiver`」，以及第 259 行「每個 worker 各自持有一份」不同。第 218 行說閒置的 worker 都排隊等這把 mutex；量測顯示，對 readers 來說，排隊等 mutex 比各自空轉便宜。readers 本來就只 `recv`，不與停止訊號一起 `select`，所以不影響停止：失敗時，送工作的那一端隨 scope 結束而 drop，readers 就結束。pool 的 worker 維持各持一份 receiver，因為量測沒有看到同樣的問題。
    - 量測：基準、第 13 步與這次修正交錯執行，各 3 次，見下面第三張表。4 GiB 的情境中，user 仍比基準多約 1 s（11.7–12.2 s 對 12.9–13.3 s）；這個情境不經過 readers，原因沒有查。
- 跨越各步：writer、fsync 與改名的分工維持現狀，因為 ARC-01 在第三波（D14）；結果改以事件送回 applier。

**量測**（各 3 次，real time）：

| 情境 | 基準（`600be6b`） | 第 5 步（`663b04f`） |
|---|---|---|
| 20 萬個小檔，`--no-fsync` | 7.5–9.1 s | 6.8–8.7 s |
| 20 萬個小檔，含 fsync | 17.5–26.9 s | 17.1–21.1 s |
| 4 GiB，`--no-fsync` | 3.2–3.5 s | 3.1–4.0 s |

第 7 步之後，機器的負載起伏很大，所以改為與基準交錯執行，各 4 次：

| 情境 | 基準（交錯） | 第 7 步（`09ec97c`） | 第 10 步 |
|---|---|---|---|
| 20 萬個小檔，`--no-fsync` | 7.8–10.0 s，sys 82–91 s | 9.5–10.9 s（基準同時 9.0–10.7 s） | 6.9–7.9 s，sys 72–81 s |
| 20 萬個小檔，含 fsync | 18.6–23.2 s，sys 42–48 s | — | 18.2–19.4 s，sys 37–39 s |
| 4 GiB，`--no-fsync` | 3.4–4.0 s | — | 3.5–3.8 s |

第 13 步（`de11e26` 之後），同樣與基準交錯各 4 次：

| 情境 | 基準 | 第 13 步 |
|---|---|---|
| 20 萬個小檔，`--no-fsync` | 7.7–8.5 s，user 9.4–9.9 s，sys 77–85 s | 7.5–7.8 s，user 11.7–12.6 s，sys 74–76 s |
| 20 萬個小檔，含 fsync | 18.8–21.3 s，user 11.0–11.2 s，sys 43–47 s | 18.3–19.0 s，user 11.8–12.5 s，sys 37–39 s |
| 4 GiB，`--no-fsync` | 3.4–3.7 s | 3.6–3.7 s |

2026-10-18 補記：readers 改回共用一個 receiver 之後（`44956bc` 之後），與基準、第 13 步交錯各 3 次：

| 情境 | 基準 | 第 13 步 | readers 共用 receiver |
|---|---|---|---|
| 20 萬個小檔，`--no-fsync` | 8.0–8.8 s，user 9.7–10.7 s，sys 82–85 s | 7.5 s，user 12.4–12.5 s，sys 73–76 s | 7.3–7.7 s，user 9.9–10.2 s，sys 73–77 s |
| 20 萬個小檔，含 fsync | 18.9–19.7 s，user 10.8–11.2 s，sys 44 s | 18.4–18.9 s，user 12.0–12.2 s，sys 37 s | 18.5–19.3 s，user 10.5–11.2 s，sys 38–39 s |
| 4 GiB，`--no-fsync` | 3.3–3.8 s，user 11.7–12.2 s | 3.5–3.8 s，user 12.5–13.4 s | 3.5–3.7 s，user 12.9–13.3 s |

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
