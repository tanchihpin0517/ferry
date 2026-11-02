# exclude：不傳的東西，與 `/` 在規則裡的意思

- 狀態：已決定，尚未實作
- 開始：2026-11-02；最後修改：2026-11-02
- 會改動：`docs/cli.md` 的選項表與新的一節、`README.md` 表裡的「not built yet」、`docs/design/pipeline.md` 的 Scan 裡講 `ignore` crate 的那一句、`docs/design/rsync-parity.md`；看[待決定](#待決定的問題)的第 1、4 點怎麼決定，還有 `docs/design/overview.md` 的原則二與刻意差異的表。以上都連同它的 `.zh-TW.md`。
- 相關：[overview 的原則順序](../design/overview.md#decided)：可靠、結尾的斜線不改變行為、結果同 rsync、速度。2026-11-02 使用者決定改掉第二條，改用 rsync 的表示法，見 [overview 的「兩個指令」](../design/overview.md#two-commands-copy-and-sync-in-rsyncs-notation)；本草稿裡的 `/`，照那份決定。

## 問題

`--exclude` 還沒做，README 列為 not built yet。rsync 的 pattern 裡，`/` 放在不同的位置，意思各不相同。其中結尾的 `/` 代表「只比對目錄」，字面上和原則二「結尾的斜線絕不改變行為」衝突。

原則二的文字只講 SOURCE 與 DESTINATION 這兩個路徑，沒講 pattern。但當初定它的理由，斜線容易不小心多打或少打，例如 tab 補全目錄名時會補上 `/`，在 pattern 裡一樣成立。所以這份草稿先把 `/` 的每一種意思攤開，再決定 Ferry 怎麼做；選項範圍等其他問題放在後面。

以下 rsync 的行為，都在本機以 rsync 3.2.7 實測過，script 在 `.scratch/rsync-slash/`：`run0.sh` 到 `run5.sh`。

## rsync 的規則怎麼用 `/`

一條規則就是一個 pattern。每個項目從第一條規則往下比，第一條比對到的規則，決定它傳不傳；都沒比對到，就傳。被排除的目錄，rsync 不會進去。

| `/` 在哪裡 | 意思 | 實測 |
|---|---|---|
| 沒有 `/`，例如 `build`、`*.o` | 比每個項目的最後一段名稱，任何深度都算 | `build` 排除了 `build/` 與 `a/build/`，`buildfile` 留著 |
| 開頭，例如 `/build` | 固定從傳輸的根開始比 | 只排除最上層的 `build/`，`a/build/` 留著 |
| 中間，例如 `sub/foo` | 比完整路徑的結尾，從某一段名稱的開頭比起，任何深度都算；含 `**` 的 pattern 也這樣比 | `sub/foo`、`a/sub/foo` 都排除；`xsub/foo` 留著 |
| 結尾，例如 `build/` | 只比對目錄，其餘同上 | 見[下一節](#結尾的-只比對目錄) |
| `*`、`**` | `*` 不跨過 `/`，`**` 跨過 | `build/*` 排除了每個 `build/` 裡的東西，目錄本身留著，是空的 |
| `dir/***` | 目錄本身，加上它底下的一切 | `build/***` 連 `build/` 本身都排除 |

rsync 不把 pattern 正規化：`./build`、`build//` 什麼都比對不到，也不說；`/build/` 只排除最上層的目錄 `build/`。

### 結尾的 `/`：只比對目錄

| 情況 | 加 `/`：`build/`、`cache/`、`x/`、`f/` | 不加 `/` |
|---|---|---|
| 名為 `build` 的 symlink，指向一個目錄 | 照傳：symlink 不是目錄 | 排除 |
| `sync`，DESTINATION 多出檔案 `cache` 與目錄 `sub/cache/` | 刪掉檔案 `cache`，留下目錄 `sub/cache/` | 兩個都留下 |
| `sync`，SOURCE 的 `x` 是目錄，DESTINATION 的 `x` 是檔案 | SOURCE 的目錄不傳，DESTINATION 的檔案被刪：結束時沒有 `x` | 兩邊都排除：檔案留下 |
| 只有一個檔案的 SOURCE `s/f` | 照傳 | 排除 |

結尾的 `/` 撐著兩個常見的寫法：

| 規則 | 結果 |
|---|---|
| `--exclude='*/'` | 只傳最上層的檔案 |
| `--include='*/' --include='*.c' --exclude='*'` | 只傳 `.c`，任何深度都算，連同裝著它們的目錄：`*/` 讓 rsync 進得了每個目錄 |
| 同上，但 `*/` 換成 `*` | 全部都傳，`.h` 也傳 |

### 開頭的 `/`：根在哪裡

rsync 的根，看 SOURCE 結尾有沒有斜線：

| 規則 | `rsync -a s d/`：根是 `s` 的上一層 | `rsync -a s/ d/`：根是 `s` |
|---|---|---|
| `--exclude=/build` | 不排除 `s/build` | 排除 |
| `--exclude=/s/build` | 排除 | 不排除 |

~~Ferry 一律複製 SOURCE 的內容，如同 `s/`，所以根就是 SOURCE 本身；DESTINATION 那邊，根就是 DESTINATION。這不隨斜線改變，原則二已經定了。~~

2026-11-02：Ferry 改用 rsync 的表示法（[overview](../design/overview.md#two-commands-copy-and-sync-in-rsyncs-notation)），根也照 rsync，看 SOURCE 結尾有沒有斜線，如上表。

- **只有一個檔案的 SOURCE**：rsync 以它的檔名比對，位置在根。`s/f` 時，`f`、`/f`、`*` 都排除它，`s/f` 不會；排除了就什麼都不傳，DESTINATION 也不建立，exit 0。
- **根本身永遠不被比對**：`--exclude='*'` 時，rsync 照樣建立 DESTINATION 的根目錄，裡面是空的。

### 中間的 `/`：rsync 與 `.gitignore` 不同

rsync 的 `sub/foo` 任何深度都比對得到；`.gitignore` 的 `sub/foo` 則固定從那個 `.gitignore` 所在的目錄比起，等於開頭有 `/`。`pipeline.md` 說可以用 `ignore` crate 的 glob 比對，而 `ignore` 照 `.gitignore` 的規則，中間有 `/` 的會固定在根，結果和 rsync 不同。

### DESTINATION 上被排除的東西

`sync` 只刪 Source 沒有的東西。DESTINATION 上被排除的不刪，加上 `--delete-excluded` 才刪：`--exclude='*.o'` 時，DESTINATION 多出的 `extra.o` 留著，`extra.txt` 被刪掉。

比對時，DESTINATION 看的是自己那邊項目的種類，所以同一個名稱，可能一邊被排除、另一邊沒有，如上一節表裡的 `x`。

被排除的東西在一個要刪的目錄裡時，留下它，連同裝著它的目錄：

| 情況 | rsync 的結果 |
|---|---|
| `sync`，DESTINATION 多出目錄 `old/`，裡面的 `keep` 被排除，`z` 沒有 | 刪掉 `z`，留下 `old/` 與 `old/keep`，exit 0 |
| `sync`，SOURCE 的檔案 `x` 取代 DESTINATION 的目錄 `x/`，裡面的 `y` 被排除，`z` 沒有 | 刪掉 `z`，留下 `x/y`，檔案 `x` 傳不了，exit 23 |
| 同上，但規則是 `x/`，只比對到目錄本身 | 目錄連同 `y` 被刪掉，換成檔案 `x`，exit 0；`x/**` 或 `x/***` 比對得到 `y`，就和上一列一樣 |

這和 Ferry 現在 `-x` 的做法同一個形狀：`sync` 不刪另一個檔案系統上的目錄，裝著它的目錄也留下，其餘的照刪。

## 結尾的 `/`：三個做法

2026-11-02 使用者決定改用 rsync 的表示法（[overview](../design/overview.md#two-commands-copy-and-sync-in-rsyncs-notation)），A 隨之而定，不再和原則衝突。以下留作紀錄。

- **A. 照 rsync**：結尾的 `/` 只比對目錄。
- **B. 忽略**：`build/` 等於 `build`。
- **C. 拒絕**：結尾有 `/` 的 pattern 不收，exit 2。

| | A 照 rsync | B 忽略 | C 拒絕 |
|---|---|---|---|
| 原則二的字面 | pattern 結尾的 `/` 改變結果 | 守住 | 守住 |
| 原則三 | 結果同 rsync | `dir/` 也排除同名的檔案與 symlink | 有 `dir/` 的 rsync 規則都不能用 |
| `--exclude='*/'` | 只傳最上層的檔案 | 什麼都不傳 | 拒絕 |
| `--include='*/' --include='*.c' --exclude='*'` | 只傳 `.c` | 全部都傳，不說 | 拒絕；要只傳某幾種檔案，就沒有寫法 |
| 不小心多打 `/`，例如 tab 補全目錄名 | 只比對目錄：別處同名的檔案、symlink 照傳；`sync` 時 DESTINATION 多出的同名檔案被刪 | 沒有差別 | 拒絕，說出原因 |
| 不小心少打 `/` | 同名的檔案、symlink 也被排除 | 沒有差別 | 沒有差別 |
| `.gitignore` 的寫法，如 `node_modules/`、`.git/` | 意思相同 | 意思不同 | 不能用 |

**建議 A**，並把原則二的文字改得更明確：它講的是 SOURCE 與 DESTINATION 這兩個路徑；pattern 裡的 `/` 是 pattern 語言的一部分，和 rsync、`.gitignore` 的意思相同。

理由：B 會把最常見的 include 寫法，悄悄變成全部都傳，結果錯了也不說；C 讓常見的規則檔與那個寫法都不能用，等於另外發明一套語法。A 的代價也要說清楚：在 pattern 裡，結尾的斜線確實改變結果。原則二要不要涵蓋 pattern，由你決定。

## 要注意的地方

以下只記問題，不寫解法：

- **Ferry 的 dry run 列表裡的路徑**，例如 `data/sub/stale/`，原樣拿來當 pattern 時，算中間有 `/` 的寫法，任何深度的 `data/sub/stale/` 都會比對到，不只那一個。0.20.0 起，列表與規則從同一個根算起，所以在列出的路徑前面加上 `/`，如 `/data/sub/stale/`，就剛好固定指到那一個項目。
- ~~**習慣 `rsync -a data dst` 的人**，寫 `--exclude=/data/build` 或 `data/build`，或在 `data` 外面以 tab 補全出 `data/build/`，在 Ferry 都比對不到 `data` 最上層的 `build`：Ferry 的路徑從 SOURCE 裡面算起，同 `rsync -a data/`。這來自原本就有的刻意差異，不是 exclude 新帶來的。~~ 改用 rsync 的表示法之後，這一條就不存在了。
- **`-0` 的 dry run 列表**，每行是 `+ PATH` 或 `- PATH`，和 rsync 規則檔的寫法一樣，意思卻不同：列表裡的 `-` 是要刪的，在 `--exclude-from` 的檔案裡是排除。檔案裡以 `+ `、`- ` 開頭的行，rsync 會當成 include、exclude，實測過。

## 其他做法

- **照 `.gitignore` 的語意**，中間有 `/` 的固定在根。原則三不成立。
- **A 再加一個警告**：只比對目錄的規則，跳過了同名的非目錄時，警告一次。多一條 rsync 沒有的規則；真的只要目錄的人，每次跑都會看到它。

## 待決定的問題

`/` 的部分：

- [x] **1. 結尾的 `/`。** 2026-11-02 使用者決定改用 rsync 的表示法（[overview](../design/overview.md#two-commands-copy-and-sync-in-rsyncs-notation)），所以是 A：只比對目錄。
- [x] **2. 開頭的 `/`。** 2026-11-02 隨結尾斜線的決定定了，包括 SOURCE。照 rsync：根看 SOURCE 結尾有沒有斜線，`dir` 時是它的上一層，`dir/` 時是 `dir`；DESTINATION 那邊對應。只有一個檔案的 SOURCE，以它的檔名比對，位置在根。
- [x] **3. 中間的 `/`。** 2026-11-02 使用者決定照建議：照 rsync，任何深度都比對得到，不照 `.gitignore`。
- [x] **4. 永遠比對不到的寫法**，如開頭的 `./`、中間的 `/./`、連續的 `//`：路徑裡不會有這些，所以這種規則等於沒寫。rsync 收下，不說。`sync` 時，一條本來要保護 DESTINATION 上某個東西的規則，因此沒生效，那個東西就被刪了。
  - 照 rsync，收下。
  - 拒絕，exit 2，同拒絕重疊的根目錄，以原則一列為刻意的差異。
  - 建議拒絕。2026-11-02 使用者決定照建議：拒絕，exit 2，列為刻意的差異。

`/` 以外，之後再談：

- [x] **5. 做哪些選項。** 2026-11-02 使用者決定照建議。建議第一批做 `--exclude`、`--include`、`--exclude-from`、`--include-from`、`--delete-excluded`；規則檔裡可以用 `+ `、`- ` 開頭，同 rsync。`--filter`、merge 與 dir-merge、`-F`、`-C`、`!` 等以後再說，寫進 `docs/design/future.md`。
- [x] **6. `sync` 時 DESTINATION 上被排除的東西。** 2026-11-02 使用者決定照建議。建議照 rsync 留下，連同裝著它的目錄；沿用 `-x` 留下目錄的做法，同一條規則，不另立。

- [x] **7. `copy --delete-excluded`。** rsync 的 `--delete-excluded` 單獨用，就隱含 `--delete`：實測只給它與 `--exclude='*.o'`，`extra.o` 與 `extra.txt` 都被刪掉（`run5.sh`）。Ferry 的刪除是 `sync` 這個指令，`copy` 從不刪東西。
  - 拒絕，exit 2，說改用 `sync --delete-excluded`；同 `-x` 給兩次被拒絕，列為刻意的差異。
  - 照 rsync，讓 `copy --delete-excluded` 也刪：`copy` 就不再是「從不刪」。
  - 建議拒絕。2026-11-02 使用者決定照建議：拒絕，exit 2，列為刻意的差異。

## 實作時照 rsync 的細節

都實測過（`run0.sh` 到 `run5.sh`），不另外要決定：

- **規則照命令列上的順序**，五個選項混在一起排；每個項目從第一條往下比，第一條比對到的決定。
- **規則檔**一行一條；`#`、`;` 開頭的是註解，空行忽略；`+ `、`- ` 開頭的是 include、exclude；`-` 是標準輸入。
- **比對的路徑從傳輸的根算起**，與 dry run 列出的相同：SOURCE 不帶斜線時，前面是它的名字。SOURCE 指名的項目本身也比對，被排除就什麼都不傳，exit 0，`sync` 也什麼都不刪；帶斜線時，根永遠不比對。
- **永遠比對不到的寫法**：去掉開頭的 `/` 與結尾的一個 `/` 之後，有空的、`.` 或 `..` 的一段，如 `./build`、`a//b`、`build//`、`a/../b`。

## 計畫

1. **規則與比對**，一個新的模組：
   - 解析規則與規則檔，拒絕永遠比對不到的寫法。
   - 比對：`*`、`**`、`?`、`[...]`、`\` 跳脫、`dir/***`；結尾 `/` 只比對目錄；開頭 `/` 固定在根；含 `/` 或 `**` 的比整條路徑的結尾，從一段名稱的開頭比起；其餘只比最後一段名稱。
   - 自己寫，不用 `ignore` 或 `globset`：前者是 `.gitignore` 的語意，後者的 `**` 只能是完整的一段，rsync 的可以在一段名稱中間。
   - 單元測試照上面實測的每一列。
2. **CLI 與 client**：五個選項照順序合成一串規則；規則檔在 client 讀；規則與 `--delete-excluded` 放進 Hello，交給兩邊的 server；第 7 點照決定。
3. **Source**：掃描時，每個項目照從傳輸的根算起的路徑比對，名稱前面加上 SOURCE 的名字（不帶斜線時）；被排除的不進 manifest，被排除的目錄不進去；SOURCE 指名的項目本身也比對。
4. **Destination**：
   - 多出來的項目被排除就不刪，dry run 也不列；`--delete-excluded` 才刪。
   - 要刪的目錄，或被別種東西取代的目錄，裡面被排除的留下，連同裝著它的目錄，同 `-x`：沿用 `src/remove.rs` 的 `Keep` 與 `kept`，同樣發出警告；dry run 的計數照算。
   - 放進 DESTINATION 的根（placed），路徑前面同樣是名字。
5. **與 rsync 對照**：`tests/rsync.rs` 的 `args` 兩邊都給，加上規則的情況：上面實測的每一列，`copy` 與 `sync`，SOURCE 帶斜線與不帶。
6. **文件**，中英兩版：
   - cli 的選項表與新的一節、README（`--exclude` 從 not built 拿掉）；
   - overview 刻意差異的表：永遠比對不到的規則被拒絕，第 7 點若拒絕也列入；
   - rsync-parity、pipeline 講 `ignore` crate 的那一句；
   - future.md：`--filter`、merge 與 dir-merge、`-F`、`-C`、`!`；
   - CHANGELOG 的 Added。
