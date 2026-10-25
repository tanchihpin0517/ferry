# Ferry 全面審查：交接文件

- 日期：2026-10-17
- 基準：HEAD `3c17a2f`，version 0.14.4。所有行號都以這個 commit 為準。
- 審查時的狀態：`cargo fmt --check` 與 `cargo clippy --all-targets` 都沒有任何警告，`cargo test` 216 個測試全部通過。
- 做法：專案分成五個區域，各由一個審查 agent 逐行追程式碼，能實測的都實際跑過。最嚴重的幾項，彙整者又親手重現或核對過一次。
- 2026-10-22 的狀態：114 項中 100 項已完成，移到 [`done/`](done/)，檔名與原檔相同，內容沒有改。原檔只留 14 項：還沒完成的（CI-04、CI-05、TRN-11、TST-03、DST-06、SRC-09；DST-06 於同日做完，也移到 `done/`），決定不做的（TRN-03、CI-07、SRC-11、ARC-04），以及修正已在、但沒有測試證明的（PRO-00、SRC-12、SRC-17、DST-12）。同日 SRC-12、DST-12 補上測試，PRO-00、SRC-17 決定接受沒有測試，四項都移到 `done/`。SRC-09 的 `-xx` 決定不做，也移到 `done/`。TST-03 的每晚 fuzz 已照排程跑過，也移到 `done/`。2026-10-25：TRN-11 測的測試與程式碼已在 A2 拿掉，以不再適用結案，也移到 `done/`。本頁其餘各節沒有改，提到的項目可能在 `done/` 裡。
- 用途：給之後執行修復的 agent。每一項都可以單獨閱讀，內容包含位置、根本原因、重現方式、修法與取捨、要加的測試、驗收條件，以及要同步的文件。

## 目錄

