# `--watch`：持續看著 SOURCE，改了就再傳一次

- 狀態：已決定，實作中
- 開始：2026-11-03；最後修改：2026-11-03
- 會改動：
  - `docs/design/overview.md`「Scope: one-way only」：「continuous watching of files for changes」目前列為非目標，要拿掉；
  - `docs/cli.md` 的 Options、Output、Exit status；
  - `docs/design/architecture.md`：place（SSH 連線、host 上的 monitor）目前跟著一次傳輸結束，要改成活得比一次傳輸久；host 上的 monitor 多啟動一種程序，watcher；
  - 以上都連同它的 `.zh-TW.md`。
- 會改的程式：`src/cli.rs` 的 `transfer`；`src/prepare/mod.rs` 的 `prepare`，拆成只做一次的部分與每一輪的部分；`src/monitor/client`；dashboard 與 `progress`；新的 watcher。
- 相關：[Future improvements](../design/future.md)。

## 決定

2026-11-03 使用者決定：

- 第 1 題：兩種用途都要，本機改、傳到叢集，以及叢集上產生、拉回本機。
- 第 2 題：**優先用事件通知，不支援才重掃**。這和原本的建議（先做輪詢）不同；因此多出第 11–17 題。
- 第 3–10 題：照建議。
- 第 7 題後來改了：`--watch` 沒有預設，後面一定要給秒數，所以不需要 `=`。秒數是整數，0 以上；`--watch 0` 是即時傳。
- 第 18 題：overlayfs 列入白名單，照建議。2026-11-03 使用者說開始實作。
- 第 19 題：(b)，只對 inotify、inotify-sys 例外。
- 第 11–17 題：照建議，但第 16 題改了：`--watch` 的秒數，事件可用時也用。它決定什麼時候傳，事件在這段時間裡累積；取代原本建議的「安靜 1 秒、最多 10 秒」。我的理解寫在第 16 題。

## 要推翻的決定

`docs/design/overview.md` 的「Scope: one-way only」把「continuous watching of files for changes」列為非目標。採用這份提案，這一條就要拿掉。

兩向同步仍是非目標：`--watch` 只做單向，讓 DESTINATION 跟著 SOURCE。

## 提議

`copy` 和 `sync` 都加上 `--watch`：

```text
ferry sync --watch 10 ./proj/ host:proj
```

1. 開始前，SOURCE 那一邊開始看著 SOURCE。
2. 第一輪就是現在的一次傳輸。
3. 之後，從上一輪結束算起，每隔 `--watch` 的秒數一個時間點：這段時間裡 SOURCE 有事件，或 SOURCE 有不支援事件的部分，就傳一輪；否則等下一個時間點。見第 16 題。
4. 一直跑到 Ctrl-C，或某一輪失敗。

rsync 沒有這種選項：3.2.7 的 `--help` 裡沒有 watch 或 repeat。所以沒有 rsync 的行為可以對照，以下參考其他工具。

## 別人怎麼做

| 工具 | 怎麼知道改了 | 知道之後 |
|---|---|---|
| lsyncd | inotify 或 fsevents | 事件累積到 `delay` 秒，或 1000 個事件，才呼叫 rsync；啟動時先完整 rsync 一次 |
| Mutagen | macOS：在根目錄設一個遞迴的 watch。Linux：輪詢，預設每 10 秒，加上對最近改過的內容設少量 inotify watch。native watching 失敗時退回輪詢 | 文件沒寫 |
| Syncthing | 檔案系統的 watcher，加上每小時一次完整掃描 | 改變累積 10 秒才掃描，刪除再延 1 分鐘；文件建議 watcher 開著也保留完整掃描，因為 watcher 可能漏掉改變 |

Mutagen 在 Linux 不用 inotify 看整棵樹，它的文件說原因是：inotify 不遞迴，每個被看的檔案或目錄都要開一個 watch descriptor。

unison 也有 `-repeat`，但它的手冊網址這次抓不到（404），沒有查證，表裡不列。

## 事件通知能做到什麼

**inotify（Linux）**，見 inotify(7)：

