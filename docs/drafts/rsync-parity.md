# 與 rsync 的行為對照測試

- 狀態：草稿
- 開始：2026-10-22；最後修改：2026-10-22（計畫第 1 步的實測，N1 至 N3 已決定）
- 會改動：`docs/design/overview.md` 與中文版，加一節「與 rsync 不同之處」，列出刻意的不同；`docs/cli.md` 與中文版的 rsync 對照表，可能要補幾列
- 相關：審查項目 DST-13、DST-06、SRC-09、SRC-13；決定 D9、D10、CLI-10 第 5 點；記憶中的使用者偏好「與 rsync 一致」

## 問題

使用者希望：兩邊功能重疊的地方，Ferry 的結果與 rsync 相同，除非是刻意的不同。現在沒有任何東西會檢查這一點：

- 測試裡有不少「as rsync does」的註解，例如結束狀態 23／24、根目錄是指向檔案的 symlink、`-x`、檔案複製到目錄上。但每個測試的預期結果都寫死在測試裡，代表的是寫測試的人對 rsync 的理解，測試本身沒有執行 rsync。
- `scripts/bench.sh` 只比速度，不比結果。
- 2026-10-22 那幾次比對（D9、D10、DST-13、CLI-10、DST-06）是在本機手動跑 rsync 3.2.7，結果沒有留成測試。

憑記憶判斷 rsync 的行為已經出錯過：DST-06 的 2026-10-20 補記說「擁有者設不了時，setuid 照留，行為與 rsync 相同」，2026-10-22 實測發現 rsync 其實跳過權限與 mtime，檔案停在 0700。rsync 改版，或 Ferry 之後改壞了某個行為，現在都不會被發現。

## 提案

### 做法

新增 `tests/rsync.rs`，用表格列出情境。每個情境：

1. 建好 SOURCE 與 DESTINATION 的初始狀態。
2. 複製成兩份，一份跑 rsync，一份跑 Ferry：
   - `copy` 對照 `rsync -a SRC/ DST/`；
   - `sync` 對照 `rsync -a --delete SRC/ DST/`；
   - 有選項的，對照 rsync 同名的選項，例如 `-x`、`--numeric-ids`、`--modify-window`。
3. 比較兩份的結果樹，以及結束狀態的分類。

比較的內容：

| 比較 | 怎麼比 |
|---|---|
| 結果樹 | 每個路徑的種類、內容（雜湊）、權限（含 setuid、setgid、sticky）、mtime（到奈秒；rsync 對 symlink 的 mtime 另外確認）、symlink 目標、擁有者與群組 |
| 結束狀態 | 分成「成功」「部分失敗」「錯誤」三類：rsync 的 0、23／24、其他，對照 Ferry 的 0、23／24、其他 |
| 輸出 | 不比：兩者的訊息本來就不同 |

環境：

- 只在 Linux 上跑。macOS 內建的是 openrsync，行為與 rsync 不完全一樣。
- 沒有 rsync 時跳過，並印出原因；CI 一定要有。
- 測試開始時印出 rsync 的版本。基準是 3.2.7，也就是這台機器上的版本；CI 的 ubuntu-26.04 是哪一版，要先確認。
- 需要 root 的情境，例如掛載點、擁有者、device，放在 CI 的 root job 跑，比照 `tests/owners.rs`。
- 只比本機對本機。經 SSH 時，結果應該與本機相同，那部分由 `tests/remote.rs` 負責。

### 情境

| 類別 | 情境 |
|---|---|
| 基本複製 | 空目錄；巢狀目錄；一般檔案、symlink、FIFO；空檔案；檔名有空白、換行、非 UTF-8 |
| 屬性 | 各種權限，包括 setuid、setgid、sticky；檔案與目錄的 mtime；唯讀目錄；symlink 本身的 mtime |
| 重跑 | 什麼都沒變；內容變了，大小相同、mtime 不同；只有權限變了；只有 mtime 變了；同一秒內改寫、大小不變（D9） |
| 種類互換 | 檔案換成目錄、目錄換成檔案、檔案換成 symlink、symlink 換成目錄，空的與非空的都要 |
| 根目錄 | SOURCE 與 DESTINATION 各是檔案、目錄、symlink 或不存在的組合（DST-13） |
| sync 刪除 | 多出來的檔案、多出來的子樹、很深的子樹、唯讀目錄裡多出來的東西 |
| 讀取錯誤 | 讀不到的檔案、讀不到的目錄；sync 時，同名的項目怎麼處理 |
| 硬連結 | 不加 `-H` 時，兩邊都當成獨立的檔案 |
| 掛載點 | 加 `-x` 與不加（root） |
| 擁有者 | `--numeric-ids`；擁有者設不了，在 `unshare -r` 中（root，或使用者 namespace） |