1. [檔案與編號](#1-檔案與編號)
2. [給執行修復的 agent：使用方式](#2-給執行修復的-agent使用方式)
3. [專案慣例（動手前必讀）](#3-專案慣例動手前必讀)
4. [優先順序](#4-優先順序)
5. [架構分析：問題背後的共同原因](#5-架構分析問題背後的共同原因)
6. [工作分包與執行順序](#6-工作分包與執行順序)
7. [需要使用者決定的事項](#7-需要使用者決定的事項)
8. [重複與交叉參照](#8-重複與交叉參照)
9. [做得好的地方：修的時候不要改壞](#9-做得好的地方修的時候不要改壞)
10. [長期：協定與功能的預留](#10-長期協定與功能的預留)
11. [實驗素材](#11-實驗素材)

---

## 1. 檔案與編號

| 檔案 | 編號 | 範圍 |
|---|---|---|
| [destination.md](destination.md) | DST-01 至 DST-19 | Destination、檔案系統層：`destination.rs`、`fsys.rs`、`compare.rs`、`owners.rs`、`names.rs`、`record.rs`、暫存檔登記 |
| [source.md](source.md) | SRC-01 至 SRC-17 | Source、掃描與並行：`source.rs`、`scan.rs`、`pool.rs`、`mux.rs`（傳送端）、`interrupt.rs`、`profile.rs` |
| [protocol.md](protocol.md) | PRO-00 至 PRO-15 | 線路格式與訊息處理：`protocol.rs`、`frame.rs`、`record.rs`、`manifest.rs`、`mux.rs`、`relay.rs`、`session.rs`、`peer.rs`、`control.rs` |
| [transport.md](transport.md) | TRN-01 至 TRN-24 | transport、SSH、遠端安裝、發布與更新：`transport/`、`remote/`、`update.rs`、`scripts/install.sh`、`release.yml` |
| [cli-tests-ci-docs.md](cli-tests-ci-docs.md) | CLI-01 至 CLI-10、TST-01 至 TST-07、CI-01 至 CI-07、DOC-01 至 DOC-09 | CLI 與使用體驗、測試、CI 與 release、文件 |
| [architecture.md](architecture.md) | ARC-01 至 ARC-05 | 架構改動的方案與量測：寫入 stage、thread 的組合方式、Destination 狀態的擁有者、大檔分段、小調整 |
| [target-architecture.md](target-architecture.md) | Q1 至 Q9（開放問題） | 長期方向：如果從零設計，架構會長什麼樣子。從高層概念、分層、串流、準備的狀態機、協定、流量控制，寫到 Source 與 Destination 的 pipeline、持久性、錯誤與重連、安全、測試，以及從現在一步步走過去的路 |
| [materials/](materials/README.md) | — | 重現問題與量測用的腳本、harness 與紀錄（第 11 節） |

前五份一共 109 項，扣掉第 8 節列出的重複項目，約 102 個獨立問題。`architecture.md` 是另外的架構方案，每個方案會吸收其中一部分項目。

每一項的嚴重度，是該區審查者在區域內的判斷：嚴重、高、中、低。跨區域的整體優先順序，看第 4 節。

驗證狀態分三種：

- **已實測重現**：實際跑過，親眼看到現象。
- **已核對程式碼**：逐行追過程式碼路徑，但沒有實際觸發。
- **推測**：可能發生，但沒有完整追到，或缺少實測環境，例如 tcsh、macOS、NFS。

各檔案中的「審查者」指負責該區的審查 agent，「彙整者」指寫這一頁的人。

---

## 2. 給執行修復的 agent：使用方式

1. **先讀本頁第 3 節（專案慣例）和第 7 節（需要使用者決定的事項）。** 碰到第 7 節的決定時，先停下來問使用者，不要自己決定。
2. **一次只做一個工作包**（第 6 節），除非使用者另有指示。工作包列出了涵蓋的項目、相依關係，以及會動到的檔案。
3. **讀完工作包中每一項的全文。** 每一項都有「修法」、「要加的測試」、「驗收條件」、「文件要同步的地方」，請照著做。若有更好的做法，可以採用，但要在 commit 訊息中說明理由。
4. **先確認問題還在。** 行號以 `3c17a2f` 為準，之後的 commit 可能讓行號偏移，也可能已經修好。請用函式名稱或程式碼片段搜尋，不要只靠行號。問題已經不在時，回報使用者，不要硬改。
5. **先寫重現問題的測試，看它失敗，再修，再看它通過。** 最後跑 `make check`。
6. **修法與 `docs/design/` 的設計衝突時，以設計文件為準，並回報使用者。** 例如修法會改變某個「Decided」的行為。若使用者同意改設計，設計文件要一起改。
7. **實驗素材**（第 11 節）在 [`materials/`](materials/)，已納入版本控制，worktree 中也有。執行時的建置結果與工作目錄一律放在 repo 根目錄的 `.scratch/` 下，不要放進 `docs/`。
8. **完成一項後，回報三件事：** 改了什麼、測試結果（附上輸出）、還沒做或刻意不做的部分。

---

## 3. 專案慣例（動手前必讀）

**建置與測試**

- `make check`：依序執行 `make fmt`、`make lint`、`make shellcheck`、`make test`。
  - CI 的 lint 使用 `make lint LINT_DENY=-Dwarnings`，clippy 的任何警告都算失敗。本機請以同樣的標準檢查。
  - CI 在 Linux 與 macOS 上都跑 lint。`cfg(target_os = ...)` 之後的程式碼兩邊都必須能編譯。
- `make test TARGET=x86_64-unknown-linux-musl`：測試 Linux release 使用的靜態 musl build。需要先安裝 `musl-tools`。
- `make filesystem-test FS=xfs|btrfs|ext4-casefold`：在 loop device 上的其他檔案系統執行測試。需要 sudo。
- `make container-test IMAGE=rockylinux/rockylinux:8`：在其他發行版的容器中測試。需要 Docker。
- `ferry test SOURCE DESTINATION`：端到端情境測試，會在 SOURCE、DESTINATION 底下建立並移除 `ferry-test-<id>-*`。本機可以用 `ferry test /tmp /tmp`。情境定義在 `src/testkit/`。
- `ferry test --scale SIZE --kind KIND`：規模測試，用來量測記憶體與速度。

**測試的寫法**

- 單元測試放在模組的 `mod tests`，整合測試放在 `tests/*.rs`，`ferry test` 的情境放在 `src/testkit/`。
- 測試要等它要測的事發生，不要用 `sleep` 猜時間，見 commit `5c38d02`。有逾時的地方，設寬鬆的上限。
- 需要 SSH 的測試，用 `FERRY_SSH` 指向假的 ssh，`tests/remote.rs` 已經這樣做。sudo 也有假的版本。
- 需要 root 的測試，在非 root 時略過；反之亦然。
- 暫存與實驗檔案放在 repo 根目錄的 `.scratch/`，它已被 `.gitignore` 涵蓋。

**程式碼風格**

- 註解用完整的英文句子，說明「為什麼」與行為，語氣與密度比照周圍的程式碼。例如 `destination.rs` 的模組說明與 `Open` 的註解。
- 錯誤用 `anyhow`，以 `.context(...)` 說明正在做什麼。使用者看到的訊息以 `ferry: ` 開頭，用語比照現有訊息。
- 名稱一律以位元組（`Vec<u8>`、`&[u8]`）處理，不假設是 UTF-8。
- 目的地上的每一個操作都從已開啟的目錄 fd 出發，用 `openat` 加 `O_NOFOLLOW`，以名稱操作前先過 `fsys::one_name`。不要新增以完整路徑操作的程式碼。

**協定**

- client 與 server 一定是同一個 build。握手時比對 build identity，不同就拒絕。所以改協定**不需要相容舊版**，直接改 `protocol.rs` 的型別即可。

**文件**

- `docs/` 下的每份文件都有 `.zh-TW.md` 版本，兩者要在同一個 commit 中一起改。
- 設計文件的每一節都標明「Decided」、「Proposed」或「Not chosen」。行為改變時，同步更新對應的段落。
- `TODO.md` 是工作清單，依序記錄每一步做了什麼。
- 還沒決定的設計改動，例如開始動工的 ARC 方案，先在 [`docs/drafts/`](../../drafts/README.zh-TW.md) 用範本開一份草稿，在草稿中討論；決定之後再寫進 `docs/design/`，並刪除草稿。

**commit 訊息**，比照 `git log`：

- 標題是 `type: 一句話`，type 有 `feat`、`fix`、`perf`、`test`、`ci`、`chore`、`docs`。一句話用小寫開頭，描述行為，而不是描述改了哪段程式碼。例如：
  - `fix: a pool counts each item until its result is taken, so its bound holds when the taker is slow`
  - `fix: delete from the directory the comparison listed, by its fd, not its path`
- 內文是條列：使用者看得到的變化、量測數字，最後一條寫 `- Tests: ...`。
- 版本號由使用者決定。不要自行修改 `Cargo.toml` 的 version、建立 `chore: version X.Y.Z` 的 commit，或打 tag。
- 依使用者的指示 commit。commit 訊息中不要加 co-author。

---

## 4. 優先順序

### P0：會刪錯資料或造成提權，最先修

| ID | 問題 | 驗證 |
|---|---|---|
| [CLI-01](cli-tests-ci-docs.md) | SOURCE 與 DESTINATION 重疊時，`sync` 會刪掉 Source 本身：`sync a/sub a` 和 `sync a a/sub` 都會。`ferry sync ./site web1:` 少打路徑時，等於同步到遠端家目錄，會清空它；`/` 也沒有防呆 | 已實測重現 |
| [DST-01](destination.md) | 比較之後，若目的地的檔案被換成 symlink，chmod 會改到目的地之外的檔案。以 root 執行時可被用來提權 | 已實測重現（呼叫層級） |

### P1：一般情境就會遇到，或會卡死、大量失敗、留下垃圾

| ID | 問題 | 驗證 |
|---|---|---|
| [DST-03](destination.md)（含 [SRC-07](source.md)） | 每個等待改名的檔案占 3 個 fd，而程式從不調高 `RLIMIT_NOFILE`。`ulimit -n 256`（macOS 的預設值）時，2,000 個小檔有 1,921 個失敗 | 已實測重現 |
| [DST-02](destination.md)（＝[PRO-02](protocol.md)） | 不是由訊號造成的失敗，例如 Source 當掉、connection 中斷、串流損壞，都會在目的地留下 `.ferry-` 暫存檔。實測一次就留下 3.7 GB | 已實測重現 |
| [SRC-01](source.md) | 需要的檔案在掃描後被換成 FIFO，傳輸永遠卡住 | 已實測重現 |
| [TRN-01](transport.md) | 一側的 server 已經在等 heartbeat，另一側才開始探測與安裝。第一次推送到新主機時，安裝超過 40 秒，先啟動的 server 就會自行結束 | 已實測重現 |
| [TRN-02](transport.md) | 上傳中斷時，殘缺的 binary 仍被改名就位，之後每次執行都崩潰 | 已實測重現 |
| [PRO-01](protocol.md) | 一個目錄的 `Needs::Deletes` 超過 16 MiB 時，`sync` 先刪完多餘的項目，接著整個傳輸失敗 | 已實測重現 |
| [PRO-00](protocol.md)、[PRO-04](protocol.md)、[PRO-05](protocol.md) | 沒有「記下第一個錯誤並讓所有 stage 停下」的機制。後果有三：Destination 的寫入端已經失敗，主 thread 仍繼續套用與刪除；回報的是後果，不是原因；`Control::Error` 送了兩次 | 已實測重現 |
| [SRC-02](source.md) | 傳輸失敗後，Source 仍掃完整棵樹才結束。在大型 NFS 樹上，錯誤可能延遲數小時才出現 | 已實測重現 |
| [SRC-03](source.md) | 任何 panic 都變成無聲的永久停滯。file sender 出錯時從不告知 Destination | pool 部分已實測重現 |
| [DST-04](destination.md)、[DST-05](destination.md)、[DST-06](destination.md) | 一個單點錯誤拖垮全部：一個目錄不能讀，整個傳輸中止；一個目錄收尾失敗，之後的目錄都不收尾；擁有者、權限或 mtime 設不上，已驗證的檔案被丟掉 | 已實測重現 |
| [PRO-06](protocol.md) | 需要的檔案始終沒送來，Destination 也不回報，傳輸仍算成功 | 已實測重現 |
| [CLI-02](cli-tests-ci-docs.md) | 沒有終端機時，各種 SSH 失敗一律被說成「needs a login」 | 已實測重現 |
| [CLI-04](cli-tests-ci-docs.md) | `host:~/path` 的 `~` 不會展開，複製會失敗；遠端剛好有名為 `~` 的目錄時，會寫錯地方 | 已實測重現 |
| [TRN-04](transport.md) | 登入 shell 是 csh 或 tcsh 時，多行腳本失敗，探測與安裝都做不了。HPC 主機常見這種設定 | 推測 |
| [TRN-05](transport.md) | Ferry 只收到 SIGTERM 時，留下的 `ssh -N` 讓 master 永不過期 | 已實測重現 |
| [TRN-03](transport.md) | release 只有同一來源的 `SHA256SUMS`，沒有簽章。帳號或 token 外洩時，攻擊者可以同時換掉 binary 與總和 | 已核對程式碼 |
| [TST-01](cli-tests-ci-docs.md) | 最核心的完整性保證「BLAKE3 不符就不改名」沒有任何測試 | 已核對程式碼 |
| [CI-01](cli-tests-ci-docs.md) | 測試沒有個別的逾時。卡住時要等滿 30 分鐘的 job timeout，也看不出是哪個測試 | 已核對程式碼 |

### P2：其餘項目

依第 6 節的工作包處理即可。各區的嚴重度寫在各檔案開頭的總表。

### 架構方案

架構方案不是缺陷，所以不列入 P0 到 P2。它們排進第 6 節的第二到第四波，詳見 [architecture.md](architecture.md)。效益最明確的是 ARC-01：小檔情境中，Ferry 的系統時間是 `cp` 的 19 倍，主要卡在同一目錄的鎖。

---

## 5. 架構分析：問題背後的共同原因

這一節說明的是：這些問題大多不是個別的疏忽，而是出自幾個架構缺口。只逐項修補，同一類問題還會在新的程式碼中再出現，所以建議把下面的機制補上。

具體的架構方案、量測數據與步驟，寫在 [architecture.md](architecture.md)：

- A1 的實作方式是 ARC-02。
- A6 的實作方式是 ARC-03。
- 另外，量測發現寫入 stage 在小檔情境中被目錄鎖拖慢，方案是 ARC-01。
- 大檔的平行讀寫是 ARC-04。
- 這些方案最後收斂成的整體設計，寫在 [target-architecture.md](target-architecture.md)。

### A1. 沒有「第一個錯誤，全體停止」的機制

**現況。** 一個 peer 內有十幾條 thread：主 thread、compare、stat pool、pump、解壓縮池、feeder、parser、16 個 writer、renamer、finisher，Source 端還有 lister、file sender、reader、frame sender。thread 之間傳遞錯誤的唯一方式，是「channel 被關掉，下一個去讀寫它的人跟著失敗」。結果有：

- 先失敗的 thread，要等別的 thread 碰到被關掉的 channel，其他人才會知道。這段期間，其他 thread 照樣改動目的地（PRO-04），Source 照樣掃完整棵樹（SRC-02）。
- 後面碰到的錯誤，例如「the workers stopped」或 EOF，看起來也像原因，於是蓋掉真正的原因（PRO-05）。現在唯一能區分前因後果的是單向的 `reading_failed`，而且它會和 parser 互相競爭。
- panic 的 thread 不會關掉任何東西，所以變成永久停滯（SRC-03）。
- 某些錯誤路徑根本不通知對方。file sender 的錯誤不會送到 Destination（SRC-03），只剩下等待。

**建議。**

1. 新增 `src/stop.rs`，兩個 peer 共用。它記下第一個「原因」，保證 `Control::Error` 只送一次、`Done` 絕不在 `Error` 之後送出，並提供 `check()` 讓每個 stage 在改動目的地之前檢查。另外把「後果」型別化成 `Stopped`，讓它永遠不會被當成原因。完整設計見 **PRO-00**，Source 端的套用見 **SRC-02** 與 **SRC-03**。
2. panic 的政策：建議在 release 與 dev profile 都設 `panic = "abort"`，再加一個 panic hook，依序還原終端機、印出 `ferry: internal error in <role>`、移除暫存檔，最後以 101 結束。這樣每一條 thread 都適用，不必逐一包 `catch_unwind`。見 **SRC-03**，這需要使用者決定（第 7 節 D4）。
3. 訊號與 control connection 關閉時，仍走現有的 `interrupt` 路徑（移除暫存檔後立刻結束），`Stop` 不取代它。

**涉及的項目**：PRO-00、PRO-04、PRO-05、PRO-08、PRO-14、SRC-02、SRC-03、DST-05（finisher 的 `?`）、DST-11（失敗時沒有還原放寬的權限）、TRN-17、TRN-18。

### A2. 錯誤分級沒有明確的規則

**現況。** 哪些錯誤只讓一個項目失敗、哪些讓整個傳輸中止，目前散落在各處，彼此不一致：

- 單一目錄打不開，或一個 stat 失敗，整個傳輸中止（DST-04）。
- 一個目錄收尾失敗，之後的目錄全部不收尾（DST-05）。
- 擁有者或權限的 `EPERM`，已驗證的檔案被整個丟掉，與設計文件寫的「略過並警告，檔案照樣傳」相反（DST-06）。
- 掃描期間消失的項目被算成「無法讀取」，結束狀態變成 1（SRC-14）。
- 需要的檔案沒有送來，卻算成功（PRO-06）。

**建議的規則**，寫進 `docs/design/pipeline.md` 的「Principles」：

| 類別 | 例子 | 處理 |
|---|---|---|
| 單一項目的失敗 | 某個檔案或目錄開不了、讀不了、stat 失敗、寫入失敗、雜湊不符、改名失敗 | 回報 `Results::Failed`，計入 failed，繼續。結束狀態非 0 |
| 權限不足、無法保留的屬性 | 非 root 時的 chown、群組；FAT 或 SMB 上的 chmod、utimens；root_squash | 依種類彙總成一則警告，檔案照樣就位。不算 failed |
| 整個傳輸中止 | 協定違規、manifest 檢查失敗、connection 或串流錯誤、根目錄本身不能用、本 peer 的內部錯誤 | 經由 A1 的機制停止，只回報第一個原因 |
| 待決定 | `ENOSPC`、`EIO`、`EROFS` | 第 7 節 D6 |

**涉及的項目**：DST-04、DST-05、DST-06、DST-14、SRC-14、PRO-06、CLI-06（結束狀態要能區分「部分失敗」）。

> 2026-10-20 補記：WP-07 把這張規則表寫進了 `pipeline.md` 的「Principles」（中文版「原則」）。D6 照建議：`ENOSPC` 與 `EROFS` 停止整個傳輸，`EIO` 只讓該項目失敗；與 D6 不同之處，`EDQUOT`（配額用盡）也停止傳輸，因為它與 `ENOSPC` 一樣，之後的項目都會同樣失敗。停止的地方：寫入、改名、fsync、建立目錄、套用項目、收尾與 `sync` 的刪除。測試：`a_full_or_read_only_drive_ends_the_transfer` 只測分類；磁碟真的滿了的端到端情境沒有自動測試。PRO-06 與 CLI-06 不在 WP-07。

### A3. 資源預算不完整

佇列已經有上限，但還有三種預算沒有明訂，也沒有測試：

1. **fd 預算。**
   - 程式從不調高 `RLIMIT_NOFILE`。
   - Destination 的每個 `Ready` 占 3 個 fd，同一目錄的檔案也各自持有目錄 fd（DST-03）。
   - Source 的掃描每層祖先占一個 fd（SRC-07）。
   - 建議：啟動時把軟上限調到硬上限；以 `Arc<OwnedFd>` 或 `Weak` 共用目錄 fd；依 fd 預算縮小批次。
2. **訊息位元組預算。**
   - 每個 packet 的上限是 16 MiB，但 `Needs::Deletes` 不分段（PRO-01），manifest 的分段只看筆數，不看位元組（PRO-03）。
   - 建議：凡是長度取決於樹內容的訊息，都同時以筆數與位元組分段，比照 `Report::Listed` 的 `LISTED_BYTES`。
   - 再加一個測試：任何訊息都不會超過上限。
3. **在途數量預算。**
   - backlog 上限是 10 萬，實測卻到 825,925（SRC-04）。striped 接收端的 early buffer 以片段數為上限，而不是位元組（TRN-16，與 PRO-15 相同）。Profile report 無上限累積（PRO-09）。
   - 建議：以「已送出的 ID 減去最後一個 `Needs::Through`」來節制掃描；早到的片段以位元組為上限。

**涉及的項目**：DST-03、SRC-07、PRO-01、PRO-03、SRC-04、TRN-16、PRO-15、PRO-09、PRO-13、DST-16。

### A4. 準備階段的實作與設計不一致

`docs/design/pipeline.md` 的「Phase 1: Prepare」寫的順序是：

1. 開啟所有 SSH connection。
2. 探測並安裝每個 host。
3. 啟動兩個 server。
4. 開啟 connection。

程式實際上是：Source 側完整啟動（含 server 與 heartbeat 監看），**之後**才對 Destination 側探測與安裝（`cli.rs:455-470` 依序呼叫 `start`，而探測與安裝在 `start` 內的 `remote::program`）。結果是 TRN-01。

安裝本身也不夠穩健：

- 上傳沒有驗證（TRN-02）。
- 上傳沒有逾時，也沒有 heartbeat（TRN-20）。
- 探測的輸出沒有加標記，可能被啟動檔印出的內容干擾（TRN-19）。
- 一個 client 剛探測到的 build，可能被另一個 client 的 `keep_recent` 刪掉（TRN-21）。

**建議**：照 TRN-01 的 A 加 B 做。A 是 connection 一建立就送 heartbeat；B 是把兩側的探測與安裝都提到啟動任何 server 之前，讓程式符合設計文件。再照 TRN-02 讓安裝腳本驗證大小與 SHA-256，用唯一的暫存名稱，並讓 probe 實際執行 `--version`。

**涉及的項目**：TRN-01、TRN-02、TRN-17、TRN-19、TRN-20、TRN-21、TRN-22。

### A5. 兩邊的根目錄沒有「身分」

**現況。**

- client 只知道兩邊的根目錄是 directory、other 還是 missing。
- 每個 server 只知道自己的根目錄，所以沒有人能判斷兩邊是不是同一個目錄、會不會互相包含（CLI-01）。
- `host:` 會被默默換成家目錄，`~` 也不會展開（CLI-04）。
- 根目錄是指向檔案的 symlink 時，比較會跟隨它，套用卻只跟隨一部分（DST-13）。

**建議**：server 在握手時回報 `RootIdentity`。它包含：

- kernel 的 `boot_id`，用來判斷兩邊是不是同一台機器；
- 根目錄的 canonical path；
- 根目錄與每層祖先的 `(dev, ino)` 鏈；
- 這個 server 代表的使用者的家目錄；
- 是否為網路檔案系統。

client 依規則表拒絕重疊，並在 `sync` 進 `/` 或家目錄時先詢問。完整設計見 **CLI-01**，其中的規則需要使用者決定（第 7 節 D1 至 D3）。`docs/design/overview.md` 的「Guarding against a mistyped DESTINATION」也要從「待決定」改為「已決定」（DOC-01）。

### A6. Destination 的狀態太集中，而且一再以路徑重新走訪

**現況。**

- `destination.rs` 有 2,643 行，主要狀態全放在一個 `Mutex<State>` 裡，有 25 處 `lock()`。
- 這把鎖在路徑走訪、套用目錄、執行 `getent` 時一直被持有，16 個 writer 的路徑走訪因此互相排隊。在 NFS 或 Lustre 上，這會明顯變慢（DST-09）。
- 每個需要的檔案都重新以路徑從根目錄走一次：Destination 的 `target_directory` 與 Source 的 `open_source_file` 都是（SRC-06）。Source 每個檔案都重新以路徑解析根目錄。根目錄的 symlink 在執行途中被換掉時，後面的檔案會從另一棵樹讀。
- 比較完的目錄，要等下一個 packet 到達才套用、才送出 needs（PRO-07）。

**建議**：先完成正確性的修正，再重構，不要混在同一個工作包。方向如下：

1. 以目錄 fd 為單位快取：`HashMap<Vec<u8>, Weak<OwnedFd>>`，與 DST-03 的第 2 部分是同一份工作。Source 保留掃描時的根目錄 fd。
2. 把路徑走訪與 `getent` 移到鎖外。
3. 把 `destination.rs` 拆成「接收與比較套用」、「寫入與改名」、「目錄收尾」三個模組，`State` 也依職責拆開。
4. 若採用 PRO-07 的「獨立的 apply thread」，它也要接上 A1 的 `Stop`。

**涉及的項目**：DST-09、DST-16、DST-17、SRC-06、SRC-11、PRO-07、PRO-13。

### A7. 信任邊界：另一個 peer 與 host 都不可信

**原則。** 拉取時，來自 host 的 manifest、record、report 都可能是惡意的或損壞的。推送時，本機的 client 會在 host 上執行它下載的 binary，用 `--remote-sudo` 時是以 root 執行。

**現況。** 做得好的部分：packet 長度上限、manifest 檢查、`one_name`、zstd 解壓上限，見第 9 節。仍有的缺口：

- **目的地與 peer 輸入**
  - chmod 跟隨 symlink（DST-01）。
  - manifest 不要求深度優先的順序（DST-08）。
  - `getent` 會把以 `-` 開頭的名稱當成選項（DST-10）。
  - mtime 的奈秒沒有檢查（DST-15）。
  - client 會被 server 送來的狀態編號弄到 panic（PRO-09）。
  - 同一 ID 的第二個 record 會被接受（PRO-12）。
  - `Needs::Through(u64::MAX)` 會溢位（PRO-11，與 SRC-16 相同）。
- **安全測試**：解碼器沒有 fuzz 測試（TST-03）。
- **供應鏈**
  - 沒有簽章（TRN-03）。
  - action 以可變的 tag 固定（CI-03）。
  - 推 tag 就 release，不要求該 commit 的 CI 已通過（CI-05）。
  - 沒有 cargo-deny（CI-02）。
  - macOS 的 binary 沒有公證（CI-06）。
- **行程衛生**
  - 收到的 fd 沒有設 close-on-exec（TRN-09）。
  - 密碼留在環境變數中（TRN-10、TRN-24）。
  - `/tmp` 下的目錄名稱可以預測（TRN-07）。

**建議**：TST-03 的 fuzz target 直接驅動 `destination::run`，斷言兩件事：絕不 panic、絕不寫到根目錄之外。它是這條邊界的長期安全網，應該排進 CI，給一個固定的時間預算。

### A8. 可觀測性與測試基礎設施

**現況。**

- 「60 秒沒有東西經過就印出每條 thread 的位置」的機制，在真正卡住時不會觸發，因為每 250 ms 的 Queues report 也算「有東西經過」（SRC-05）。
- 儀表板開著時，Ctrl-\ 沒有作用（SRC-08，與 CLI-09 相同）。
- 會卡住的測試沒有期限（TST-04、CI-01、TRN-11）。
- 需要 root 的行為從沒在 CI 跑過（TST-02）。

**建議**：

1. CI 改用 nextest，設 `slow-timeout` 與 `terminate-after`（CI-01）。
2. 新增一套只在 debug build 生效的故障注入點，例如 `FERRY_TEST_PANIC=<role>`、`FERRY_TEST_FAIL=<point>`（設計見 SRC-03），讓 A1 與 A2 的修正都能用整合測試驗證。是否接受這種隱藏開關，見第 7 節 D12。
3. 加一個以 root 身分在容器中執行的 CI job（TST-02）。

### 不建議改的地方

- **remote-to-remote 時資料經過 client 兩次。** 這是設計文件寫明的取捨：兩台主機不必互通，也不必在任一台放憑證。量測也顯示 relay 不是瓶頸。保留。
- **client 與 server 必須是同一個 build。** 這讓協定可以自由修改。保留。
- **一個 lister 加 8 個 stat。** 這是在 NFS 上量測後的選擇。保留。
- **Rust 加同步 thread，不換成 async；使用系統的 `ssh`。** 理由見 [architecture.md](architecture.md) 的「不建議改的地方」。

---

## 6. 工作分包與執行順序

**原則。**

- 一個工作包大致是一個 PR，可以有數個 commit。
- `src/destination.rs` 被很多工作包修改。這些工作包請**依序**做，不要平行，以免衝突。可以平行的組合，寫在各波的說明裡。
- 先做正確性，再做效能與重構。
- 架構方案（ARC-*）的完整內容在 [architecture.md](architecture.md)。採用 ARC-02 加 ARC-03 時，原本的 WP-06a、WP-06b 與 WP-17 的一部分由它們取代：PRO-00 直接寫在新結構裡，不要先在舊結構上做一次。使用者若不採用 ARC-02、ARC-03，就改做表中標為「備案」的工作包。

### 第一波：先止血。WP-01、WP-02 → WP-03、WP-15a 可以平行

| 工作包 | 項目 | 主要檔案 | 相依與備註 |
|---|---|---|---|
| **WP-01 防止刪錯資料** | CLI-01、CLI-04、DOC-01 | `cli.rs`、`protocol.rs`（`Welcome`）、`peer.rs`、`session.rs`、新增的 `guard.rs`、`fsys.rs`、`remote/location.rs`、文件 | 先請使用者決定 D1 至 D3。CLI-04 的 `~` 規則會影響 CLI-01 的家目錄判斷，所以一起做 |
| **WP-02 chmod 不跟隨 symlink** | DST-01 | `fsys.rs`、`destination.rs` | 小。Linux 用 `O_PATH` 加 `/proc/self/fd`，macOS 用 `AT_SYMLINK_NOFOLLOW` |
| **WP-03 暫存檔一定清掉** | DST-02（＝PRO-02） | `interrupt.rs`、`destination.rs` | `Registered` 在 drop 時刪檔，改名成功後才解除。macOS 與 ARC-01 的退回路徑都需要它，所以採用 ARC-01 也要做 |
| **WP-15a 測試的安全網** | CI-01、TST-01、TST-04 | `.github/workflows/ci.yml`、`Makefile`、`destination.rs` 的 tests、`tests/*.rs` | nextest 的逾時讓之後每個工作包卡住時，都看得出是哪個測試 |

### 第二波：卡死、大量失敗，以及 Destination 的重構

| 工作包 | 項目 | 主要檔案 | 相依與備註 |
|---|---|---|---|
| **WP-04 fd 預算** | DST-03 的第 1、3 部分（啟動時調高 `RLIMIT_NOFILE`、依預算縮小批次、登記表改用 `Arc<OwnedFd>`）、SRC-07 | `cli.rs`、`destination.rs`、`interrupt.rs`、`scan.rs` | 依賴 WP-03。macOS 預設的上限是 256，很急，所以先做。DST-03 的第 2 部分（共用目錄 fd）併入 ARC-03 的 `DirCache` |
| **WP-05 Source 讀檔安全** | SRC-01、SRC-10 | `fsys.rs`、`source.rs` | 開檔時加 `O_NONBLOCK`，用 fstat 確認是一般檔案，再清掉 `O_NONBLOCK`；可選擇加 `O_NOATIME`。SRC-06 併入 ARC-03。可以和 WP-04 平行 |
| **WP-09 準備順序與安裝的完整性** | TRN-01、TRN-02、TRN-17、TRN-19、TRN-20、TRN-21、TRN-22 | `cli.rs`、`remote/install.rs`、`remote/probe.rs`、`remote/mod.rs`、`transport/mod.rs` | 先請使用者決定 D11。動到 `cli.rs`，和 WP-01 錯開或在它之後做。可以和下一列平行 |
| **WP-ARC 停止機制與 Destination 的重構** | ARC-02、ARC-03。吸收 PRO-00、PRO-04、PRO-05、PRO-06、PRO-07、PRO-08、PRO-14、SRC-02、SRC-03 中 file sender 的部分、TRN-18、DST-03 的第 2 部分、DST-09、DST-11、DST-16、DST-17、SRC-06 | 新增的 `stop.rs`、`stage.rs`、`dirs.rs`；`destination.rs` 拆成 `destination/` 底下的模組；`pool.rs`、`source.rs`、`relay.rs`、`Cargo.toml` | 先請使用者決定 D15（crossbeam-channel）。依賴 WP-03、WP-04。依 architecture.md 中 ARC-03 的七個步驟，每步一個 commit。這是最大的工作包，可以由同一個 agent 分數次完成 |
| **WP-06c panic 政策** | SRC-03 中 panic 的部分 | `Cargo.toml`、`cli.rs`（hook）、`interrupt.rs`（`exit_now`）、`profile.rs` | 先請使用者決定 D4。與 WP-ARC 的 `Stages` 互相配合：`Stages` 在測試中以 `catch_unwind` 接住 panic；release build 是否改成 `panic = "abort"` 由 D4 決定 |
| WP-06a 停止機制（Destination）（備案） | PRO-00、PRO-04、PRO-05、PRO-06、PRO-08、DST-11 | 新增的 `stop.rs`、`destination.rs`、`peer.rs`、`pool.rs`、`mux.rs` | 只在不做 WP-ARC 時才做 |
| WP-06b 停止機制（Source 與 client）（備案） | SRC-02、SRC-03 中 file sender 的部分、PRO-14、TRN-18 | `source.rs`、`scan.rs`、`relay.rs` | 只在不做 WP-ARC 時才做。依賴 WP-06a |

### 第三波：寫入 stage、錯誤分級、訊息大小、SSH 相容性、輸出

| 工作包 | 項目 | 主要檔案 | 相依與備註 |
|---|---|---|---|
| **WP-ARC-01 寫入 stage** | ARC-01（01a 至 01d） | `fsys.rs`、`destination/write.rs`、`destination/durable.rs`、文件與 `pipeline-workers.svg` | 先請使用者決定 D14。依賴 WP-ARC（在新結構上做）。01a 與 01b 先做；01c 與 01d 先量測再決定。每一步都附前後的量測數字 |
| **WP-07 錯誤分級** | DST-04、DST-05、DST-06、DST-14、SRC-14 | `compare.rs`、`destination/`、`owners.rs`、`scan.rs` | 依賴 WP-ARC，或備案的 WP-06a。先把第 5 節 A2 的規則寫進 `pipeline.md`，並請使用者決定 D6 |
| **WP-08 訊息位元組預算** | PRO-01、PRO-03 | `destination/`、`source.rs`、`scan.rs`、`protocol.rs` | `Needs::Deletes` 建議直接拿掉，因為 Source 根本沒用到它。PRO-01 會在 `sync` 時讓傳輸失敗：若 WP-ARC 會拖很久，可以先在舊結構上做 |
| **WP-10 SSH 相容性** | CLI-02、TRN-04、TRN-06、TRN-08、TRN-12、TRN-13（＝CLI-05）、TRN-14、DOC-06 的 `FERRY_SSH` 部分 | `transport/ssh.rs`、`transport/mod.rs`、`remote/shell.rs`、`remote/location.rs` | TRN-04 的測試需要在 CI 安裝 tcsh。和 WP-ARC 不衝突，可以平行 |
| **WP-11 行程與權限的衛生** | TRN-05、TRN-07、TRN-09、TRN-10、TRN-24、SRC-12 | `interrupt.rs`、`transport/*`、`remote/sudo.rs`、`home.rs` | 子行程登記表：結束時殺掉 ssh |
| **WP-13 輸出與結束狀態** | CLI-03、CLI-06、CLI-07、CLI-08、CLI-10、DOC-05、DOC-06 | `cli.rs`、`report.rs`、`progress.rs`、`update.rs`、文件 | 先請使用者決定 D7、D8。CLI-03 的跳脫規則要套用到 dry run 清單、警告與儀表板 |
| **WP-14 可觀測性** | SRC-05、SRC-08（＝CLI-09）、SRC-17、PRO-09、ARC-05b | `relay.rs`、`dashboard.rs`、`control.rs`、`profile.rs`、`destination/finish.rs` | 小而獨立 |
| **WP-ARC-05 小的結構調整** | ARC-05a、ARC-05c | `cli.rs`、`source.rs`（壓縮）、各 pool 的 thread 數 | 先請使用者決定 D17、D18。05c 等 WP-ARC-01 之後再量測 |

### 第四波：供應鏈、測試深度、效能、雜項

| 工作包 | 項目 | 主要檔案 | 相依與備註 |
|---|---|---|---|
| **WP-12 發布供應鏈** | TRN-03（＝CI-07）、CI-02、CI-03、CI-04、CI-05、CI-06、TRN-15、DOC-09 的 SECURITY.md | `release.yml`、`ci.yml`、`remote/release.rs`、`update.rs`、`scripts/install.sh`、新增的 `deny.toml`、`.github/dependabot.yml` | 先請使用者決定 D5（簽章方式與私鑰保管）。CI-02 與 CI-03 不需要決定，可以早點做，甚至提前到第一波 |
| **WP-15b 測試的深度** | TST-02、TST-05、TST-06、TST-07、TRN-11 | `tests/*.rs`、`scripts/container-test.sh`、`ci.yml` | TRN-11 是調查計畫：先加 watchdog，讓它失敗時有名字 |
| **WP-16 fuzz** | TST-03 | 新增的 `fuzz/`、`ci.yml` | 在 WP-ARC 與 WP-07 之後做，否則 fuzz 會先找到已知的問題 |
| **WP-17 記憶體與效能（剩下的部分）** | SRC-04、SRC-15、PRO-13、TRN-16（＝PRO-15）、TRN-23 | `source.rs`、`frame.rs`、`transport/striped.rs` | PRO-07、DST-09、DST-16、DST-17 已併入 WP-ARC，SRC-11 併入 ARC-04。每一項都先用 `ferry test --scale` 量測，改完再量一次 |
| **WP-ARC-04 大檔分段** | ARC-04（吸收 SRC-11） | `protocol.rs`、`record.rs`、`source.rs`、`destination/write.rs` | 先依 architecture.md 中 ARC-04 的量測計畫量測，再請使用者決定 D16 |
| **WP-18 文件修正** | DOC-02、DOC-03（與 SRC-13 一起，先請使用者決定 D9）、DOC-04、DOC-07、DOC-08、DOC-09 的 CHANGELOG | `TODO.md`、`docs/design/overview*.md`、`README.md`、`docs/cli*.md` | 純文件，隨時可做 |
| **WP-19a Destination 雜項** | DST-07、DST-08、DST-10、DST-12、DST-13、DST-15、DST-18、DST-19 | `destination/`、`manifest.rs`、`names.rs`、`fsys.rs` | DST-19 是否跨掛載點，先請使用者決定 D10 |
| **WP-19b 協定雜項** | PRO-10、PRO-11（＝SRC-16）、PRO-12 | `session.rs`、`source.rs`、`destination/` | 小 |
| **WP-20 新功能（使用者決定後才做）** | SRC-09 的 `-x`／`--one-file-system` | `cli.rs`、`scan.rs`、`compare.rs` | D10 |

### 一覽

- 第一波：WP-01；WP-02 → WP-03；WP-15a。
- 第二波：WP-04（在 WP-03 之後）；WP-05；WP-09（在 WP-01 之後）；WP-ARC（在 WP-03、WP-04 之後）；WP-06c（與 WP-ARC 協調）。
- 第三波：WP-ARC-01 與 WP-07（都在 WP-ARC 之後）；WP-08；WP-10；WP-11；WP-13；WP-14；WP-ARC-05。
- 第四波：WP-12；WP-15b；WP-16；WP-17；WP-ARC-04（先量測）；WP-18；WP-19a；WP-19b；WP-20。

每一項都被分配到上表的某個工作包。若發現遺漏，以各檔案的總表為準。

---

## 7. 需要使用者決定的事項

下列每一項都會改變使用者看得到的行為，或牽涉安全上的取捨。執行的 agent 請先問，不要自己決定。

**已決定（2026-10-18）**：D1、D2、D3、D11 都照「審查者的建議」那一欄決定。準備階段的另外三個問題（Q1 至 Q3）與這四項的完整內容，記在 [docs/drafts/preparation.md](../../drafts/preparation.md) 的「待決定的問題」。WP-01 另外三個小選擇，同日也已決定：新旗標的名稱就用 `--allow-dangerous-destination`；CLI-01 規則表中「網路檔案系統上推測為重疊」那一列這次不做，在文件中列為已知的限制，等有 NFS 可以量測再加；CLI-01 第六部分的 `--max-delete` 這次不做。WP-05 的 SRC-01 也照使用者的決定，與該項的建議不同：開檔前不先 `stat`，只以不阻塞的方式開啟，開啟後 `fstat`，不是一般檔案就關掉並回報無法讀取。理由：奇怪的檔案極少，開檔前的檢查幾乎派不上用場，卻在 NFS 上讓每個檔案多一次往返；FIFO 卡住的問題，不阻塞的開啟加上 `fstat` 已經完全解決。剩下的風險是開到一個被換進來的裝置檔，那需要 root 才能建立，而且開了就關、不讀內容。在 fatcat 的 NFSv4.1 上量測（2026-10-18，10 萬個 4 KiB 檔案，16 條 thread，掃描後 65 秒才讀，屬性快取已過期）：開檔前多一次 `stat`，送到伺服器的 GETATTR 從約 1 萬次增為約 10 萬次，讀檔從約 2.07 秒增為 2.9 到 3.5 秒；不阻塞的開啟加 `fstat`、`fcntl`，與現在的程式一樣快。WP-09 用到的 D12 部分，同日照建議決定：加隱藏的 `ferry build-id` 子命令（release build 也有，安裝腳本要用它試執行）；測試用的 heartbeat 時間開關 `FERRY_TEST_HEARTBEAT` 以 `cfg!(debug_assertions)` 限定，release build 忽略它。D12 的其餘部分（`FERRY_TEST_PANIC`、`FERRY_TEST_FAIL`，以及既有的 `--cut-a-path` 要不要從 release build 移除）仍待決定。（2026-10-18 補記：WP-ARC 與 WP-06c 開工前，D15、D4 與 D12 的注入開關都照建議決定：新增 crossbeam-channel；panic 政策採 SRC-03 的選項 B，release 與 dev 都設 `panic = "abort"`，hook 還原終端機、印出 `ferry: internal error in <role>`、盡量告知 client、移除暫存檔，以 `exit(101)` 結束，結束狀態表新增 101；`FERRY_TEST_PANIC=<role>` 與 `FERRY_TEST_FAIL=<point>` 以 `cfg!(debug_assertions)` 限定。D12 只剩 `--cut-a-path` 是否從 release build 移除待決定。）（2026-10-18 補記：WP-06c 照 D4 與 SRC-03 的選項 B 實作，與 SRC-03 的建議不同之處：server 告知 client 的方式，是在 control connection 上送 `Report::Said`，由 client 顯示，而不是 `Control::Error`；告知不了 client 時，才自己寫到 stderr。server 以 101 結束時，client 從它的結束狀態得知，也以 101 結束，而且這個原因優先於 path 中斷之類的後果。同時有好幾條 thread panic 時，只有第一條說明並結束行程。`FERRY_TEST_PANIC=<role>` 讓那個 role 的每個 stage 在開始時 panic，所以涵蓋所有 stage，包括 pool 的 worker 與 client 的 relay。SRC-03 驗收條件中「client 因 panic 結束後，server 不留下暫存檔」只核對了程式碼：server 在 control connection 關閉時以 `interrupt::stop` 結束並移除暫存檔，但沒有測試；「儀表板開著時 panic」也沒有測試。）（2026-10-18 補記：WP-ARC-01 開工前，D14 照建議決定：Linux 上以沒有名稱的暫存檔（`O_TMPFILE` 加 `linkat`）取代 `.name.ferry-XXXX`，macOS、NFS 與不支援的檔案系統退回現在的做法。使用者決定先做 01b，再做 01a；進度記在 architecture.md 的 ARC-01。）（2026-10-20 補記：WP-ARC-01 完成後，D6、D7、D8、D17、D18 都照「審查者的建議」那一欄決定：D6 是 `ENOSPC` 與 `EROFS` 中止整個傳輸，`EIO` 只讓該項目失敗；D7 是重新編排結束狀態，並在 release notes 註明是行為變更；D8 是「where the time went」預設隱藏，建議中的幾種顯示方式，在 WP-13 實作時再提出具體做法；D17 是兩側都在本機時預設不壓縮，連續壓不下去的 frame 暫停嘗試一段時間；D18 是在 ARC-01 之後先量測，再決定要不要提供選項，量測要涵蓋 01d 新增、固定為 64 條的 fsync thread。）（2026-10-20 補記：WP-13 實作 D8 時，選擇預設隱藏、新增 `--profile` 旗標才顯示；「縮短成一行」沒有採用，見 CLI-08 的補記。D7 的碼表照 CLI-06，讀取期間消失的路徑（SRC-14）也歸入 24。）（2026-10-20 補記：D17 的後半「連續壓不下去的 frame 暫停嘗試一段時間」實作後，使用者決定拿掉：檔案傳輸中 CPU 不會是瓶頸。前半「兩側都在本機時預設不壓縮」保留。）（2026-10-20 補記：D18 量測之後，使用者決定預設只用 8 個核心，並提供選項。實作為 `--cpus N|all`，限制的是核心（Linux 的 CPU affinity），不是建議中的 thread 數；沒有依 cgroup 的配額自動決定。細節與量測見 architecture.md 的 ARC-05c。）（2026-10-21 補記：D5 討論後，參考 Herdr 的做法（沒有簽章，也沒有 macOS 的簽章與公證，但開了 immutable releases、Actions 以 commit SHA 固定），使用者決定：不做簽章，與建議的 minisign 不同；也不開 GitHub 的 immutable releases（開了之後，發布的附檔與 tag 都不能改，刪掉 release 後同一個 tag 名稱也不能再用，發錯只能跳過那個版本號）；做 CI-03。對照表中的另外三點留待日後討論：固定 Rust toolchain（CI-04）、workflow 權限是否最小、誰能發布（限定 repo admin 推的 tag，以及 CI-05 的「該 commit 的 CI 已通過」）。CI-02、TRN-15、CI-06 與文件中註明沒有簽章，不在這次的討論中。）（2026-10-22 補記：D10 使用者選 A，完全比照 rsync：預設照舊跨過掛載點，並寫進文件；新增 `-x`／`--one-file-system`，掃描不進入掛載點，`sync` 不在另一個檔案系統上刪除；stat 一律帶 `AT_NO_AUTOMOUNT`。與建議相同，細節與不同之處見 source.md 的 SRC-09 與 destination.md 的 DST-19。同日也確認 DST-13 的根目錄連結各種組合都與 rsync 一致，只有「檔案複製進既有目錄前先詢問」是 Ferry 刻意的不同，使用者決定維持。）（2026-10-22 補記：D9 使用者選 A：只改文件，程式不改，結果與 rsync 一致。以 rsync 3.2.7 實測過，rsync 同樣偵測不到同一秒內、大小不變的改寫，也不警告。見 SRC-13 與 DOC-03 的補記。）（2026-10-22 補記：D12 剩下的部分，使用者決定 `--cut-a-path` 與其他隱藏的測試旗標都留在 release build：`ferry test` 的斷線情境需要它，而它只讓帶著它的那次傳輸失敗。見 CLI-10 的補記。D12 已全部決定。）（2026-10-22 補記：D13 照建議決定：CHANGELOG 用 Keep a Changelog 格式，release notes 取自它；漏洞回報用 GitHub 的 private vulnerability reporting，已照使用者的要求開啟；兩個檔案只寫英文版。見 DOC-09 的補記。）（2026-10-22 補記：D16 使用者決定先不做，也不量測：沒有比約 1.2 GB/s 更快的網路，好處只剩本機複製大檔；續傳另作為之後的功能。見 architecture.md 的 ARC-04。D1 至 D18 至此都已決定。）（2026-10-23 補記：D16 在架構重新設計中重議，使用者決定做：資料改成 block（pack 與 segment）。理由不是吞吐量，而是 block 可以單獨重送、不互相阻塞、續傳不必再改格式；端到端確認與重送也同時決定做。見 [docs/drafts/architecture-redesign.md](../../drafts/architecture-redesign.md)。）其餘各項仍待決定。

| 編號 | 問題 | 審查者的建議 | 相關項目 |
|---|---|---|---|
| D1 | 根目錄重疊時要怎麼處理？Source 在 Destination 裡時，`copy` 也拒絕嗎？重疊可以用旗標覆寫嗎？ | 三種重疊（相同、Destination 在 Source 裡、Source 在 Destination 裡）對 `copy` 和 `sync` 一律拒絕，不提供覆寫旗標 | CLI-01 |
| D2 | `sync` 進 `/`、家目錄、家目錄的祖先，或空的遠端路徑時，要怎麼處理？ | 在終端機上詢問；沒有終端機時拒絕，除非給了新旗標（暫名 `--allow-dangerous-destination`）。`--yes` **不**涵蓋這一類，因為腳本已經用它同意安裝。`--dry-run` 只警告 | CLI-01 |
| D3 | `host:~/x` 要比照 rsync 和 scp 展開 `~` 嗎？`~user` 呢？ | `~` 與 `~/` 在 client 端正規化成相對於家目錄的路徑；`~user` 先明確拒絕，或由 server 展開 | CLI-04 |
| D4 | panic 的政策是什麼？ | `panic = "abort"`，加上 hook：還原終端機、印出訊息、移除暫存檔，以 101 結束。文件的結束狀態表新增 101 | SRC-03 |
| D5 | release 的簽章要用什麼方式？私鑰怎麼保管？要不要啟用 immutable releases？ | minisign，把公鑰編進 binary；私鑰放在 GitHub Environment 的 secret，設定 required reviewers；舊版 client 不驗簽，所以要規劃過渡 | TRN-03、CI-05 |
| D6 | `ENOSPC`、`EIO`、`EROFS` 等錯誤，算單一項目失敗，還是整個傳輸中止？ | 審查者沒有定論。彙整者的建議：`ENOSPC` 與 `EROFS` 中止整個傳輸，因為後面的項目必然也會失敗；`EIO` 只讓該項目失敗 | A2、DST-04 至 DST-06 |
| D7 | 結束狀態要不要重新編排？例如「有新版」改成 3，部分失敗用 23 和 24，比照 rsync | 要改，並在 release notes 註明這是行為變更。這會影響使用者的腳本 | CLI-06、DOC-05 |
| D8 | 每次結束時印出的約 40 行「where the time went」，預設要不要隱藏？ | 預設隱藏，只在 `--profile-file` 或新旗標時顯示，或縮短成一行 | CLI-08 |
| D9 | 讀取期間被就地改寫、mtime 落在同一秒的檔案，下次執行偵測不到。只改文件，還是對接近掃描時間的 mtime 改用奈秒比較（git 處理 racy 的做法）？ | 至少要改文件，修正過度的承諾。加上偵測更好 | SRC-13、DOC-03 |
| D10 | 要不要加 `-x`／`--one-file-system`？`sync` 刪除多餘的目錄時，要不要進入其中的掛載點？ | 加 `-x`，掃描時帶 `AT_NO_AUTOMOUNT`；刪除時不跨掛載點 | SRC-09、DST-19 |
| D11 | 準備的順序要不要改成「兩側都探測、安裝完，才啟動任何 server」？這會改變使用者看到的提示順序 | 先做 TRN-01 的 A（connection 一建立就送 heartbeat），再做 B（重排順序） | TRN-01 |
| D12 | 只在 debug build 生效的故障注入開關（`FERRY_TEST_PANIC`、`FERRY_TEST_FAIL`）、隱藏的 `ferry build-id` 子命令，可以接受嗎？目前隱藏的測試鉤子（例如 `--cut-a-path`）也會進 release build | 注入開關以 `cfg!(debug_assertions)` 限定；既有的隱藏旗標是否從 release build 移除，請使用者決定 | SRC-03、TRN-01、CLI-10 |
| D13 | CHANGELOG 的格式與維護方式？SECURITY.md 的回報管道（email 或 GitHub private advisory）？ | 用 Keep a Changelog 的格式；回報管道用 GitHub 的 private vulnerability reporting | DOC-09 |
| D14 | Linux 上要不要改用沒有名稱的暫存檔（`O_TMPFILE` 加 `linkat`），取代 `.name.ferry-XXXX`？這會改寫 overview 中已是「Decided」的暫存檔設計 | 要。量測顯示同一目錄的鎖競爭是小檔情境的主要成本；`kill -9` 也不會留下暫存檔。macOS、NFS 與不支援的檔案系統，退回現在的做法 | ARC-01 |
| D15 | 要不要新增依賴 crossbeam-channel，取代 std 的 `mpsc`？ | 要。需要它的 `select!` 與 MPMC。std 的 `mpsc` 從 Rust 1.67 起，內部就是它的實作 | ARC-02 |
| D16 | 大檔要不要改成分段的 record？要不要支援中斷後續傳（需要有名稱的暫存檔，與 D14 互斥，只用在大檔上）？ | 先依 ARC-04 的量測計畫量測工作負載再決定 | ARC-04、SRC-11、DOC-07 |
| D17 | 兩側都在本機時，預設不壓縮嗎？ | 是。另外，連續壓不下去的 frame 暫停嘗試一段時間 | ARC-05a |
| D18 | 要不要提供限制 thread 數的選項（例如 `--threads`），或依 cgroup 的 CPU 配額自動決定？ | 先等 ARC-01 做完再量測；系統時間降下來後，可能就不需要 | ARC-05c |

---

## 8. 重複與交叉參照

以下是不同審查者各自找到的同一個問題。做的時候一起處理；修法以「主項」為準，「副項」補充證據或文件同步的地方。

| 主項 | 副項 | 說明 |
|---|---|---|
| DST-02 | PRO-02 | 暫存檔殘留。PRO-02 補了端到端的實測（3.7 GB），以及 `docs/cli.md` 要同步的地方 |
| DST-03 | SRC-07 | fd 預算。調高上限是共同的修法；SRC-07 另有掃描祖先 fd 的部分 |
| PRO-00 | SRC-02、SRC-03 | 停止機制。`stop.rs` 兩邊共用 |
| TRN-03 | CI-07 | 簽章。CI-07 只是交叉參照 |
| TRN-13 | CLI-05、DOC-06 | `FERRY_SSH` 不做分詞 |
| SRC-08 | CLI-09 | raw mode 下的 Ctrl-\。CLI-09 另外提到 Ctrl-Z |
| SRC-16 | PRO-11 | `compared_through` 溢位 |
| TRN-16 | PRO-15 | early buffer 以片段數為上限 |
| SRC-13 | DOC-03 | 同一秒內的就地改寫偵測不到 |
| TRN-11 | TST-04、CI-01 | macOS 的偶發卡住，以及測試沒有期限 |
| CLI-01 | DOC-01 | 防呆設計，以及文件的「待決定」 |

---

## 9. 做得好的地方：修的時候不要改壞

下列不變式是現有設計的核心。修改時要保住，相關測試也必須繼續通過。

**路徑安全**

- Destination 的每一個操作，都從比較時開啟的根目錄 fd 出發，以 `openat` 加 `O_NOFOLLOW` 逐層走下去。以名稱操作前，先過 `fsys::one_name`。
- 多餘的項目，從比較時列出的那個目錄 fd 刪除，絕不以路徑重新解析，見 commit `d2d29ce`。
- `remove_tree` 與 `let_owner_in_at` 不跟隨 symlink。

**manifest 檢查**（`manifest::Check`）

- 拒絕空的名稱、`.`、`..`、含 `/` 或 NUL 的名稱。
- 拒絕沒有事先宣告的目錄、重複的目錄、亂序的名稱、不依序的 ID。

**刪除的安全**

- Source 讀不到的名稱，絕不刪除。
- Source 沒有完整列出的目錄，不比較，也不刪除其中的東西。
- 不分大小寫的檔案系統上，與讀不到的名稱互為別名的項目會保留，以 inode 判斷。
- 一個目錄的多餘項目，在建立它的任何東西之前就先刪除。

**完整性**

- 只有在 BLAKE3 相符，而且整個檔案都讀到時，才改名就位。
- 屬性的順序是：fchown、fchmod、futimens，然後 fsync，最後改名。
- 目錄由深到淺收尾。
- `Done` 在目錄 fsync 與 `make_durable` 之後才送出。
- macOS 上，每台磁碟機設一道屏障。

**長度上限**

- 每個 packet 最多 16 MiB。record 的標頭與 trailer 最多 64 KiB。
- zstd 解壓以宣告的原始長度為上限，最多 2 MiB，所以不怕解壓炸彈。
- postcard 依實際收到的位元組決定 `Vec` 的大小。

**shell 與 SSH**

- `shell::quote` 符合 POSIX sh。`assign` 以八進位跳脫，能安全傳遞任何位元組。
- token 是 128 位元，在 join 端驗證是十六進位。
- 主機名稱之前一律加 `--`，以 `-` 開頭的主機名稱會被拒絕。

**私有目錄**

- 以 `symlink_metadata` 加上 uid 與 mode 檢查，拒絕 symlink 與別人的目錄。
- sudo 的 socket 目錄屬於 root、模式 0711；socket 本身 chown 給使用者、模式 0600。

**訊號**

- 由專門的 `sigwait` thread 處理訊號，而不是在 signal handler 裡做事。
- 以 `GATE` 讓「建立並登記暫存檔」和清除互斥。
- 結束狀態是 128 加訊號編號。終端機一定會被還原。

**mux**

- 不會遺失喚醒：packet 先入列，才通知等待者。
- 有速率限制的送出者，在 pump 消失時會被放行。

**安裝與更新**

- `install.sh` 用 `set -eu`，以檔名完全比對 checksum，限定 `--proto-redir =https`，用 `mktemp -d` 並以 trap 清理。
- `ferry update` 在 binary 所在的目錄中，先寫到暫存名稱，再改名就位。

**CI**

- 頂層設 `permissions: contents: read`，每個 job 都有 timeout。
- 測試多種發行版與多種檔案系統（xfs、btrfs、ext4-casefold）。
- release 時檢查 tag 等於版本號、確認是靜態連結，並測試安裝腳本。

---

## 10. 長期：協定與功能的預留

這一節不是缺陷，是規劃時的參考。client 與 server 一定是同一個 build，所以改 record 或 manifest 的格式不需要相容舊版。這些功能可以等到要做時再改格式，不必現在預留欄位。

- **sparse 檔案**（rsync `-S`）。現在檔案中的洞會被讀成零。zstd 能把零壓得很小，所以網路上的成本不高；但 Destination 會把零實際寫出來，檔案變成完全配置。複製 VM 映像或資料庫檔案時，可能塞爆目的地的磁碟。做法：
  - Source 以 `SEEK_DATA` 與 `SEEK_HOLE` 找出資料區段，record 帶上區段表。
  - Destination 先 `ftruncate` 到完整大小，只寫資料區段。
  - BLAKE3 仍對整個邏輯內容計算，洞以零計入。若成本太高，就改成「資料區段加區段表」的雜湊。
  - 建議排在 hardlink 之前，因為它的風險（塞爆磁碟）比較直接。
- **hardlink**（rsync `-H`）。
  - Source 只對 `nlink > 1` 的檔案記錄 `(dev, ino) → 第一個 FileId`。
  - manifest 新增一種項目，例如 `Kind::Hardlink { first: FileId }`。
  - Destination 等第一個檔案改名就位後才建立連結。這要和批次改名、目錄收尾的順序協調。
  - 記憶體只隨 `nlink > 1` 的檔案數成長。
- **xattr 與 ACL**（rsync `-X`、`-A`）。
  - manifest 的項目要能帶可選的延伸屬性。
  - Linux 的 `security.*` 需要 root。
  - macOS 的 `com.apple.*` 與 resource fork 要決定保留哪些。
- **delta transfer**。這是 rsync 處理「大檔案只改了一點」時的優勢，例如 VM 映像、log、資料庫檔案。
  - 「由 Destination 決定要什麼」的架構正好適合：Destination 對已存在的檔案送出區塊簽章，Source 回送「沿用某區塊」或「字面資料」的指令。
  - 這會大幅改動 record 的格式與兩邊的 pipeline。建議先用 `ferry test --scale` 加上真實的工作負載，量測這類情境的比重，再決定是否要做。
- **`--checksum`**。設計文件已經提議了。它會讀遍兩邊的每個檔案，所以絕不會是預設。
- **斷線重連**。`TODO.md` 中已經暫時擱置。現在任何一條 SSH connection 斷掉，整個傳輸就失敗。

---

## 11. 實驗素材

重現問題與量測用的腳本、harness 與紀錄，都在 [`materials/`](materials/)，已納入版本控制。用法、每個檔案的內容與常用指令，見 [materials/README.md](materials/README.md)。

| 位置 | 內容 | 相關項目 |
|---|---|---|
| `materials/destination/exp/` | 以 path 依賴 ferry 的小 crate，把手工組出的 packet 直接餵給 `destination::run` | DST-02、DST-05、DST-08 |
| `materials/protocol/harness/` | 以手工組出的串流驅動 `destination::run` 與 `source::run`，有情境 A 至 G（含 `truncated`） | PRO-01 至 PRO-08 |
| `materials/transport/bin/fakessh` | 在本機執行一切的假 ssh，透過 `FERRY_SSH` 使用 | TRN-01、TRN-05 |
| `materials/transport/release_script.sh` | `release_script` 產生的安裝腳本，host 的目錄改成參數 | TRN-02 |
| `materials/source/` | `mk.py`（產生大型樹）、`repro-*.sh`、`poolpanic/`，以及 `records/` 中各次實驗的 profile JSON 與 stderr | SRC-01 至 SRC-07 |
| `materials/arch/` | `prof.py`、`dirbench.py`，以及 `profiles/` 中架構量測的 profile JSON | ARC-01、ARC-04、ARC-05 |

各檔案的每一項也都附有不依賴這些素材的重現方式或測試草稿。這些素材只是當時發現問題的工具與證據，修好之後的回歸測試，應依各項目寫進 `src/` 或 `tests/`。
