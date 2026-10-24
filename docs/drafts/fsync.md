# fsync：斷電之後，寫好的檔案還在不在

- 狀態：擱置。2026-10-24 使用者決定：預設不 fsync，fsync 相關的東西全部移除，只留 `--fsync` 這個選項，給了就報錯；相關討論寫在這裡，未來再回頭看。
- 開始：2026-10-24；最後修改：2026-10-24
- 基準：移除之前的 HEAD `6114252`（0.16.0 之後）。下面提到的程式位置都以它為準。移除的程式與這份文件在同一個 commit，`git log --diff-filter=A -- docs/drafts/fsync.md` 找得到；它的上一個 commit 就是移除之前的樣子。
- 會改動（要加回來時）：`docs/design/overview.md` 的「Every file is complete and correct」第 3 條與「Later, with room left for them in the design」、`docs/design/pipeline.md` 的「6. Write」與「8. Finish」、`docs/cli.md` 講 fsync 的那段與選項表的 `--fsync`，以及它們的 `.zh-TW.md` 版本。
- 相關：
  - `c06ff61`（2026-10-14）：fsync 改成批次，加上 macOS 的 barrier；
  - 審查的 ARC-01d（64 條 fsync 執行緒）、WP-19a（新建的根目錄所在的目錄也 fsync）、WP-15b（以 strace 檢查 fsync 的測試），見 `docs/review/2026-10-17/done/`；
  - `771bdcd`、`fd376ee`、`6114252`（2026-10-24 的量測與改進選項），原在 [architecture-redesign.md](architecture-redesign.md)，fsync 的部分搬到這裡。

## 為什麼要 fsync

寫入只進 page cache，kernel 之後才寫到磁碟。Linux 預設，髒資料放超過 30 秒就寫回（`vm.dirty_expire_centisecs` 3000，每 5 秒檢查一次）；髒資料超過可用記憶體的 10% 開始在背景寫回，超過 20% 就擋住寫入（`dirty_background_ratio` 10、`dirty_ratio` 20；fatcat 上約 12 GB 與 23 GB）。

斷電或 kernel 當機時，還沒寫回的就沒了。process 被殺、當掉、被 Ctrl-C 都不算：page cache 還在，照樣會寫回。

fsync 有三個用處：

1. **有名稱就是完整的。** 名稱（目錄項）與資料各自寫回，先後由檔案系統決定。沒有先 fsync，斷電後檔案可能有名稱，卻是空的或只有一部分；取代舊檔時，舊的也沒了。ext4 對「rename 蓋過舊檔」有補救（`auto_da_alloc`：rename 蓋過舊檔時，新檔的資料在這次 rename 的 journal commit 之前寫出），`linkat` 到新的名稱則沒有；其他檔案系統各不相同。
2. **寫回時的錯誤看得到。** 寫回時才發生的錯誤，例如 NFS server 的磁碟滿了、超過配額、磁碟故障，只有 fsync 一定會回報（NFS 上 `close` 也可能回報，見「移除之後少了什麼」）。fsync 失敗，那個檔案就失敗，不給它名稱。
3. **完成就是已在磁碟上。** Ferry 說複製好了，之後斷電也不會掉。

## 移除之前的做法

到 0.16 為止，預設全部做，`--no-fsync` 全部不做：

