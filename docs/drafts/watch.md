# `--watch`：持續看著 SOURCE，改了就再傳一次

- 狀態：草稿，討論中
- 開始：2026-11-03；最後修改：2026-11-03
- 會改動：
  - `docs/design/overview.md`「Scope: one-way only」：「continuous watching of files for changes」目前列為非目標，要拿掉；
  - `docs/cli.md` 的 Options、Output、Exit status；
  - `docs/design/architecture.md`：place（SSH 連線、host 上的 monitor）目前跟著一次傳輸結束，要改成活得比一次傳輸久；
  - 以上都連同它的 `.zh-TW.md`。
- 會改的程式：`src/cli.rs` 的 `transfer`；`src/prepare/mod.rs` 的 `prepare`，拆成只做一次的部分與每一輪的部分；`src/monitor/client`；dashboard 與 `progress`。
- 相關：[Future improvements](../design/future.md)。

## 要推翻的決定

`docs/design/overview.md` 的「Scope: one-way only」把「continuous watching of files for changes」列為非目標。採用這份提案，這一條就要拿掉。

兩向同步仍是非目標：`--watch` 只做單向，讓 DESTINATION 跟著 SOURCE。

## 提議

`copy` 和 `sync` 都加上 `--watch`：

```text
ferry sync --watch ./proj/ host:proj
```

1. 第一輪就是現在的一次傳輸。
2. 傳完之後不結束，等一段時間（間隔），再傳一輪。每一輪都是完整的一次傳輸：Source 列出整棵樹，Destination 比對、決定、複製；`sync` 也刪除。
3. 一直跑到 Ctrl-C，或某一輪失敗。

rsync 沒有這種選項：3.2.7 的 `--help` 裡沒有 watch 或 repeat。所以沒有 rsync 的行為可以對照，以下參考其他工具。

## 別人怎麼做

| 工具 | 怎麼知道改了 | 知道之後 |
|---|---|---|
| lsyncd | inotify 或 fsevents | 事件累積到 `delay` 秒，或 1000 個事件，才呼叫 rsync；啟動時先完整 rsync 一次 |
| Mutagen | macOS：在根目錄設一個遞迴的 watch。Linux：輪詢，預設每 10 秒，加上對最近改過的內容設少量 inotify watch。native watching 失敗時退回輪詢 | 文件沒寫 |
| Syncthing | 檔案系統的 watcher，加上每小時一次完整掃描 | 改變累積 10 秒才掃描，刪除再延 1 分鐘；文件建議 watcher 開著也保留完整掃描，因為 watcher 可能漏掉改變 |

Mutagen 在 Linux 不用 inotify 看整棵樹，它的文件說原因是：inotify 不遞迴，每個被看的檔案或目錄都要開一個 watch descriptor。

來源：

