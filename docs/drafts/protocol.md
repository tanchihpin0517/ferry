# 傳輸協定：連線、pipe 與封包

- 狀態：草稿
- 開始：2026-10-28；最後修改：2026-10-28
- 寫什麼：[pipes](pipes.md) 提案之後的協定，從程式寫進 pipe，到連線上的每個 byte。pipes.md 寫為什麼這樣設計，這份寫協定長什麼樣；和現在不同的地方，列在[第 10 節](#10-和現在的差別)。
- 未定的：標「待決定」，答案在 [pipes 的待決定](pipes.md#待決定的問題)。[第 6 節](#6-封包草案)的封包格式整節是草案。
- 會改動：採用後，併入 `docs/design/architecture.md` 的 Transport、Monitors 與 Packets over the stream，以及它的 `.zh-TW.md`。

## 1. 分層

![分層：寫的一方與讀的一方，各有四層。程式，peer、client 或 monitor，有什麼寫什麼，寫不下就等，不知道一次寫得下多少；讀的一方有什麼讀什麼，自己解出記錄。pipe：每種連線一組，用途與編號固定，單向、有序、可靠，讀到 End 才完整。transport：送出 buffer、壓縮、pump 排程與切成封包、守窗口；接收 thread 只分流，再解壓，讀走多少還多少窗口。連線，即 wire：準備時是原樣的 bytes，ready 之後是封包，經 SSH channel、socket pair 或 pipe。準備的 bytes 不走這條路；本機不壓縮，跨網路的每條 pipe 都壓。](assets/protocol-layers.svg)

- **程式**：peer、client、monitor。寫：`write(&[u8])`，全部收下才回來，寫不下就等，不知道一次寫得下多少。讀：有什麼讀什麼，自己解出一筆筆記錄。程式看不到封包、壓縮與窗口。
- **pipe**：一種內容一條，單向、有序、可靠。寫的一方寫完就關閉，transport 送出它的 End；讀的一方讀到 End，這條 pipe 才完整。每種連線一組 pipe，用途與編號固定（[第 5 節](#5-ready每種連線的-pipe)）。
- **transport**：送出端有每條 pipe 的送出 buffer、壓縮（[第 7 節](#7-壓縮)）與 pump，pump 排程、切成封包、守窗口（[第 8 節](#8-窗口)）；接收端的 thread 只把封包分進各 pipe，從不等程式。
- **連線**，即 wire：一條 SSH channel、socket pair 或 pipe。準備時原樣讀寫 bytes；ready 之後，上面只有封包。

## 2. 連線的狀態

2026-10-28 使用者定的：狀態要明確，ready 之後才走壓縮的路；ready 之前的準備，用簡潔的方法就好。

![連線的狀態，兩端各有自己的一份，只往前走。準備：原樣的 bytes，到達、說明用途、交接、交換設定，沒有 pipe、窗口與壓縮。兩端都是最終的程式、設定交換過，經 Wire::stream() 進入 ready：有 pipe、窗口與壓縮，程式只在這裡讀寫，封包是 pipe、kind、長度與內容。每條 pipe 都讀到 End，兩個方向的 pipe 都結束，連線才結束。準備時斷了，或 ready 時在 End 之前斷了，就是失敗：傳輸失敗，不重連。每一端只有一個轉換點。](assets/protocol-states.svg)

- **準備。** 連線交到最終的程式手上，並交換 ready 要用的設定：到達、說明用途、交接、交換設定。原樣的 bytes，沒有 pipe、窗口與壓縮，照現在的做法（[第 4 節](#4-準備)）。
- **ready。** 兩端都是最終的程式、設定也交換過了，transport 把連線轉成這種連線的一組 pipe。每一端只有一個轉換點，就是現在的 `Wire::stream()`（`src/transport/wire.rs`）。之後才拿得到 pipe，寫進去的才有窗口與壓縮。
- **結束。** 兩個方向的 pipe 都讀到 End，連線才結束（[第 9 節](#9-結束與失敗)）。
- **失敗。** 準備時斷了，或 ready 時在 End 之前斷了：傳輸失敗，不重連。唯一的例外在準備的第一步：直連 socket 的 channel 沒等到 start mark，表示 SSH 沒有 forward 到 socket，client 改試 port。
- 兩端各有自己的狀態，不必同時轉換：例如 control connection 上，server 寫完答案就 ready，client 讀到 end mark 才 ready。

## 3. 三種連線

| 連線 | 兩端 | 怎麼建 | 準備帶的設定 | pipe |
|---|---|---|---|---|
| link | client 的 monitor 與主機的 monitor | `ssh -W` 連到 monitor 的 socket，或連 port | `Follow` 的壓縮 | control |
| control connection | client 與每個 server | 主機：同上，再由 monitor 交給它起的 server；本機：server 子程序的 stdin 與 stdout | `Hello`：build、角色、根目錄、選項、壓縮 | control、reports |
| stream，K 條 | Source 與 Destination 的 server | client 做 socket pair，兩端各交給一個 server | client 寫進去的 header：編號、壓縮 | control、manifest、data、answers |

不走 pipe 的連線，first session、lifeline、sudo 檢查、fixture 與交接 fd 的連線，見 [pipes 的表](pipes.md#哪些連線走-pipe)。

## 4. 準備

![三種連線的準備，原樣的 bytes，箭頭的終點是讀走那一段的程式。a，link：client 的 monitor 寫 01，或 port 的 secret，monitor 裡的 listener 讀到就回 start mark；client 再寫 Follow，帶 link 的壓縮設定；之後 ready，link 的 control pipe。b，主機上的 control connection：client 寫 01 或 secret，listener 回 start mark；client 寫 Launch，monitor 讀，把連線交給經 sudo 新起的 server；要密碼時 client 寫密碼行，sudo 讀；client 寫 Hello，帶 build、角色、根目錄與這條連線的壓縮設定，server 讀；server 回 start mark、Welcome 與 end mark；之後 ready，reports 與 control。client 依序寫出，不必等。本機沒有 listener、monitor 與 sudo，只有 Hello 與答案。c，stream：client 做 socket pair，先寫進兩端 01 或 secret，以及編號與壓縮等級的 header；再把兩端交出，本機經 server 的 socket 給 fd，主機接上 ssh -W；兩頭各寫一個 start mark 給對面，主機上由 server 的 listener 寫，本機那頭由 client 代寫；server 讀完 header、跳到對面來的 start mark，就 ready：manifest、data、answers 與 control。](assets/protocol-prepare.svg)

### 4.1 用到的元素

| 元素 | bytes | 誰寫，誰讀 |
|---|---|---|
| `STRAIGHT` | 1 byte，`01` | client 寫；socket 的 listener 讀：這條是直連來的，回 start mark |
| port 的 secret | 64 個十六進位字元 | client 寫；port 的 listener 讀，不對就斷線 |
| 交接 | 1 byte `00`，加兩個 fd（SCM_RIGHTS） | client 交出本機 stream 的一端，或 join 程序交出自己的 stdin 與 stdout；收下的一方回 1 byte |
| start mark | `\0ferry: start\0`，14 bytes | 對面寫：「從下一個 byte 起，寫的是最終的程式」；讀的一方跳到它，丟掉之前的 |
| end mark | `\0ferry: end\0`，12 bytes | server 寫在答案之後；client 確認答案沒有被插話 |
| 訊息 | 5 bytes 的 header，類型 1 byte、長度 4 bytes little-endian，加 postcard | `Follow`、`Launch`、`Hello`、`Welcome`，照現在的格式 |
| 密碼行 | 密碼加換行 | client 寫；`sudo -S` 讀，只有 `--remote-sudo` 要密碼時 |
| stream 的 header | `[編號][壓縮]`，2 bytes | client 寫；server 讀。現在只有編號 |

### 4.2 link

1. client → listener：`01`，或 port 的 secret。
2. listener → client：start mark。沒等到，表示 SSH 沒有 forward 到 socket，client 改試 port。
3. client → monitor：`Follow { 壓縮 }`。它說明這條連線是 link：monitor 讀到它之前，還不知道自己會不會一直握著這條連線。
4. 兩端 ready。

### 4.3 control connection

主機上：

1. client → listener：`01` 或 secret；listener → client：start mark。
2. client → monitor：`Launch { id, Server { sudo, port } }`。monitor 起 server，`--remote-sudo` 時經 sudo，把連線交給它當 stdin 與 stdout，自己不再讀。
3. client → sudo：密碼行，只有要密碼時。
4. client → server：`Hello`。
5. server → client：start mark、`Welcome`，或 `Refused`、`Error`，再加 end mark，一次寫完。
6. server 寫完答案就 ready；client 讀到 end mark 就 ready。

client 依序寫出第 2 到 4 步，不必等：每一段由該讀的程式讀走，讀完才輪到下一個。

本機：client 以 pipe 起 `ferry server --lifeline`，只有第 4 到 6 步。

### 4.4 stream

1. client 做一個 socket pair，在兩端各先寫進：主機上的 server，`01` 或 secret；`[編號][壓縮]`；另一頭的 server 在本機時，再寫一個 start mark，因為本機的 server 不先寫東西。
2. client 交出兩端：本機的 server，經它的 socket 交 fd；主機上的 server，由 `ssh -W` 連到它的 socket，socket pair 的一端當 `ssh` 的 stdin 與 stdout。兩台主機之間，兩條 channel 就這樣背對背接起來，由 kernel 搬 bytes，client 不經手。
3. 主機上的 server，由 listener 讀 `01` 或 secret，回 start mark；它經 socket pair 到對面的 server。
4. 每個 server 讀 `[編號][壓縮]`，跳到對面來的 start mark，就 ready。

K 條 stream 各做一次，編號 0 到 K−1；data 以外的 pipe 都走第 0 條。

### 4.5 `--remote-join`

給 SSH 既不 forward 到 socket、也不 forward 到 port 的主機用，由使用者以 `--remote-join` 指定。channel 經過主機的 shell，執行 `ferry server --join <socket>`：join 程序先在 stdout 寫 start mark，再把自己的 stdin 與 stdout 交給 socket（`00` 加兩個 fd）。之後同上，只是沒有 `01`。shell 印的東西在 start mark 之前，讀的一方跳過；shell 留下的程式在 start mark 之後才寫進來的，變成協定錯誤，傳輸失敗。

### 4.6 準備帶的設定

- **規則：每種連線的準備，帶它自己那組 pipe 的設定**（2026-10-28 使用者同意）。link 在 `Follow`，control connection 在 `Hello`，stream 在 client 寫進去的 header。pipe 的用途與編號由連線的種類決定，兩端又是同一個 build，所以不逐條宣告。
- **壓縮由 client 為每條連線決定：**
  - `--no-compress`：都不壓；
  - `--compress-level N`，N 是 1 到 19：每條都以 N 壓，本機的也壓；
  - 否則：連到主機的連線以等級 1 壓，本機的不壓。

  現在的規則是對整個傳輸判斷：兩邊都在本機才不壓。改成每條連線各自判斷，stream 的結果和現在一樣。
- **`Hello` 其他的欄位照舊**：build、角色、根目錄、選項，以及測試用的覆寫。它的壓縮欄位，現在指 stream 上的 block，改成指這條 control connection 自己。

## 5. ready：每種連線的 pipe

每筆記錄是長度（varint）加 postcard；一筆的上限待決定，建議 1 MiB。manifest、data、answers 的記錄見 [pipes 的記錄](pipes.md#記錄)；control connection 與 link 的記錄，就是現在的訊息。編號是這份草案定的。

**stream**

| 編號 | pipe | 方向 | 記錄 | 窗口 |
|---|---|---|---|---|
| 0 | control | 雙向 | 錯誤 | 無 |
| 1 | manifest | S → D | `Root`、`In`、`Entry`、`Listed`、`Unlistable`、`User`、`Group`、`Problem`、`End` | 有 |
| 2 | data | S → D | `File`、`Segment`、`Hash`、`Unopened` | 有；塊分到所有 stream |
| 3 | answers | D → S | `File`、`Attributes`、`Compared`、`Failed`、`NotDeleted`、`NeedsEnd`、`Done` | 有 |

**control connection**

| 編號 | pipe | 方向 | 記錄 | 窗口 |
|---|---|---|---|---|
| 0 | control | 雙向 | client 寫 `Proceed`、`Snapshot`；server 寫每秒的 metrics 與 carried、`Snapshot`、`Failed`、`Error` | 無 |
| 1 | reports | server → client | 計數、dry run 列的路徑、警告、佇列、在傳的檔案、忙什麼、等級；最後是 `Profile` 與 `Done` | 有 |

**link**

| 編號 | pipe | 方向 | 記錄 | 窗口 |
|---|---|---|---|---|
| 0 | control | 雙向 | `Beat`、`Ping` 與 `Pong`、`Stop`、`Look` 與 `Looked`、`Ended`、`NotStarted`、`Paused`、`Resumed`、`Awaiting` | 無 |

## 6. 封包（草案）

![ready 之後的封包與壓縮，草案。a，封包：pipe 一個 byte、kind 一個 byte、長度四個 bytes，接著內容，最多 64 KiB；kind 0 是 Bytes，這條 pipe 的下一段內容；kind 1 是 Window，還這條 pipe 的窗口；kind 2 是 End，這條 pipe 寫完了，data 的帶塊數。pipe 的編號依連線種類固定。b，一條串流壓縮的 pipe，以 manifest 為例：記錄一筆接一筆，壓成一條 zstd 串流，切成好幾個 Bytes 封包；記錄、壓縮的 block 與封包的邊界互不相干。c，data：每塊是序號、原樣或 zstd、長度與內容；一塊可以切成好幾個封包，都在同一條 stream 上，中間可以夾別的 pipe 的封包；塊分到各條 stream，接收端照序號排回。d，第一條 stream 的兩個方向：Source 往 Destination 有 manifest、data 與 control；反方向有 answers、還給 manifest 與 data 的窗口，以及 control。](assets/protocol-frames.svg)

- **格式**：`[pipe：1 byte][kind：1 byte][長度：4 bytes，little-endian][內容]`，內容最多 64 KiB。ready 之後，連線上只有這一種封包。
- **kind**：
  - `0 Bytes`：這條 pipe 的下一段內容；
  - `1 Window`：還這條 pipe 的窗口，內容是還的 bytes 數；
  - `2 End`：這條 pipe 寫完了；data 的帶塊數。
- **串流壓縮的 pipe**：`Bytes` 的內容是這條 pipe 的 zstd 串流的下一段；不壓時就是 pipe 的 bytes。記錄、壓縮的 block 與封包的邊界互不相干。
- **data 的塊**：`[序號：8 bytes][0 原樣、1 zstd：1 byte][長度：4 bytes][內容]`。一塊在一條 stream 上，可以切成好幾個 `Bytes` 封包，中間可以夾別的 pipe 的封包；一條 stream 一次只送一塊。接收端照序號排回順序。
- **走哪條 stream**：data 的塊分到各條；其他 pipe 的封包，以及所有的 `Window`，都走第 0 條。`Window` 走它那條 pipe 的反方向。
- **排程**：control 與 `Window` 最先；answers 與 reports 其次；data 與 manifest 之間照現在的規則，data 的 buffer 至少四分之一滿時 data 先，否則 manifest 先。

## 7. 壓縮

- 以連線為單位設定（[4.6](#46-準備帶的設定)）。跨網路的每條 pipe 都壓，沒有例外；本機不壓（2026-10-28 使用者決定）。
- **串流的 pipe**：manifest、answers、reports、control，每條一個 zstd 串流，寫的一方一個壓縮狀態，讀的一方一個解壓狀態。flush 的規則待決定：小而頻繁的寫入不能每次都 flush，否則反而變大。
- **data**：切成塊，建議 2 MiB，由一組 thread 平行壓縮與解壓，建議 8 條；壓了不會變小的塊原樣送。塊多大、幾條 thread 待決定。
- 完整性檢查不在這裡：檔案的 BLAKE3 hash、分段的 chaining value，由 peer 算與驗證。

## 8. 窗口

- 有窗口的是 manifest、data、answers、reports；control 沒有。
- 窗口由讀的一方給，單位是連線上的 bytes，也就是封包的內容，壓縮之後的。
- 寫的一方用完窗口就等；讀的一方的程式讀走多少，transport 就還多少，攢到窗口的四分之一再送 `Window`。
- 接收 thread 從不等程式：對方不會超過窗口，接收 buffer 一定放得下。
- 初始值：data 用現在的資料額度 64 MiB；manifest、answers、reports 待決定。

## 9. 結束與失敗

- 寫的一方寫完一條 pipe 就關閉，transport 送出它的 `End`；讀的一方讀到 `End`，這條 pipe 才完整。
- 一條連線上，兩個方向的 pipe 都 `End` 了，transport 才關掉連線。
- 連線讀到結尾時，對方寫的 pipe 還有沒 `End` 的，就是斷了：傳輸失敗，不重連。K 大於 1 時，第 1 條以後的 stream 只帶 data 的塊：斷在一塊的中間，或 data 的 `End` 說的塊數沒有到齊，就是斷了。
- 對方是否還在，照舊由 monitor 的 heartbeat 判斷（architecture 原則 10）；連線上任何 bytes 進來都算聽到。
- 各連線什麼時候結束，照現在：server 的 peer 做完，送 `Done`，等 client 結束 control connection；client 的 monitor 在兩個 server 都做完時結束它們；link 在主機上的東西都結束之後才關。

## 10. 和現在的差別

| 現在 | 改了之後 |
|---|---|
| 準備：stream 的 header 只有編號，`Follow` 沒有欄位，`Hello` 的壓縮指 stream 上的 block | header 加壓縮，`Follow` 帶 link 的壓縮，`Hello` 的壓縮指這條 control connection；其餘照舊 |
| ready 之後的封包：`[類型][長度][postcard]`，一種訊息一種類型 | `[pipe][kind][長度][內容]`；訊息是 pipe 上的記錄 |
| stream layer 的 end（類型 10） | 每條 pipe 的 `End` |
| `Credit`（類型 8） | data pipe 的 `Window` |
| manifest 的段、`ManifestStream`（類型 6）、peer 壓 manifest | manifest pipe 上的記錄，壓縮在 transport |
| block、pack、`BlocksEnd`（類型 12）的計數 | data pipe 上的記錄；transport 的塊，與 data 的 `End` 帶的塊數 |
| `FileHash`（類型 7） | data pipe 上的 `Hash` 記錄 |
| control connection：peer 的報告走 mux，metrics 由 `tell` 直接寫，dry run 的路徑每則 1 MiB | reports 與 control 兩條 pipe，一筆一條路徑 |
| 只壓 stream 上的 manifest 與 block | 跨網路的每條 pipe |

## 11. 待決定

都在 [pipes 的待決定](pipes.md#待決定的問題)：

- 封包的格式與 pipe 的編號：這份第 6 節與第 5 節是草案；
- flush 的規則；
- control connection 上的 `Done`：建議在 reports 的最後；
- needs 與 results 合成一條 answers；
- 窗口的初始值；
- data 的塊多大、壓縮與解壓各幾條 thread；
- pack 整個拿掉；
- 一筆記錄的上限。