- **每個檔案只 fsync 一次。** 寫完、設好屬性之後（大檔是最後一段寫完、雜湊對上之後），交給 fsync 執行緒之一。最多 `SYNCING` = 64 條（`src/destination/write.rs:231`）；可開的檔案少時更少：`clamp((open_files − KEPT_OPEN) / 4, 1, 64)`。寫入執行緒交給 fsync 執行緒的 channel 容量等於 fsync 執行緒的數目，所以最多 64 個檔案同時在 fsync、另外 64 個在等，寫入執行緒則繼續寫下一個。fsync 失敗，那個檔案就失敗，暫存名稱移除。等 fsync 的檔案開著自己的 fd，批次的大小因此要扣掉這些 fd。
- **改名一批最多 256 個**（`BATCH`），由 4 條改名執行緒做。Linux 上 fsync 本身就等到資料進了儲存媒體，不需要別的。macOS 上 fsync 只把資料交給磁碟機，磁碟機可能在自己的快取裡重排，所以每批改名之前，每台磁碟設一道 `F_BARRIERFSYNC`：這批的資料都先於它們的名稱寫進儲存媒體。為此每批每台磁碟留一個檔案開著。
- **目錄。** 每個建立過東西的目錄，收尾時 fsync 一次。根目錄是檔案時，改名之後 fsync 它所在的目錄。這次執行建立了根目錄時，最後 fsync 根目錄所在的目錄（WP-19a）。
- **最後** `make_durable`：Linux 上 fsync 根目錄；macOS 上 `F_FULLFSYNC` 一次，清空磁碟機的快取。
- 資料額度在區塊寫進 page cache 時就歸還（`src/destination/mod.rs:156`），在 fsync 之前。
- **由來與當時的量測：**
  - `c06ff61` 把 fsync 改成批次。當時的 `docs/cli.md`：macOS 上複製 10 萬個小檔約 17 秒；只 fsync、不設 barrier 15 秒，但沒有這個保證；每個檔案都 `F_FULLFSYNC` 超過 500 秒。
  - ARC-01d 把 fsync 從寫入執行緒移到 64 條專門的執行緒。ext4 上 20 萬個 4 KiB 的檔案，8 核心時 6.3–7.1 秒，每個寫入執行緒自己 fsync 時 17 秒；128 條再快約 8%，但在 96 核心上系統時間更多。NFS 與 WekaFS 上沒有差別。

移除的東西：

- `fsys::fsync`、`barrier`、`sync_durable`、`apple_fcntl`、`BARRIER_NEEDS_A_FILE`；
- `write.rs` 的 fsync 執行緒、`SYNCING`、`syncers`、`Ready::sync`、`Ready` 為 fsync 與 barrier 留的檔案與磁碟編號、`into_batch`；
- `durable.rs` 的 `barrier`，檔案改名為 `rename.rs`；
- `finish.rs` 的目錄 fsync 與 `make_durable`；`State::root_made`、`DirCache::held_root`；
- profile 的「fsync files」與「wait for the fsync threads」；
- Hello 與 `destination::Options` 的 `fsync`；`--no-fsync` 的作用（選項本身後來又留下，沒有作用，見「待決定的問題」）；
- 測試：`files_and_directories_are_fsynced` 改成 `nothing_is_fsynced`（strace 看不到任何 fsync），`copies_without_fsync` 改成 `fsync_is_refused`；`the_fsync_threads_fit_the_open_file_limit`、`a_batch_keeps_one_file_open_for_each_drive`、`ordered_and_durable_syncs_work_here` 拿掉；`ferry test` 的「without fsync」情境拿掉。

沒有動：`src/update.rs` 的自我更新，新的執行檔與它所在的目錄仍然 `sync_all`，再 rename。那不是傳輸的一部分，只有兩行；斷電後執行檔變成空的，ferry 就不能用了。

## rsync 怎麼做

讀 rsync 3.2.7 的原始碼（本機的版本；原始碼在 `.scratch/rsync-src/`）：

- 預設不 fsync（`do_fsync = 0`）。暫存檔加 rename 只防中斷，不防斷電。
- `--fsync` 是 3.2.4（2022）才加的：在 `receive_data` 的最後 fsync 暫存檔（`receiver.c:399`），在比對 checksum 與 rename 之前；失敗就整個結束（`exit_cleanup(RERR_FILEIO)`）。不 fsync 目錄，所以斷電之後名稱可能不在。receiver 一個檔案接一個處理，所以同一時間只有一個 fsync。
- 不論有沒有 `--fsync`，暫存檔的 `close` 都檢查，失敗就整個結束（`receiver.c:885`）。`util1.c` 的 `copy_file`（改名跨了檔案系統時改用複製，例如 `--temp-dir` 在另一個檔案系統上）也一樣：`--fsync` 時 fsync，`close` 都檢查。

## 量到的代價

### 小程式：每檔 fsync、最後 sync 一次、不 sync

2026-10-24，使用者問。程式是 `.scratch/verify/a1/nfiles.py`，不是 ferry。寫法三種：