### 已知的刻意不同

這些情境不比「相同」，而是斷言兩邊確實不同，而且不同的地方如下表。表本身就是文件，之後寫進 `docs/design/overview.md`。

| # | 情境 | rsync | Ferry | 決定 |
|---|---|---|---|---|
| 1 | SOURCE 結尾的 `/` | `src` 與 `src/` 不同 | 一律複製內容 | 設計如此，見 `docs/cli.md` |
| 2 | `copy f new/`，`new` 不存在 | 建立目錄 `new`，放成 `new/f` | 建立檔案 `new` | 行為不因結尾的 `/` 而改變（CLI-10 第 5 點） |
| 3 | 擁有者設不了 | 不設權限與 mtime，檔案停在 0700，結束狀態 23 | 拿掉 setuid／setgid，其餘照設，警告，結束狀態 0 | DST-06 的 B；設不了的屬性只是警告，見 CLI-06 |
| 4 | SOURCE 與 DESTINATION 重疊 | 照做 | 拒絕 | D1 |
| 5 | `sync` 時有讀不到的項目 | 遇到 I/O 錯誤就停止所有刪除 | 只保留同名的項目，其餘照刪 | `docs/design/overview.md` 的 `ferry sync` 一節 |
| 6 | `sync` 到 `/`、家目錄 | 照做 | 先詢問；沒有終端機就拒絕 | D2 |
| 7 | 準備階段就拒絕，例如目錄複製到檔案上 | 結束狀態 3 | 結束狀態 1 | 下面的 N3；兩者都算錯誤，Ferry 的 3 是 `ferry update --check` 有新版 |

實測時，一旦發現不在這張表上的不一致，就先停下來列給使用者：由使用者決定要改 Ferry，還是把它記成刻意的不同。決定之後再寫成測試。

## 實測結果（2026-10-22）

以 `.scratch/parity/parity.py` 實測：rsync 3.2.7 與 `1645acb` 的 debug build，本機 ext4，一般使用者；需要 root 的情境在 `unshare -rm` 中。共 60 個情境：50 個結果相同，10 個不同。

**結果相同的（50 個）：**

- 基本複製：空目錄；含檔案、相對與絕對 symlink、斷掉的 symlink、FIFO、空檔案、3 MiB 檔案的樹；檔名有空白、換行、tab、反斜線、非 UTF-8、`-` 開頭；`copy` 保留 DESTINATION 多出來的東西。
- 屬性：權限，包括 setuid、setgid、sticky 目錄、setgid 目錄、唯讀目錄；檔案、目錄與 symlink 的 mtime（到奈秒）；目的地只有權限不同。
- 重跑：都沒變；同大小、新 mtime；只有權限；只有 mtime；同大小、同 mtime、內容不同（兩者都不複製，D9）。
- 種類互換：目錄換成檔案；非空目錄換成檔案（`copy` 兩者都以 23 失敗並保留目錄，`sync` 兩者都換掉）；空目錄換成檔案；symlink 與檔案互換；目的地的 symlink 指向目錄，Source 是目錄（兩者都換掉 symlink，不跟隨）；非空目錄換成 symlink（`copy` 23、`sync` 換掉）。
- `sync` 刪除：多出來的檔案、子樹、FIFO；150 層深的子樹；唯讀目錄裡多出來的東西；多出來的唯讀目錄。
- 讀取錯誤：讀不到的檔案與目錄，`copy` 與 `sync` 的結果樹相同（結束狀態不同，見下面的 N2）。
- 硬連結：不加 `-H` 時兩邊都是兩個獨立的檔案。
- 根目錄：Source 是檔案、目錄或指向目錄的 symlink，DESTINATION 是檔案、目錄、symlink 或不存在的各種組合（16 種）。Source 是目錄而 DESTINATION 是檔案時，兩者都拒絕，原本的第 7 項不是不同。
- 掛載點（root）：不加 `-x` 時跨過掛載點；加 `-x` 時掛載點只建目錄；`sync -x` 不刪另一個檔案系統上的東西。