- 不遞迴：一個目錄一個 watch，子目錄要另外加。整棵樹很大時，加 watch 要花不少時間。
- 新的子目錄：等 watch 加上去時，裡面可能已經有檔案和子目錄了，要在加了之後掃一次。
- 網路檔案系統：「Inotify reports only events that a user-space program triggers through the filesystem API. As a result, it does not catch remote events that occur on network filesystems.」別台機器做的改動看不到。
- 經過 `mmap`、`msync`、`munmap` 的改動不報。
- 事件佇列可能滿出來，這時事件會遺失，但一定會收到 `IN_Q_OVERFLOW`。
- `max_user_watches` 是每個使用者 watch 數的上限。這台機器（kernel 5.15）是 1048576；5.11 以前的 kernel 預設 8192。

**FSEvents（macOS）**，見 Apple 的 File System Events Programming Guide（2012 年的版本，可能過時）：

- 根目錄設一個就涵蓋整棵樹，預設只說到「哪個目錄有改變」。
- 事件合併或遺失時，會帶 `kFSEventStreamEventFlagMustScanSubDirs`，要重掃那個路徑。
- 文件說事件清單只能當參考，不是所有改變的完整清單；備份軟體仍應定期完整掃一次。
- 網路磁碟上別台機器做的改動報不報，文件沒寫。

**notify crate**（8.2.1，CC0-1.0，在 `deny.toml` 和 `about.toml` 允許的授權裡）：

- Linux 用 inotify，macOS 預設用 FSEvents；另有 `PollWatcher`。
- 文件說 NFS 這類網路檔案系統可能完全不發事件；Linux 的 backend「not a 100% reliable source」。

來源：