- **每檔 fsync**：每個檔案寫完就 fsync，最多 N 個同時等，等的佇列上限 N，如移除之前的 Destination；
- **最後 sync 一次**：都不 fsync，全部寫完後整個檔案系統 sync 一次，如 `cp` 再 `sync`；
- **不 sync**：寫完關檔就算，如 `--no-fsync`、rsync 的預設，與現在的 ferry。

大檔是 16 個 128 MiB 的檔案，共 2 GiB；小檔寫 5,000 個 4 KiB 的檔案，在 50 個目錄裡。讀之前以 fadvise 丟掉資料（目錄與 inode 仍在快取裡），讀是同一天稍早量的數字。表中是中位數，各 5 輪，硬碟的小檔 3 輪。

| 大檔，N：1、2、4、8、16 | fatcat 硬碟（MB/s） | 本機 SSD（ext4，3 顆 RAID0） | fatcat NFS（NFSv4.1） | nano4 WekaFS（GB/s） |
|---|---|---|---|---|
| 讀 | 231、220、188、177、177（安靜的那輪） | 839、854、855、855、855 | 815、1,118、1,163、1,160、1,151 | 3.3、6.7、12.3、14.4、13.1 |
| 寫，每檔 fsync | 206、209、209、122、99 | 769、778、784、784、775 | 1,006、1,110、1,096、1,101、1,121 | 2.2、3.5、4.6、4.2、5.0 |
| 寫，最後 sync 一次 | 170、170、159、211、181 | 557、643、695、741、764 | 703、910、1,029、1,084、1,126 | 2.5、4.1、5.9、4.7、4.9 |
| 寫，不 sync | 4.7k、9.6k、16.6k、21.8k、39.7k | 1.9k、3.4k、5.6k、9.4k、15.4k | 713、928、1,030、1,089、1,117 | 2.9、5.4、9.7、11.4、10.7 |

| 小檔，N：1、16、64（個／秒） | fatcat 硬碟 | 本機 SSD | fatcat NFS | nano4 WekaFS |
|---|---|---|---|---|
| 讀 | 475、1,247、1,624 | 13.4k、60.2k、74.8k | 4.7k、48.3k、52.3k | 1.2k、15.5k、17.6k |
| 寫，每檔 fsync | 28、197、411 | 3.7k、16.3k、19.8k | 3.0k、3.5k、5.8k | 1.7k、2.3k、4.4k |
| 寫，最後 sync 一次 | 3.9k、2.2k、1.8k | 39.0k、37.3k、36.7k | 1.7k、3.3k、5.9k | 1.5k、2.3k、4.4k |
| 寫，不 sync | 76.6k、49.1k、47.5k | 56.7k、54.7k、52.9k | 1.8k、3.4k、6.0k | 1.5k、2.4k、4.4k |

- **不 sync 的大檔只是寫進 page cache 的速度**，資料還沒到磁碟。2 GiB 放得進記憶體；更大的傳輸超過髒資料的上限之後，會慢到磁碟的速度。所以保證的代價，要比的是每檔 fsync 對最後 sync 一次。
- **大檔：** 硬碟上同時 fsync 1 到 4 個約 210 MB/s，不比最後 sync 一次慢；8、16 個剩 100–120，最後 sync 一次仍有 160–210（它也寫了別人的髒資料，每輪 90–1,700 MB，所以偏慢）。SSD 上 N 大時兩者一樣，N 小時每檔 fsync 反而快，因為寫入與寫回重疊。NFS 上三種一樣：NFS 關檔時就把資料送到 server，並等它寫好（close-to-open），fsync 不多花時間，所以 `--no-fsync` 在 NFS 上沒有用。WekaFS 上不 sync 快約一倍，但最後 sync 一次與每檔 fsync 差不多，那是快取的速度。
- **小檔：只有本機的磁碟有代價。** 硬碟上每檔 fsync，1 條每秒 28 個，是最後 sync 一次的 140 分之一；64 條 411 個（安靜的那輪 621），仍慢 4 到 9 倍。SSD 上 1 條慢 11 倍，64 條慢 1.9 倍。NFS 與 WekaFS 上三種一樣，瓶頸在建檔與關檔的往返。同時等的 fsync 越多越好，64 條比 1 條：硬碟 15–22 倍，SSD 5.4 倍，NFS 1.9 倍，WekaFS 2.6 倍：一次 journal 寫入涵蓋的檔案越多。
- **限制：** 每檔 fsync 多了 N 條 fsync 執行緒，關檔也在那裡做，所以 NFS 與 WekaFS 上 N=1 時看來比不 fsync 快。小檔不 sync 的數字受限於 Python 本身，約每秒 5–8 萬個。硬碟上別人一直在讀寫，有一輪別人讀了 8.7 GB。WekaFS 的數字隨 login node 的負載（170–500）變動。
- **更正：** 硬碟的「小檔，每檔 fsync」最初量成 159、999、3,406，是錯的。程式在 fatcat 上撞到 1,024 個開檔的上限，寫入的執行緒寫了約 1,020 個檔案就結束，速度卻照 5,000 個算；等 fsync 的佇列又沒有上限，N=1 那輪跑超過 30 秒，剩下的由 kernel 自己寫回。程式改成一條執行緒出錯就整次失敗、開檔上限提高、佇列上限 N 之後重量，是 28、197、411。其他欄沒有撞到上限，也都在 30 秒內。
- 程式與輸出：`.scratch/verify/a1/` 的 `nfiles.py`、`nsum.py`、`fatcat-sync-run.sh`、`nano4-sync-run.sh`、`sync-*.out`。