**結果不同、已在刻意不同表中的：** 第 1 項（SOURCE 結尾的 `/`）、第 2 項（`copy f new/`）、第 3 項（擁有者設不了，兩種情境）、第 4 項（重疊）、第 5 項（`sync` 遇到讀不到的目錄後，rsync 不再刪除之後的多餘項目，Ferry 照刪）。

**不在表中的新發現，待決定：**

- **N1：Source 的根目錄是指向檔案的 symlink。** rsync（`rsync -a link dst`）複製 symlink 本身；Ferry 跟隨它，複製目標檔案。四種 DESTINATION 都是如此。Source 是指向目錄的 symlink 時，Ferry 跟隨，與 `rsync -a link/ dst` 相同，因為 Ferry 一律把 SOURCE 當成結尾有 `/`。
- **N2：Source 有讀不到的路徑時的結束狀態。** rsync 是 23（「some files/attrs were not transferred」），只在「讀取期間消失」時用 24；Ferry 讀不到也用 24。CLI-06 的碼表寫「24：只有 SOURCE 讀不到的路徑，仿 rsync」，那是誤讀。
- **N3：準備階段就拒絕時的結束狀態。** 目錄複製到檔案上時，rsync 是 3（「errors selecting input/output files」），Ferry 是 1。兩者都算錯誤，只是碼不同；Ferry 的 3 已用於 `ferry update --check` 有新版。

**決定與修正（2026-10-22）：**

- N1 照 rsync：新的 `fsys::stat_source_root`，Source 的根目錄是 symlink 時，指向目錄就跟隨，否則以 symlink 本身送出（指向檔案、不存在的東西，或到不了的地方）；掃描與 `check_root` 都用它。`root_identity` 也一樣：不指向目錄的 symlink，以連結本身與它所在的目錄為身分，否則第二次執行時，DESTINATION 那個指向同一檔案的連結會被當成 Source 本身而拒絕。實測時另外加了 Source 是斷掉的 symlink（5 種 DESTINATION）、重跑、改指向後重跑三種情境，都與 rsync 相同。測試：`a_source_that_is_a_symlink_to_no_directory_is_copied_as_itself`，把兩處修正各自改回，它都會失敗。
- N2 照建議：讀不到的路徑算進 23，24 只留給讀取期間消失的路徑。`SOME_UNREAD` 改名為 `SOME_VANISHED`。測試：`a_partial_transfer_exits_as_rsyncs_do`、`an_unreadable_file_exits_23`（原 `an_unreadable_file_exits_24`）。
- N3：記成刻意不同表的第 7 項。

修正後再跑一次：本機 62 個情境中 58 個相同，其餘 4 個是刻意的不同（第 1、2、4、5 項）；root 的 5 個情境中 3 個相同，2 個是第 3 項。

**還沒測的：** `--numeric-ids` 與真實的多個使用者、裝置檔、擁有者設不了的 `EPERM`（都需要真正的 root，留給 CI 的 root job）；`--modify-window`；經 SSH 的情境。

## 其他做法

- **用 shell 腳本，而不是 Rust 測試。** 寫起來快，但不會跟著 `make test` 與 CI 跑，等於又是手動比對。
- **隨機產生目錄樹來比對（property-based）。** 找得到想不到的情境，但失敗時難以判讀，結果也受隨機種子影響。可以在表格化的情境穩定之後，再加一個固定種子的版本。
- **在 macOS 上對照 openrsync。** openrsync 不是使用者熟悉的 rsync，對照它沒有意義。

## 待決定的問題

- [ ] 每次 push 都跑，還是只在每晚跑？每個情境要跑兩次複製，加起來可能要幾十秒。由使用者決定。
- [x] N1、N2、N3：使用者決定（2026-10-22）N1 改成與 rsync 一致，N2 照建議改成 23，N3 照建議記成刻意的不同（第 7 項）。N1、N2 的修正見「實測結果」之後的補記。
- [ ] 結束狀態分三類是否夠細，例如 23 與 24 要不要分開比。

## 計畫

1. 實測：逐一用 rsync 3.2.7 跑每個情境，記下 rsync 與 Ferry 的結果。發現新的不一致，就列給使用者決定。
2. 寫比對用的工具函式：建樹、複製成兩份、走訪並比較兩棵樹、對照結束狀態。先用基本複製的情境驗證它。
3. 依類別逐批加入情境，每批一個 commit。
4. 需要 root 的情境，接進 CI 的 root job。
5. 把刻意的不同表寫進 `docs/design/overview.md` 與中文版。草稿至此採納，之後刪除。
