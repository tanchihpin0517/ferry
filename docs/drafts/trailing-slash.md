# 結尾的斜線：改用 rsync 的表示法

- 狀態：草稿
- 開始：2026-11-02；最後修改：2026-11-02
- 會改動：
  - `docs/design/overview.md` 的原則順序、「兩個複製內容的指令」一節、刻意差異的表；
  - `docs/cli.md` 的 Synopsis，以及 SOURCE、DESTINATION 的說明；
  - `README.md`、`docs/design/rsync-parity.md`；
  - 以上都連同它的 `.zh-TW.md`。
- 會改的程式：`src/prepare/ask.rs` 的 `place_destination`、兩邊 server 的根、`tests/rsync.rs` 的兩個 `deliberately_` 測試，以及所有用了不帶斜線的 SOURCE 的測試。
- 相關：[exclude 草稿](exclude.md)。規則裡的 `/`，以及規則的根在哪裡，都照這份決定。

## 決定

2026-11-02 使用者決定：改掉原則二「結尾的斜線絕不改變行為」，改用 rsync 的表示法。

我的理解是全部照 rsync，包括 SOURCE：`dir` 複製目錄本身，`dir/` 複製它的內容。如果只要改 DESTINATION 與規則，見[待決定](#待決定的問題)第 1 點。

原則二當初的理由是：斜線容易不小心多打或少打，例如 tab 補全目錄名時會補上 `/`。改用 rsync 的表示法，這個風險就回來了，見[風險](#風險)。

## rsync 的表示法

以下都以 rsync 3.2.7 實測過，script 在 `.scratch/slash-paths/`：`run.sh` 到 `run4.sh`。

**SOURCE**

- **最後一段是名稱**，也就是不以 `/` 結尾，也不是 `.` 或 `..`：它本身放進 DESTINATION，成為 `DESTINATION/名稱`。目錄、檔案、symlink 都一樣；symlink 不跟過去，複製的是 symlink 本身。
- **以 `/` 或 `/.` 結尾，或者就是 `.`、`..`**：它的內容放進 DESTINATION。
- **檔案加上 `/`**：失敗，Not a directory。

**DESTINATION**

- 下列情況，DESTINATION 是一個目錄，不存在就建立，只建最後一層：
  - 它已經是目錄，或是指向目錄的 symlink；
  - 它以 `/` 結尾；
  - 要放進去的不只一個東西：SOURCE 的內容，或一個目錄。
- 其餘情況，DESTINATION 就是那個檔案的名字：`copy f new` 建出檔案 `new`。
- 以 `/` 結尾、卻是一個既有的檔案：拒絕。
- `DESTINATION/名稱` 已經是別種東西，例如 `dir` 是目錄、`dst/dir` 卻是檔案：照樣取代。`copy` 和 `sync` 都實測過。

**刪除（`sync`）**

只刪傳過去的目錄裡面的東西：`sync dir dst` 只刪 `dst/dir/` 裡 Source 沒有的，`dst` 裡的其他東西都不動。

**DESTINATION 本身的權限與 mtime**

- 複製內容（`dir/`）時，DESTINATION 拿 `dir` 的。
- 複製目錄本身（`dir`）時，DESTINATION 若是新建的，權限照 umask，mtime 是當下；`DESTINATION/dir` 才拿 `dir` 的。
- 實測：`dir` 是 700，mtime 在 2001 年。`rsync -a dir new` 建出的 `new` 是 775、當天，`new/dir` 是 700、2001 年。

**dry run 列出的路徑**

從傳輸的根算起：`rsync -n -v dir new` 列出 `dir/`、`dir/a`；`rsync -n -v dir/ new` 列出 `./`、`a`。

## Ferry 會變的地方

| 情況 | 現在 | 改了之後，同 rsync |
|---|---|---|
| `copy dir dst` | `dst/a` | `dst/dir/a` |
| `sync dir dst`，`dst` 裡另有 `other` | `dst/a`，`other` 被刪掉 | `dst/dir/a`，`other` 留著 |
| `copy link dst`，`link` 指向目錄 | 跟過去：`dst/r` | 複製 symlink 本身：`dst/link` |
| `copy f new/`，`new` 不存在 | 建出檔案 `new` | 建出目錄：`new/f` |
| `copy f ef/`，`ef` 是既有的檔案 | 蓋掉 `ef`，exit 0 | 拒絕；rsync 是 exit 3，Ferry 在開始前拒絕，照舊 exit 1 |
| `copy dir dst`，`dst/dir` 是檔案 | 不會遇到 | 檔案換成目錄 |
| 規則的根（exclude） | 還沒做 | `dir` 時是它的上一層，規則寫 `/dir/build`；`dir/` 時是 `dir`，規則寫 `/build` |
| dry run 與 dashboard 的路徑 | 從 SOURCE 裡面算起，根是 `.` | 見[待決定](#待決定的問題)第 2 點 |

**不變的**：

- `dir/`、`dir/.`、`.`、`..`：複製內容，同現在。
- DESTINATION 是既有目錄時，有沒有 `/` 都一樣。檔案放進既有目錄，成為 `DESTINATION/名稱`。
- DESTINATION 是指向目錄的 symlink：跟過去。
- 上一層不存在：失敗。
- 重疊的根照樣拒絕，比的是放好之後的根。
- `sync` 在 `/`、家目錄，或裝著家目錄的目錄裡刪東西之前，照樣先問。問的也是放好之後的根：現在 `plan::agree` 就在 `place_destination` 之後。所以 `sync data ~` 只刪 `~/data` 裡的，不問；`sync data/ ~` 會問。
- `sync` 的 SOURCE 是檔案時：照舊拒絕。

## 風險

1. **tab 補全的 `/`**，例如把 `data` 補成 `data/`：
   - `copy`：放成 `dst/a`，而不是 `dst/data/a`。位置錯了，但沒有東西不見。
   - `sync`：要同步的從 `dst/data` 變成 `dst` 本身，`dst` 裡 Source 沒有的東西全部刪掉。這正是原則二當初要擋的。剩下的保護，只有 `/`、家目錄、裝著家目錄的目錄會先問；例如 `/mnt/backup` 裡放著別的專案，不會問。rsync 的使用者也有同樣的風險。
2. **現在的 Ferry 有反方向的陷阱。** 照 rsync 的習慣寫 `ferry sync data /mnt/backup`，本意是放進 `/mnt/backup/data`；現在的 Ferry 卻會把 `/mnt/backup` 同步成 `data` 的內容，其餘全刪。改了之後，這個陷阱就沒了。
3. **從 0.19 升級。**
   - 腳本裡不帶斜線的 `ferry copy data backup`、`ferry sync data backup`，升級後會放進 `backup/data`，整棵樹重傳一次，磁碟用量可能加倍，甚至寫滿。
   - `backup` 裡原本的東西不會被刪，因為 `sync` 只刪 `backup/data` 裡面的。
   - 例外：`data` 最上層剛好有個名為 `data` 的目錄時，`backup/data` 已經存在，會被同步成整個 `data`；它裡面 Source 的 `data` 沒有的東西會被刪掉。
4. **0.x 裡的破壞性改變。** 要在 CHANGELOG 的 Changed 裡寫在最前面，版號升到 0.20.0。

## 原則的新文字（提案）

overview 的原則順序改成：

1. **可靠**：每個檔案完整、正確；打錯的路徑不造成傷害。
2. **結果同 rsync**：同樣的輸入，留下同樣的結果與結束碼，包括結尾斜線的意思。
3. **速度**。

連帶要改的：

- 刻意差異的表，拿掉前兩列：SOURCE 不帶斜線、檔案放到 `new/`。
- 「兩個複製內容的指令」一節改寫：`copy`、`sync` 照 rsync 的表示法。
- 比對的對象從 `rsync -a SOURCE/ DESTINATION/` 改成 `rsync -a SOURCE DESTINATION`，SOURCE 與 DESTINATION 照使用者寫的原樣。
- 「只收一個 SOURCE」的理由是多個來源的內容會在 DESTINATION 裡互相衝突。照 rsync 的表示法，不帶斜線的多個來源各放進自己的名稱，不再衝突。這份草稿不改它，只記下來。

## 順帶發現的問題

只記問題，不寫解法：

- **`sync` 的 SOURCE 是檔案時，Ferry 拒絕**，rsync 則照樣複製，什麼都不刪：`rsync -a --delete f dst` 得到 `dst/f`，`other` 留著。這個差異不在刻意差異的表裡，也沒有 `deliberately_` 測試。改了之後，指向目錄的 symlink 不帶斜線時不再跟過去，也會落進這一類：`rsync -a --delete link dst` 得到 `dst/link`，`other` 留著。

## 待決定的問題

- [ ] **1. 範圍。** 全部照 rsync，包括 SOURCE？這是我的理解。如果只改 DESTINATION 與規則，上表前三列與「規則的根」都不變，風險 1 到 3 也不存在。
- [ ] **2. dry run 與 dashboard 的路徑。** 建議照 rsync，從傳輸的根算起：`dir` 時列出 `dir/`、`dir/a`；`dir/` 時照現在，根是 `.`。訊息是 Ferry 自己的，原則不管，所以這是個選擇。
- [ ] **3. 原則一的「打錯的路徑不造成傷害」。** tab 補全的 `/` 會讓 `sync` 刪掉 DESTINATION 裡的其他東西，是這句話的例外。原則一的文字要不要跟著改？
- [ ] **4. `sync` 的 SOURCE 是不帶斜線、指向目錄的 symlink。** 建議照 `sync` 拒絕檔案的做法拒絕；rsync 會複製 symlink，什麼都不刪。這和「順帶發現的問題」是同一件事。

## 計畫

決定之後再細排。大致是：

1. **放 DESTINATION 的地方。** 有兩種做法，實作時再比：
   - **(a) 放好之後的根是 `DESTINATION/名稱`**，延伸現在檔案放進目錄的 `into`；DESTINATION 不存在時，Destination 先建它。dry run 的路徑、規則的根、取代 `DESTINATION/名稱` 的別種東西、DESTINATION 本身的權限，都要各自處理。`sync` 前的詢問自然問對地方。
   - **(b) 照 rsync 的做法**：Destination 的根是 DESTINATION，Source 的根是 SOURCE 的上一層，裡面只有「名稱」這一個項目，而且根這一層不刪多出來的。dry run 的路徑、規則的根、取代、權限都自然和 rsync 一樣；但 `sync` 前的詢問要改成看 `DESTINATION/名稱`。
2. **斜線原樣保留**，本機與遠端的路徑都是，交給 server。
3. **測試。**
   - 兩個 `deliberately_` 測試，改成和 rsync 相同的對照。
   - `tests/rsync.rs` 把路徑原樣交給 rsync 和 Ferry，帶斜線、不帶斜線各一次。
   - 其他測試裡，本意是複製內容、卻沒帶斜線的 SOURCE 都要加上 `/`：`tests/` 裡 `"copy"`、`"sync"` 約出現 140 處，加上 `ferry test` 自己組出的路徑。
4. **文件。** overview、cli、README、rsync-parity、dashboard（看第 2 點）、CHANGELOG，中英兩版。