### Ferry 本身

- **大檔到硬碟**（2026-10-24）：16 個 256 MiB 的檔案從 RAM 複製到 fatcat 的硬碟，各 3 輪：A1 之前 55.0–80.2 秒，A1 45.4–56.6 秒，A1 改成照順序讀之後 37.5–51.7 秒；`cp` 再 `sync` 19.6–41.5 秒（中位數 21.6；41.5 秒那輪別人也在寫）。程式是 `.scratch/verify/a1/many.py`，輸出 `many-hdd.out`。原因：額度在寫進 page cache 時就還，網路比磁碟快時，好幾個大檔一起等 fsync，硬碟在它們之間來回；小程式裡同時 fsync 16 個大檔約 80–100 MB/s，1 到 4 個約 180–210。
- **小檔到 SSD**（2026-10-24，移除時）：20 萬個 4 KiB 的檔案，2,000 個目錄，從 `/dev/shm` 複製到本機的 ext4（3 顆 SSD 的 RAID0），release build，各 5 輪交錯，中位數：

  | build | 秒 | 5 輪 |
  |---|---|---|
  | 移除之前（`6114252`），每檔 fsync | 6.25 | 5.77–6.45 |
  | 移除之後，不 sync | 2.01 | 1.77–2.44 |
  | 移除之後，再 `sync -f` 目的地 | 3.22（其中複製 1.93） | 3.07–3.72 |

  每檔 fsync 是最後 sync 一次的兩倍左右，與小程式的 SSD 64 條（1.9 倍）一致。量的時候 load average 升到 34，事後看 `ps` 沒有別人的重工作，應是這幾輪自己在 D 狀態的執行緒。程式是 `.scratch/fsync-off/smallcopy.py`，輸出 `smallcopy-ext4.out`。
- **沒有用 ferry 量的：** 硬碟上的小檔（小程式推算，20 萬個每檔 fsync 約 5–8 分鐘，最後 sync 一次約 1–2 分鐘）、NFS、WekaFS。

## 為什麼移除

使用者（2026-10-24）：「fsync 目前真的讓我們的設計太複雜了。」它要的東西：

- 64 條 fsync 執行緒與它們的 channel；等 fsync 的檔案開著 fd，批次大小與執行緒數都要依開檔上限算；
- macOS 每批每台磁碟一道 barrier，為此留檔案開著；最後一次 `F_FULLFSYNC`；
- 目錄收尾時 fsync、根目錄所在的目錄、`make_durable`；
- 一個傳到 server 的選項，與兩個 profile 狀態；
- 量出來的問題還在增加：硬碟上好幾個大檔同時 fsync 會慢一半，要再加「大檔同一時間只 fsync 一個」（下面）；硬碟小檔的量測一度量錯。

而它的效益只在斷電或 kernel 當機時才看得到；rsync 預設也不做。