- [lsyncd：layer 4 設定](https://lsyncd.github.io/lsyncd/manual/config/layer4/)、[設定檔](https://lsyncd.github.io/lsyncd/manual/config/file/)
- [Mutagen：watching](https://mutagen.io/documentation/synchronization/watching/)
- [Syncthing：syncing](https://docs.syncthing.net/users/syncing.html)、[config](https://docs.syncthing.net/users/config.html)
- [inotify(7)](https://man7.org/linux/man-pages/man7/inotify.7.html)
- [File System Events Programming Guide](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/FSEvents_ProgGuide/Introduction/Introduction.html)
- [notify](https://docs.rs/notify/latest/notify/)

## 已決定的問題

### 1. 主要用途：兩種都要

- **本機改，傳到叢集**，例如在筆電改程式碼，同步到 nano4：SOURCE 在本機磁碟，延遲要短。
- **叢集上產生，拉回來**，例如計算節點寫實驗結果，同步回本機：SOURCE 在 WekaFS 或 NFS 上，由別台機器寫入。inotify 看不到別台機器的改動，所以這一種要靠重掃。

### 2. 怎麼知道 SOURCE 改了：事件優先，不支援才重掃

原本的三個選項：

- **(A) 輪詢**：上一輪結束後等一個間隔，再完整傳一輪。
- **(B) 事件觸發**：事件說有改變，就完整傳一輪。
- **(C) 事件，只比對改了的部分**：只列出、只比對事件說改了的目錄。

使用者決定事件優先，不支援才重掃。事件之後是 (B) 還是 (C)，見第 11 題。

### 3. 兩輪之間留著什麼：place 留著，每一輪啟動新的 server

- 只做一次的：登入、probe、DESTINATION 放在哪裡、安裝、host 上的 monitor、sudo。
- 每一輪的：經過 host 上的 monitor 啟動兩個 server（不開新的 session）、`plan::agree`、streams、follow。
- 沒選的：每一輪重新登入（nano4 新的連線要 OTP，一個 session 要 10–16 秒）；server 留著（要新的控制訊息，以及把 server 的狀態歸零）。
- 現在 `transfer` 在 `client.wait()` 之後就關掉 place（`drop((streams, places))`），host 上的 monitor 跟著結束；要改成整個 watch 結束才關。
- DESTINATION 放在哪裡，第一輪決定，之後每一輪照用，不重新 probe。每一輪的 server 照舊檢查自己的根，所以 SOURCE 的根中途換了種類，那一輪會失敗，見第 5 題。
- 兩輪之間沒有傳輸，但 client 的 monitor 和 host 的 monitor 之間照舊每 5 秒互送一次心跳，30 秒什麼都沒收到就判定那台 host 失去（architecture.md 的 transport 第 11 點）。所以連線在兩輪之間斷了，也會發現。
- 遠端 SOURCE 的事件，誰在兩輪之間看著，見第 15 題。

### 4. 連線斷了：watch 以失敗結束

沿用 2026-10-25 的決定：一台 host 一條 SSH 連線，斷了傳輸就失敗，不重新登入（architecture.md「One SSH connection to a host, and a drop fails the transfer」）。長時間跑的 watch 一斷就停，要自己重新執行。

### 5. 某一輪出錯

- **一輪有做完**，但有些沒複製、沒刪除，或讀的時候消失了（23、24）：繼續。watch 時檔案常常正在被寫，這種情況會比單次傳輸常見。
- **一輪沒做完**：傳輸失敗（1），例如磁碟滿、連線斷了；或 SOURCE 本身讀不到（23，什麼都沒做）。watch 結束，結束碼就是那一輪的。
- 要注意的一種情況：建置工具先刪掉再重建 SOURCE 時，剛好碰到的那一輪讀不到 SOURCE，watch 就結束了。

### 6. 結束碼：Ctrl-C 一律 130

同現在的表，不加規則。

### 7. 間隔：`--watch SECONDS`，沒有預設

- 沒有預設，`--watch` 後面一定要給秒數，例如 `--watch 10`。值一定要有，所以 `ferry sync --watch 5 src dst` 的 `5` 一定是秒數，不需要 `=`；寫成 `--watch=5` 也可以，同其他選項。
- 秒數決定什麼時候傳，用事件時也用，見第 16 題。
- 從上一輪結束算起，兩輪不會重疊。一輪很久的樹，自然就跑得少。
- 整數秒，0 以上；負數、小數都是命令列錯誤（結束碼 2）。`--watch 0` 是即時傳：有事件就傳，一輪進行中來的事件，這一輪一結束就傳下一輪。
- `--watch 0` 遇到不支援事件的部分，一輪結束就馬上再傳一輪，不停地掃整棵樹。

### 8. 輸出

- **有終端機**：dashboard 整個 watch 都在。一輪進行中，照現在顯示；兩輪之間，顯示上一輪的結果，以及在等什麼：等事件，或者下一輪什麼時候開始。現在的 dashboard 跟著一次傳輸結束，結束後印摘要。
- **沒有終端機，或 `--no-ui`**：一輪有複製、刪除或出錯時，印出那一輪的摘要和警告。什麼都沒變的一輪不印，同現在「An unchanged tree prints nothing」。
- **`--profile`、`--profile-file`**：每一輪寫一次，檔案留最新一輪的。
- 開始時說用事件還是重掃；重掃的話，說是哪個路徑、為什麼，例如它在 NFS 上。

### 9. `--dry-run` 加 `--watch`：拒絕

結束碼 2，同 `copy --delete-excluded` 被拒絕的做法。

### 10. `sync` 問危險的 DESTINATION：只在第一輪問

沒有終端機時照舊拒絕，除非給了 `--allow-dangerous-destination`。

## 已決定的問題：事件通知

### 11. 事件之後：完整傳一輪 (B)，還是只比對改了的部分 (C)

「不支援才重掃」字面上像 (C)：事件可用時，不再掃整棵樹。(B) 則是每一輪仍然掃整棵樹，只是由事件決定什麼時候開始。

- **(B)**：
  - 一輪就是一次傳輸，正確性和單次傳輸一樣。
  - 漏掉的事件影響小：之後任何一個事件觸發的那一輪，會把漏掉的改變一起帶過去。新子目錄的時間差、佇列滿出來，也都只是「再完整傳一輪」。
  - 代價：每次有改變，兩邊都列出整棵樹。什麼都沒變時，沒有負擔。
- **(C)**：
  - 每次改變的代價只跟改了多少有關，大樹上差很多。
  - 要新的協定：部分的 manifest，Destination 只就一部分決定，`sync` 的刪除也只限那一部分。現在的設計是 Destination 拿整份 manifest 決定。
  - 漏掉的事件，那個改變就一直沒傳過去，直到那個目錄又有事件。
- **決定 (B)**，照建議。(C) 記到 future.md。

### 12. 怎麼判斷「不支援」

有兩種不支援：

- **API 本身失敗**：沒有 inotify、watch 數到了上限。看得出來，直接改成重掃。
- **API 正常，但看不到別台機器的改動**：網路檔案系統。API 不會報錯，只能看檔案系統的種類判斷。

看種類的做法：

- **(a) 白名單**：列出的本機檔案系統才用事件，其餘都重掃。Linux 看 `statfs` 的 `f_type`，例如 ext4、xfs、btrfs、tmpfs；macOS 看 `statfs` 的 `MNT_LOCAL`。
- **(b) 黑名單**：列出的網路檔案系統才重掃，其餘都用事件，例如 NFS、SMB、Lustre、GPFS、WekaFS。

**決定 (a)**，照建議：沒見過的檔案系統就重掃，不會漏掉改變；代價是沒列到的本機檔案系統也重掃。

overlayfs 列入，見第 18 題。

### 13. 一棵樹跨了檔案系統，只有一部分支援

沒有 `-x` 時，SOURCE 底下可以有別的 mount point，例如本機目錄裡掛了一個 NFS。

- **(a) 整棵樹都重掃。**
- **(b) 兩者都用**：事件照樣觸發一輪，另外每隔一個間隔傳一輪。因為每一輪都是完整的（第 11 題 (B)），定時的那一輪就涵蓋了不支援的部分。
- **決定 (b)**，照建議。第 16 題改成秒數決定什麼時候傳之後，(a) 和 (b) 變成一樣：有任何一部分不支援，每個時間點都傳一輪。

### 14. 支援時，要不要也定時完整傳一輪

- Apple 的文件說，事件只能當參考，備份軟體仍應定期完整掃一次。Syncthing 文件建議 watcher 開著也保留完整掃描，預設每小時一次。
- 有訊號的遺失（`IN_Q_OVERFLOW`、`MustScanSubDirs`）都會觸發一輪，不成問題。
- 沒有訊號的遺失，例如經過 `mmap` 的改動：照第 11 題 (B)，等下一個事件觸發的那一輪才帶過去；沒有下一個事件，就一直不帶。
- **(a) 不要**，照「不支援才重掃」。
- **(b) 要**，例如每小時一次。
- **決定 (a)**，照建議，因為第 11 題是 (B)。

### 15. 遠端 SOURCE，誰在兩輪之間看著

事件要在 SOURCE 所在的機器上收。但照第 3 題，server 只活一輪。

- **(a) 一個 watcher 程序**：
  - 由 host 上的 monitor 啟動，同 server，活過整個 watch，經它自己的控制連線告訴 client「改了」，以及哪些部分不支援。
  - 本機的 SOURCE 也一樣，在本機起一個 watcher。
  - 每一輪照舊啟動新的 server。
- **(b) Source server 兩輪之間不結束**，自己看著。這就是第 3 題沒選的「server 留著」。
- **決定 (a)**，照建議：一輪仍然就是一次傳輸；host 上的 monitor 仍然只負責啟動程序，只是多一種。

watcher 在第一輪之前就要把 watch 都加好，第一輪期間的改變才會觸發下一輪。在 Linux，這要先走過整棵樹，一個目錄一個 watch。

### 16. 事件之後什麼時候傳：`--watch` 的秒數

- lsyncd 累積到 `delay` 秒；Syncthing 累積 10 秒，刪除再延 1 分鐘。
- 原本的選項：(a) 第一個事件之後等固定的秒數；(b) 等安靜，沒有新事件滿 1 秒才開始，第一個事件之後最多等 10 秒。原本建議 (b)。
- **使用者決定**：`--watch` 的秒數決定什麼時候傳，事件在這段時間裡累積；`--watch 0` 是即時傳。

我的理解：

- 從上一輪結束算起，每隔 SECONDS 秒一個時間點。
- 到了時間點，這段時間裡有事件，就傳一輪；沒有，就等下一個時間點。SOURCE 有不支援事件的部分時，每個時間點都傳一輪。
- 一輪進行中來的事件也算，所以上一輪結束後的第一個時間點就會傳。
- 改變到開始傳，最多等 SECONDS 秒。
- `--watch 0`：時間點是連續的，有事件就馬上傳。
- 另一種理解，第一個事件之後等 SECONDS 秒（lsyncd 的做法）：差別只在改變之後等多久，固定 SECONDS 秒，而不是 0 到 SECONDS 秒。

### 17. 用 notify crate，還是自己寫

- **notify**：Linux 的 inotify 和 macOS 的 FSEvents 都有，會自己替新的子目錄加 watch；授權可以用。「不支援」的判斷（第 12 題）要自己做。
- **自己寫**：Linux 用 `libc` 的 inotify；macOS 的 FSEvents 要經過 CoreServices 和 run loop。
- **決定 notify**，照建議。

## 已決定的問題：overlayfs

### 18. overlayfs 列不列入白名單

- Docker 容器的根是 overlayfs，開發用的這台機器就是。
- 經過 overlay 的改動，inotify 看得到。直接改底下的層，看不到；Docker 的底下的層是映像檔，不會改。
- **決定列入**，照建議。

## 風險

1. **SOURCE 誤刪，很快 DESTINATION 也刪了。** `sync --watch` 在 `--watch` 的秒數之內，就把 SOURCE 的刪除帶到 DESTINATION；`--watch 0` 是馬上；單次的 `sync` 至少要有人再下一次指令。rsync 有 `--max-delete`，Ferry 沒有。Syncthing 把刪除多延 1 分鐘。
2. **寫到一半的檔案。** 一輪讀到正在寫的檔案時，只讀到打開那時的大小。DESTINATION 會有一個完整、驗證過，但只有前半段內容的檔案，直到下一輪。這個檔案保留打開時的 mtime，所以下一輪會再複製。這是現有的行為，watch 讓它變得常見；`--watch` 的秒數越小越常見。
3. **同一秒內、大小沒變的改動。** 預設 mtime 比到秒（overview「Every file is complete and correct」第 2 點）。一輪複製之後，同一秒內又改了一次，而大小沒變，之後每一輪都看不出來，直到再改一次。單次傳輸也會這樣，但 watch 時讀到正在改的檔案的機會多很多。`--modify-window -1` 比到奈秒。
4. **變小的檔案算錯誤。** 讀的時候變小的檔案，例如被截斷的 log，現在回報為錯誤（23）。watch 時，每一輪都可能遇到。
5. **共用檔案系統的負擔。** 重掃時，每一輪兩邊都列出整棵樹，每個 entry 都 stat。在 nano4 的 WekaFS 或 fatcat 的 NFS 上，即使什麼都沒變，這也是 metadata server 的固定負擔；`--watch 0` 時是不停地掃。DESTINATION 在這些檔案系統上時，照第 11 題 (B)，每次有改變也要列出整棵 DESTINATION。
6. **host 上的程序一直在。** 整個 watch 期間，SSH 連線、host 上的 monitor，以及遠端 SOURCE 的 watcher 都在。nano4 的登入節點對長時間的程序或連線有沒有限制，沒查過。
7. **watch 的數量。** 在 Linux，一個目錄一個 watch，上限是每個使用者的，同一使用者的其他程式（例如編輯器）也在用。

## 不做

- 兩向同步。

## 計畫

每一步各自 commit，不 push。

1. **monitor 分輪**：watch 時，client 的 monitor 在這一輪的 server 都做完時，說「這一輪結束」，而不是整個傳輸結束；watcher 是活過整個 watch 的 worker，不算在哪一輪裡。不用 `--watch` 時行為不變。
2. **prepare 拆開**：只做一次的部分（登入、probe、`host:~`、DESTINATION 放在哪裡、安裝、host 上的 monitor、sudo），和每一輪的部分（啟動 server、`plan::agree`、streams）。危險的 DESTINATION 只在第一輪問。不用 `--watch` 時行為不變。
3. **`--watch SECONDS`，只有計時器**，每個時間點都傳一輪：
   - 參數，整數秒，0 以上；拒絕 `--dry-run`；
   - 一輪做完就繼續，一輪沒做完就結束，結束碼是那一輪的；Ctrl-C 是 130；
   - 沒有 UI 時，每一輪有東西才印；dashboard 暫時每一輪各開一次；
   - 測試：本機的，和經過 host（container）的：改檔、新增、`sync` 刪檔都在下一輪到 DESTINATION；一輪 23 之後繼續；兩輪之間按 Ctrl-C；連線斷了，watch 結束。
4. **watcher**：
   - server 的第三種角色 `Role::Watcher`，同 Source、Destination，由 host 上的 monitor 或本機的 client 啟動；沒有 stream，只有控制連線。
   - 開始時看 SOURCE 的根，以及根底下每個 mount point 的檔案系統，從掛載表看（Linux 的 `/proc/self/mountinfo`，macOS 的 `getmntinfo`），不必走過整棵樹；`-x` 時只看根的。
   - 都支援：加 watch（Linux 一個目錄一個，macOS 在根目錄一個），都加好了才說準備好，第一輪才開始。有事件就說改了；佇列滿出來、要重掃子目錄，也說改了。
   - 有一部分不支援，或 watch 數到了上限：說是哪個路徑、為什麼，就不再看，之後每個時間點都傳一輪（第 13 題）。
   - client：時間點上有改變才傳。改變在一輪開始時清掉，一輪進行中來的事件算下一輪的。
   - 測試：事件觸發一輪；不支援（測試注入檔案系統的種類）；watch 數到了上限。
5. **輸出**：dashboard 整個 watch 都在；兩輪之間顯示上一輪的結果，以及在等事件，還是等下一個時間點；開始時說用事件還是重掃。
6. **文件**：overview、cli、architecture、protocol，連同 zh-TW；CHANGELOG；future.md 記只比對改了的部分（第 11 題的 (C)），以及照規則過濾事件。
7. **量**：一棵沒改的樹一輪要多久、每一輪啟動 server 要多久、加好所有 watch 要多久；本機、fatcat、nano4。

第一版的限制：

- 被規則排除的路徑有事件，也會觸發一輪，那一輪什麼都不傳。
- 被排除的目錄底下有不支援的 mount point，也會改成重掃。

## 進度

2026-11-03，第 1–6 步做完，各自 commit，沒有 push：

1. monitor 分輪：4e0f315。
2. prepare 拆開：8658a2c。
3. `--watch SECONDS`，只有計時器：02dd884。
4. watcher：5926808。
5. 輸出：0102835。
6. 文件：cli、overview、architecture、protocol、dashboard、future，連同 zh-TW；CHANGELOG、README。

第 7 步（量）：本機與 fatcat 量了，nano4 沒量，因為沒有 ControlMaster，新的連線要 OTP。

### 量到的

script 在 `.scratch/watch-measure/measure.py`：`sync --watch 0`，從開始到 watcher 說 watching 的時間；然後每隔 1 秒在 SOURCE 寫一個新檔，量到那一輪的摘要印出來為止的時間，各 8 次。樹是每個目錄 10 個 1 byte 的檔。release build（4ecee8d375c9c014），2026-11-03。

**本機到本機**（`/home`，ext4，cache 是熱的；這台的負載約 22／96 核，是使用者自己的訓練程式）：

| 檔案數 | 到 watching | 第一輪結束 | 改了到那一輪結束（中位數，最小～最大） |
|---|---|---|---|
| 1 | 0.02 s | 0.14 s | 0.119 s（0.111～0.125） |
| 10,000（1,000 個目錄） | 0.04 s | 0.44 s | 0.196 s（0.177～0.278） |
| 100,000（10,000 個目錄） | 0.19 s | 1.42 s | 0.308 s（0.294～0.375） |

其中 0.05 s 是 watcher 把改變攢在一起的時間。

**本機到 fatcat**（DESTINATION 在 fatcat 的 `/tmp`；fatcat 的負載約 37～47／56 核，142 個使用者，可用記憶體約 4 GB）：

| 檔案數 | 到 watching | 第一輪結束 | 改了到那一輪結束（中位數，最小～最大） |
|---|---|---|---|
| 1 | 0.07 s | 3.51 s | 1.376 s（0.570～5.338） |
| 10,000 | 0.08 s | 0.83 s | 2.911 s（1.573～9.753） |
| 100,000 | 0.23 s | 2.67 s | 2.769 s（1.286～7.933） |

fatcat 的數字很分散。同一時段，單次 `sync` 1 個檔到 fatcat 花 0.83～4.36 s（6 次）；`--profile` 顯示 fatcat 上 Destination 的 compare 與 apply 各「執行」了 0.8～1.5 s，1 個檔不需要這麼久。所以這些數字反映的主要是 fatcat 當時很忙，不是每一輪的成本；fatcat 空閒時沒有量。

**真的 NFS**：這台機器的 `/nas_data` 是 nfs4。以 `sync --watch 5 --exclude '*' /nas_data/tanch/ ...` 試過（全部排除，只讀根目錄），watcher 說它在 nfs4 上、改成每 5 秒一輪，結束碼 130。

「每一輪啟動 server 要多久」沒有單獨量：1 個檔的樹，本機一輪約 0.07 s（0.119 s 減掉 0.05 s 的攢），包含啟動兩個 server 與建立 stream。

### 和決定不同的地方

1. **Linux 不用 notify（第 17 題）。** notify 的 inotify watch 包含 `IN_OPEN`：每一輪自己讀檔、開目錄都會被當成改變，一輪接一輪停不下來；大樹還會讓事件佇列滿出來，又觸發一輪。所以 Linux 改用 rustix 的 inotify，自己決定看哪些事件，不看打開與讀取；rustix 本來就是相依套件。macOS 照決定用 notify（FSEvents）。
2. **notify 帶進 ISC 授權。** notify 在 Linux 上依賴 inotify 與 inotify-sys，它們是 ISC 授權，不在 `deny.toml`、`about.toml` 允許的清單裡。兩者的 target 都同時有 Linux 與 macOS，所以只在 macOS 用的 notify，它在 Linux 才用的這兩個也被算進去，雖然它們不會編進任何一個執行檔：`cargo deny` 與 `make licenses` 都失敗。第 19 題決定 (b)，兩個檔案都只對這兩個 crate 例外；`THIRD-PARTY-LICENSES` 因此也列出它們的 ISC 條文。
3. **SOURCE 不是目錄時**，watcher 看的是裝著它的目錄，所以同一個目錄裡其他東西改變，也會觸發一輪（那一輪什麼都不傳）。
4. **SOURCE 本身被刪除或移走時**，watcher 的 watch 也跟著沒了：watcher 說一次改了，然後改成每個時間點都傳一輪，和不支援時一樣。之後那一輪讀不到 SOURCE，watch 以 23 結束；SOURCE 已經重建的話，就照時間點繼續。
5. **沒有改變的一輪**，看的是複製、刪除、失敗、讀不到、消失的數量都是 0；只改了權限或 mtime 的一輪，也算沒有改變，不印任何東西。
6. **沒有終端機時**，每一輪的摘要照單次傳輸的規則，只在 `--progress` 時印；警告與讀不到、複製不了的訊息照樣印。
7. **watch 結束時**（第 8 題沒寫到）：有儀表板時，先印期間說過的話，再印最後一輪的摘要，結果是 watch 怎麼結束的（例如 stopped）。`--profile` 的「時間花在哪裡」，有儀表板時只在結束時印最後一輪的。
8. **host 上的 monitor 在它啟動的行程結束後就放掉它**，單次傳輸也一樣；client 的 monitor 的 scope 改用不重複的 ID 存放，一輪結束就放掉那一輪的。不然跑很久的 watch，monitor 的狀態會一直長大。

### 已決定的問題：授權

#### 19. notify 帶進來的 ISC 授權

- **(a) 允許 ISC**：加進 `deny.toml` 與 `about.toml` 的允許清單。ISC 是 OSI 認可的寬鬆授權，內容和 MIT 相近。
- **(b) 只對 inotify、inotify-sys 例外**：`deny.toml` 的 `exceptions`；`about.toml` 也要對應設定。它們不會編進任何一個執行檔。
- **(c) macOS 也不用 notify**：自己寫 FSEvents 的綁定，經過 CoreServices 與 run loop；這台機器只能做型別檢查，要靠 CI 的 macOS 測試。
- 建議 (b)。2026-11-03 使用者決定 (b)。