- [lsyncd：layer 4 設定](https://lsyncd.github.io/lsyncd/manual/config/layer4/)、[設定檔](https://lsyncd.github.io/lsyncd/manual/config/file/)
- [Mutagen：watching](https://mutagen.io/documentation/synchronization/watching/)
- [Syncthing：syncing](https://docs.syncthing.net/users/syncing.html)、[config](https://docs.syncthing.net/users/config.html)

unison 也有 `-repeat`，但它的手冊網址這次抓不到（404），沒有查證，表裡不列。

## 待決定的問題

### 1. 主要用途

以下幾題的建議，取決於主要怎麼用：

- **本機改，傳到叢集**，例如在筆電改程式碼，同步到 nano4：SOURCE 在本機磁碟，樹不大，延遲要短。
- **叢集上產生，拉回來**，例如計算節點寫實驗結果，同步回本機：SOURCE 在 WekaFS 或 NFS 上，由別台機器寫入。
- 兩種都要。

### 2. 怎麼知道 SOURCE 改了

- **(A) 輪詢**：上一輪結束後等一個間隔，再完整傳一輪。
- **(B) 事件觸發**：inotify（Linux）或 FSEvents（macOS）說有改變，累積一小段時間後完整傳一輪；另外定時完整傳一輪，補事件漏掉的。
- **(C) 事件，只比對改了的部分**：只列出、只比對事件說改了的目錄。

**事件的限制**：

- inotify 一個目錄要一個 watch。這台機器（kernel 5.15）的 `max_user_watches` 是 1048576；5.11 以前的 kernel 預設 8192。設 watch 要先走過整棵樹。
- NFS 上，inotify 只看得到經過這台機器 kernel 的改動；別的 client 或 server 本身做的改動看不到。WekaFS 沒查過。
- SOURCE 在遠端時，watcher 要在那台機器上跑。所以 Source server 要在兩輪之間活著，或者另外起一個程序。
- Linux 和 macOS 要各寫一套。

**(A) 的代價**：

- 樹沒改，每一輪兩邊還是各列一次整棵樹。一棵沒改的樹一輪要多久，還沒量。
- 延遲最多是間隔加上一輪的時間。

**(C)** 要新的協定：部分的 manifest，以及 Destination 只就一部分決定，`sync` 的刪除也只限那一部分。現在的設計是 Destination 拿整份 manifest 決定。

**建議 (A)**：

- 一輪就是一次傳輸，不加新的比對方式。沿用的模式是「Destination 拿整份 manifest 決定」。
- 網路檔案系統、遠端 SOURCE 都能用。
- (B) 之後再加，只改「什麼時候開始下一輪」，不改一輪做什麼。記到 future.md。

如果第 1 題的答案是「本機改，傳到叢集」，(B) 的延遲優勢比較重要，這個建議要重新考慮。

### 3. 兩輪之間留著什麼

- **(i) 每一輪重新登入**，等於在 shell 裡反覆執行 `ferry sync`。
- **(ii) 留著 place，每一輪啟動新的 server**。只做一次的：登入、probe、DESTINATION 放在哪裡、安裝、host 上的 monitor、sudo。每一輪的：經過 host 上的 monitor 啟動兩個 server（不開新的 session）、`plan::agree`、streams、follow。
- **(iii) 連 server 都留著**，每一輪告訴它們重新開始。

各自的事實：

- (i)：nano4 新的連線要 OTP，一個 session 要 10–16 秒。(i) 在 nano4 不能用。
- (ii)：現在 `transfer` 在 `client.wait()` 之後就關掉 place（`drop((streams, places))`），host 上的 monitor 跟著結束。要改成整個 watch 結束才關。每一輪啟動 server 要多久，還沒量。
- (iii)：省下每一輪啟動 server 和建立 streams 的時間；(B) 在遠端 SOURCE 也需要 Source 活著。但要新的控制訊息讓 server 開始下一輪，以及把每個 server 的狀態歸零。

**建議 (ii)**：一輪就是一次傳輸，只有 place 活得比它久。

**DESTINATION 放在哪裡**：建議第一輪決定，之後每一輪照用，不重新 probe。每一輪的 server 照舊檢查自己的根，所以 SOURCE 的根中途換了種類，那一輪會失敗，見第 5 題。

兩輪之間沒有傳輸，但 client 的 monitor 和 host 的 monitor 之間照舊每 5 秒互送一次心跳，30 秒什麼都沒收到就判定那台 host 失去（architecture.md 的 transport 第 11 點）。所以連線在兩輪之間斷了，也會發現。

### 4. 連線斷了

現在的規則：一台 host 一條 SSH 連線，斷了傳輸就失敗，不重新登入（architecture.md「One SSH connection to a host, and a drop fails the transfer」，2026-10-25 決定）。

- **(a) 照舊**：watch 以失敗結束。
- **(b) watch 時重新登入**：在 nano4 要再輸入一次 OTP。

**建議 (a)**：沿用 2026-10-25 的決定。代價是長時間跑的 watch 一斷就停，要自己重新執行。

### 5. 某一輪出錯

- **一輪有做完**，但有些沒複製、沒刪除，或讀的時候消失了（23、24）：繼續下一輪。watch 時檔案常常正在被寫，這種情況會比單次傳輸常見。
- **一輪沒做完**：傳輸失敗（1），例如磁碟滿、連線斷了；或 SOURCE 本身讀不到（23，什麼都沒做）。watch 結束，結束碼就是那一輪的。

**建議如上**。要考慮的一種情況：建置工具先刪掉再重建 SOURCE 時，剛好碰到的那一輪讀不到 SOURCE，watch 就結束了。

### 6. 結束碼

watch 正常結束的唯一方式就是 Ctrl-C，現在 Ctrl-C 的結束碼是 130（128 + signal）。

- **(a) 一律 130**，同現在的表。
- **(b) 在兩輪之間按 Ctrl-C，傳回最後一輪的結束碼**（0、23、24）；一輪進行中按，才是 130。

**建議 (a)**：不加規則。

### 7. 間隔

- 從上一輪結束算起，兩輪不會重疊。一輪很久的樹，自然就跑得少。
- 預設值：Mutagen 輪詢預設 10 秒。建議也用 10 秒。
- 寫法：
  - `--watch` 用預設；`--watch=SECONDS` 指定。一定要寫 `=`，否則 `ferry sync --watch 5 src dst` 的 `5` 是間隔還是 SOURCE 分不清楚。
  - 或另一個選項 `--watch-interval SECONDS`。
- **建議第一種**，少一個選項。

### 8. 輸出

- **有終端機**：dashboard 整個 watch 都在。一輪進行中，照現在顯示；兩輪之間，顯示上一輪的結果，以及下一輪什麼時候開始。現在的 dashboard 跟著一次傳輸結束，結束後印摘要。
- **沒有終端機，或 `--no-ui`**：一輪有複製、刪除或出錯時，印出那一輪的摘要和警告。什麼都沒變的一輪不印，同現在「An unchanged tree prints nothing」。
- **`--profile`、`--profile-file`**：每一輪寫一次，檔案留最新一輪的；或者 watch 時拒絕。**建議前者**：一輪就是一次傳輸，不加拒絕的規則。

### 9. `--dry-run` 加 `--watch`

dry run 什麼都不改，每一輪都會列出一樣的東西。**建議拒絕**（結束碼 2），同 `copy --delete-excluded` 被拒絕的做法。

### 10. `sync` 問危險的 DESTINATION

只在第一輪問一次。沒有終端機時照舊拒絕，除非給了 `--allow-dangerous-destination`。

## 風險

1. **SOURCE 誤刪，一個間隔之後 DESTINATION 也刪了。** `sync --watch` 在一個間隔內，就把 SOURCE 的刪除帶到 DESTINATION；單次的 `sync` 至少要有人再下一次指令。rsync 有 `--max-delete`，Ferry 沒有。Syncthing 把刪除多延 1 分鐘。
2. **寫到一半的檔案。** 一輪讀到正在寫的檔案時，只讀到打開那時的大小。DESTINATION 會有一個完整、驗證過，但只有前半段內容的檔案，直到下一輪。這個檔案保留打開時的 mtime，所以下一輪會再複製。這是現有的行為，watch 讓它變得常見。
3. **同一秒內、大小沒變的改動。** 預設 mtime 比到秒（overview「Every file is complete and correct」第 2 點）。一輪複製之後，同一秒內又改了一次，而大小沒變，之後每一輪都看不出來，直到再改一次。單次傳輸也會這樣，但 watch 時讀到正在改的檔案的機會多很多。`--modify-window -1` 比到奈秒。
4. **變小的檔案算錯誤。** 讀的時候變小的檔案，例如被截斷的 log，現在回報為錯誤（23）。watch 時，每一輪都可能遇到。
5. **共用檔案系統的負擔。** 輪詢時，每一輪兩邊都列出整棵樹，每個 entry 都 stat。在 nano4 的 WekaFS 或 fatcat 的 NFS 上，即使什麼都沒變，這也是 metadata server 的固定負擔。
6. **host 上的程序一直在。** 整個 watch 期間，SSH 連線和 host 上的 monitor 都在。nano4 的登入節點對長時間的程序或連線有沒有限制，沒查過。

## 不做

- 兩向同步。
- 只比對改了的部分（第 2 題的 (C)）。

## 計畫

照上面的建議，等決定之後再細分：

1. place 的生命週期和一次傳輸分開：`prepare` 拆成只做一次的部分和每一輪的部分；client 的 monitor 每一輪等那一輪的 server 結束。
2. `--watch[=SECONDS]`：迴圈、間隔、只在第一輪問、拒絕 `--dry-run`、結束碼。
3. 輸出：dashboard 跨輪；沒有 UI 時每輪一段。
4. 測試：本機的，和經過 host（container）的：
   - 改檔、新增、`sync` 刪檔，都在下一輪到 DESTINATION；
   - 一輪 23 之後繼續；
   - 連線斷了，watch 結束；
   - 兩輪之間按 Ctrl-C。
5. 量：一棵沒改的樹一輪要多久，每一輪啟動 server 要多久；本機、fatcat、nano4。
6. 文件：overview、cli、architecture，連同 zh-TW；CHANGELOG。
7. (B) 事件觸發，記到 future.md。