## 移除之後少了什麼

1. **斷電或 kernel 當機。** 傳輸後約半分鐘內（Linux 預設），檔案可能有名稱卻是空的或只有一部分，被取代的舊版本也不在；名稱也可能不在。Ferry 自己怎麼結束都不影響：page cache 還在。
2. **寫回時的錯誤可能沒人看到，檔案卻算複製成功。** 本機的 ext4、xfs 在 `write` 時就預留空間，磁碟滿了 `write` 就會失敗；剩下的是磁碟故障、thin provisioning 之類。網路檔案系統不同：NFS server 的磁碟滿了、超過配額，錯誤要到寫回時才知道。
   - rsync 靠檢查暫存檔的 `close`（見上）。Ferry 不檢查：檔案是 Rust 的 `File`，drop 時關閉，錯誤丟掉。以前 fsync 在 close 之前，錯誤由 fsync 抓到；`--no-fsync` 時就已經有這個缺口，現在變成預設。
   - 只補檢查 `close` 可能不夠：Ferry 在關檔之前，透過 fd 設屬性（`fchown`、`fchmod`、`futimens`）。我的印象（讀 kernel 原始碼的記憶，沒有查證，版本之間也不同）：Linux 的 NFS 設屬性前先把髒資料寫出（`nfs_setattr` 裡的 `nfs_sync_inode`），寫出時的錯誤記在 mapping 上，setattr 本身不回報；5.x 的 `nfs_file_flush` 只回報它自己開始之後的錯誤，所以 `close` 可能回報 0。rsync 是先 `close`，再以路徑設屬性，沒有這個問題。
   - 怎麼補是待決定的問題（下面）。沒有在 NFS 上驗證：fatcat 的 NAS 是共用的，又快滿了，不能故意把它寫滿。
3. **完成不等於已在磁碟上。** 需要的話，傳輸後在 Destination 的機器上執行 `sync`；Linux 的 `sync` 會等到寫完，macOS 的不一定。
4. **NFS 上本來就沒差：** 上面的小程式，三種寫法在 fatcat 的 NFS 上一樣快，關檔時資料已經送到 server。WekaFS 上每檔 fsync 與最後 sync 一次也差不多。

## 曾列的改進選項：Destination 的大檔，同一時間只 fsync 一個

2026-10-24 量到後列入 architecture-redesign.md 的「改進選項」，還沒決定，fsync 就移除了。原文：

- 現在：每個檔案只 fsync 一次，在寫完、給了屬性之後（大檔是最後一段寫完、雜湊對上之後），交給 64 個 fsync 執行緒之一，所以最多 64 個檔案同時在 fsync。額度在寫進 page cache 時就還。網路比磁碟快時，好幾個大檔一起等 fsync，硬碟在它們之間來回。
- 量到的：16 個 256 MiB 的檔案從 RAM 複製到 fatcat 的硬碟，ferry 要 37.5–80.2 秒，三版都是，A1 之前也是；`cp` 再一次 `sync` 只要 19.6–21.6 秒（另一輪 41.5 秒，那時別人也在寫）。小程式也一樣：16 個大檔同時 fsync 約 80 MB/s，1 到 4 個約 180–200 MB/s。
- 做法：大於一段的檔案，照寫完的順序，同一時間只 fsync 一個（或兩個），由一條專門的執行緒做，每個檔案仍只 fsync 一次；小檔照舊，同時等的越多越好，硬碟上 64 條是 1 條的 15–22 倍。也可以在每段寫完時以 `sync_file_range` 先開始寫回，讓資料照檔案的順序下去。
- 代價：NFS 與 WekaFS 上，同時 fsync 好幾個大檔比較快（小程式 16 個比 1 個快 11%、74%），但 ferry 在那裡寫入只有約 0.5–0.7 GB/s，遠低於同一時間只 fsync 一個時的上限（1.0、2.4 GB/s）。
- 還沒量：改了之後 ferry 寫到硬碟能多快；SSD、NFS、WekaFS 上會不會變慢。

## 要加回來時的做法

| 做法 | 保證 | 代價（依上面的量測） | 複雜度 |
|---|---|---|---|
| A. 照移除之前：每檔 fsync（N 條）、macOS 的 barrier、目錄 fsync、最後 `F_FULLFSYNC` | 三個用處都有 | 本機小檔慢（SSD 約 2 倍，硬碟 4–9 倍）；硬碟上好幾個大檔同時 fsync 慢一半 | 最高；程式從 git 拿得回來 |
| B. 照 rsync 的 `--fsync`：每個檔案改名前 fsync，失敗就讓它失敗；不 fsync 目錄，不設 barrier | 有名稱就完整（Linux；macOS 要 barrier 或 `F_FULLFSYNC` 才算）、錯誤看得到；名稱可能不在 | 與 A 差不多，少了目錄的 fsync | 中：N 條 fsync 執行緒 |
| C. 最後每個 Destination 的檔案系統 `syncfs` 一次，如 `cp` 再 `sync` | 只有「完成就在磁碟上」；中途斷電照樣有空的檔案；寫回錯誤看得到（Linux 5.8 起 `syncfs` 回報），但不知道是哪個檔案 | 最便宜：SSD 小檔 3.2 對 6.3 秒（ferry）；硬碟小檔比每檔 fsync 快 4–9 倍（小程式） | 低；macOS 沒有 `syncfs`，`sync` 不等 |
| D. A 或 B，加上大檔同一時間只 fsync 一個（上面的改進選項） | 同 A 或 B | 硬碟上的大檔可望接近 `cp` 再 `sync`；沒量 | A 或 B 再多一條執行緒 |
| E. 只檢查 `close`（並改成先關檔再設屬性） | 只有 NFS 這類關檔時寫出的檔案系統，寫回錯誤看得到 | 幾乎沒有 | 低；但屬性要改以路徑設，O_TMPFILE 的檔案沒有路徑 |

E 與 fsync 無關，可以先做；見下面的問題。

## 改名的批次

改名一批 256 個，是 `c06ff61` 為 fsync 加的：一批共用一道 barrier，目錄的 fsync 也分攤到一批。fsync 拿掉之後，批次還在，因為改名執行緒還有別的用處：寫入執行緒不必等改名；一批跨好幾個目錄時，改名平行做，同一個目錄只由一條執行緒改名（量到的：比一條改名執行緒快 26%（WekaFS）與 20%（ext4））。這次沒有拿掉批次，理由：

- 要量：寫入執行緒自己改名，是否比交給改名執行緒慢，特別是每次改名都要等 server 的 NFS 與 WekaFS；
- 現在同一個目錄大致只由一條執行緒改名（目錄從快取被擠掉又重開時，一批裡會分成兩組）。改成寫入執行緒自己改名，大小寫不分的檔案系統上，兩條執行緒可能同時替 `A` 與 `a` 做 DST-18 的檢查（`not_an_alias`），都沒看到對方，再各自以暫存名稱 rename，後一個蓋掉前一個（沒有名稱的檔案以 `linkat` 取名，不會）；要另想辦法，例如 `renameat2` 的 `RENAME_NOREPLACE`；
- 不滿一批的最後幾個檔案要等到傳輸結束才有名稱（2026-10-24 發現，待使用者決定），批次拿掉就沒有這個問題。

## 待決定的問題

- [ ] **寫回的錯誤怎麼辦**（使用者決定）：照 rsync 檢查 `close`（E），或最後 `syncfs` 一次（C），或照現在不管。建議先在 NFS 上找一個能寫滿的地方，確認錯誤到底在哪一步出現，再決定。
- [x] **`--no-fsync` 要不要留著當作沒有作用的選項**：起初照使用者的話拿掉，舊的腳本會以結束狀態 2 失敗，clap 還會建議「a similar argument exists: '--fsync'」，正好相反。2026-10-24 使用者決定留著：接受、沒有作用、不列在 `--help`，只寫在 `docs/cli.md`。rsync 3.2.7 沒有這個選項（給了是 unknown option，結束狀態 1），所以這是 Ferry 自己的相容，不是比照 rsync。
- [ ] **改名的批次要不要拿掉**（使用者決定，要先量）：見上。
- [ ] **什麼時候加回 fsync，用哪個做法**：未來回頭看時決定。

## 計畫

現在沒有。
