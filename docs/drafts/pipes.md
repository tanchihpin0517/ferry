# Peer 寫 pipe，transport 決定怎麼送

- 狀態：草稿
- 開始：2026-10-28；最後修改：2026-10-28
- 會改動：`docs/design/architecture.md` 的「Transport」（原則 2、6、8，以及 control connection 那一段的「It is plain」與「the server's answer between marks」）、「Monitors」（link 上的訊息）與「Packets over the stream」；`docs/design/pipeline.md` 的「Principles」（stage 與上限）、「1. Scan」（manifest 的單位、掃描在什麼時候等）、「2. Compare」、「3. Read and hash」（pack）、「4. Serialise and compress」（block 與 Compression）、「5. Decode」；以上兩份的 `.zh-TW.md`。
- 相關：[架構重新設計](architecture-redesign.md) 的 R4（流量控制散在十幾個常數裡）與 B2（以目錄為單位的 page）；2026-10-28 討論 B2 時，談「大目錄為什麼要切段」談到這裡；同一天又談到 control connection 與 handshake 能不能壓縮，於是把每一種連線列出來，看哪些走 pipe。

## 問題

現在由 peer 決定一則訊息多大，於是這個大小又變成好幾件別的事的單位；peer 之間的流量控制也各自為政。

**1. 訊息的大小是 peer 決定的，而且被拿去當別的單位。**

- 一個封包的 payload 最多 16 MiB（`MAX_PAYLOAD`）。`src/protocol.rs` 的註解列出 peer 為了它各自設的上限：manifest 一段最多 `PART` 10,000 個項目、`PART_BYTES` 4 MiB；一則 Needs 最多一段的 ID；dry run 列的路徑每則 1 MiB（`LISTED_BYTES`）。`src/scan.rs` 的 `bound()` 與 `in_bytes()` 為此估算一段編碼後多大。
- 段接著被當成：
  - stat 的工作單位：少於 10,000 個項目的目錄只有一條 thread 在 stat，10,000 到 20,000 之間的分得不平均；
  - Needs 的單位：Destination 一段送一則；
  - Source 的 `Known` 記錄與釋放的單位；
  - Destination 比較的工作單位。
- 平行 stat 其實不需要段：worker 從整個排好的目錄依序認領就可以。段是為了訊息大小而有的，其他事只是順手用它。

**2. 一種內容讀得慢，整條連線都停。**

Destination 的 receive thread 讀 stream 上的每個封包，manifest 的段交給比較，`parts_in` 只能放 4 段（`src/destination/receive.rs` 的 `PARTS`）。比較變慢時，它卡在 `parts_in.send` 上，整條 stream 都不讀了，資料封包也跟著停。資料額度的註解說它讓 Destination「never stops reading its connection for want of room」，但這只對資料成立。

![a，現在：Destination 的 receive thread 把 manifest 的段交給比較，parts_in 只放得下 4 段；比較變慢，它就卡在 parts_in.send，連資料封包也不讀了，decoder 閒著，stream 塞住，Source 的 manifest 與資料一起停。b，pipe：接收 thread 照常把封包分進各 pipe 的 buffer；manifest 的 buffer 滿了，就是 manifest 的窗口用完，只有掃描在等；資料走自己的窗口，照常送、照常驗證。比較讀走多少，才還多少 manifest 的窗口。](assets/pipes-slow.svg)

**3. 流量控制散在各處，單位各不相同。**

| 上限 | 值 | 單位 | 在哪裡 |
|---|---|---|---|
| 資料額度 | 64 MiB | block 的 bytes，Destination 寫完才還 | `DATA_CREDIT`，`Credit` 封包 |
| 在途的項目 | 262,144 | manifest 的項目，Destination 比完才說 | `IN_FLIGHT_ENTRIES`，`Needs::Through` |
| 送出端的 manifest 佇列 | 8 MiB | bytes | `PACED_BYTES` |
| 送出端的資料佇列 | 256 個 | 64 KiB 的封包 | `DATA_QUEUE` |
| 等著讀的檔案 | 100,000 | 檔案 | `BACKLOG` |

**4. `Through` 一個訊息做兩件事。** 它是流量控制（在途的上限），也讓 Source 放掉 `Known` 裡的路徑。而且只在一個目錄結束時才說，所以一個大目錄裡，在途的上限要到下一個目錄才擋得住。

**5. 壓縮也是 peer 決定的，而且分成兩種做法。**

- manifest 由 Source 送 manifest 的 thread 壓成一條 zstd 串流，每則訊息 flush 一次（`ManifestEncoder`）。
- 資料由 Source 的 peer 一個 block 一個 block 地壓：8 條 thread 各壓一個；Destination 的 8 條 decoder 解壓並驗證 hash（`src/block.rs`）。每個 block 各自壓縮，是為了兩件事：能單獨重送；從好幾條 stream 回來時能照編號排回順序。單獨重送在 A2 的收回時已經拿掉。
- pack 把小檔湊成一個 block，主要也是為了壓縮率與分攤每個 block 的固定成本；它什麼時候封口，又是 peer 的另一條規則（改進選項「小檔的 pack 裝滿再封口」）。

**6. control connection 與 link 另成一套，而且不壓縮。**

- architecture.md 說 control connection「plain」，「a few hundred bytes a second」，所以不壓縮；link 也一樣是不壓縮的訊息。
- server 這一端其實已經有自己的一套多工：peer 的報告走 `mux::channel_for_reports`，dry run 列的路徑要等空間；每秒的 metrics 與 carried 則由 `tell` 加鎖直接寫（`src/peer.rs`）。dry run 的路徑每則 1 MiB（`LISTED_BYTES`），也是 peer 決定訊息大小。
- 量少只是一般情形。2026-10-28 在本機用 strace 記下每次 write 的 bytes，用 zstd 等級 3 壓，比較「每次 write 都 flush」與「整條一次壓」（53,362 個項目、`/usr` 形狀的樹）。每一列是那個方向的全部 bytes：

| 內容 | 原樣 | 每次 write 都 flush | 整條一次壓 |
|---|---|---|---|
| dry run，Destination → client，幾乎都是列的路徑，4,276 次 write | 2.50 MB | 395 KB（6.3 倍） | 330 KB（7.6 倍） |
| 1 萬個讀不到的檔案，Source → client，幾乎都是警告，10,099 次 write | 877 KB | 180 KB（4.9 倍） | 102 KB（8.6 倍） |
| 複製時的報告 6.8 秒，Source → client | 5.9 KB | 4.0 KB | 3.4 KB |
| 同上，Destination → client | 16.0 KB | 8.6 KB | 7.1 KB |
| 沒有變動的再同步，stream 上 Destination → Source，幾乎都是 needs，3,851 次 write | 36.6 KB | 45.9 KB（**變大** 1.25 倍） | 5.1 KB（7.2 倍） |
| client → server：`Hello`、`Proceed` 與結束，3 次 write | 68–113 B | 大 18 B | 大 9 B |

- 前兩列是 control connection 真正量大的時候，壓縮有 5 到 8 倍。
- needs 那一列是反面：小而頻繁的寫入，每次都 flush，zstd 每次的 block header 比省下的還多，反而變大；整條一起壓則仍有 7 倍。所以要壓，就不能每次寫入都 flush。
- client → server 只有幾則，太小，壓了只會變大；`Hello` 值得放進 pipe，是為了規則一致，不是為了省 bytes。

## 提案

### 原則

像寫檔：程式有什麼就寫什麼，`write` 寫不下就把呼叫它的 thread 卡住，等到寫得下為止；程式不知道一次寫得下多少。讀的一方有什麼就讀什麼，自己解出一筆筆記錄，不必等「一則訊息」到齊，因為沒有訊息邊界，只有記錄自己的長度。

切成多大的封包、什麼時候送、怎麼和其他內容交錯、壓不壓縮、對方還收不收得下，都是 transport 的事，peer 看不到。

![a，三種連線，各帶一組 pipe：client 與 Source server 在這台機器，主機上有 monitor 與 Destination server。link 在 client 的 monitor 與主機的 monitor 之間，一條 control pipe。control connection 在 client 與每個 server 之間，reports 由 server 寫給 client，control 雙向；本機那條不壓縮。stream 在兩個 server 之間，1 到 8 條，manifest 與 data 由 Source 寫，answers 由 Destination 寫，control 雙向。first session 經過 login shell，原樣，不走 pipe。跨機器的連線都在同一條 SSH 連線裡，各是一個 channel。b，一條連線裡面，以 stream 為例：Source 的掃描與送資料的 thread 各寫一條 pipe：manifest 與 data，寫不下就等。transport 把每條 pipe 的 bytes 放在送出 buffer：manifest 壓成一條 zstd 串流，data 切塊平行壓縮；pump 排程、切成封包，經 1 到 8 條 stream 送出。Destination 的接收 thread 只把封包分進各 pipe 的接收 buffer，從不等 peer；比較讀 manifest，驗證讀 data。讀走多少，就還多少窗口；窗口用完，寫的一方就等。answers 反方向：決策者寫，Source 讀 answers 的 thread 從不阻塞。control 雙向，沒有窗口，最先送。control connection 與 link 一樣，只是 pipe 不同。](assets/pipes-overview.svg)

### pipe

- **pipe** 是兩支 ferry 程式之間單向、有序、可靠的 byte 串流，一種內容一條：兩個 peer 之間，client 與 server 之間，兩個 monitor 之間。
  - 寫：`write(&[u8])` 全部收下才回來，期間可能阻塞；一次寫多少都可以。
  - 讀：`read(&mut [u8])` 有就回，沒有就等；pipe 結束時回 0。
  - 寫的一方關閉時，transport 送出這條 pipe 的結束。讀的一方讀到結束，才算這條 pipe 完整；連線先斷了，照舊是傳輸失敗。
- pipe 和實體的連線不同。連線是 SSH channel、socket pair 或 pipe；兩個 server 之間的 **stream** 一個傳輸有 1 到 8 條（architecture 原則 4），control connection 與 link 各一條。pipe 是寫的一方看到的邏輯單位，多工在它那種連線上。

### 哪些連線走 pipe

**規則：兩端都是 ferry 的連線，前段一過就走 pipe。** 前段照舊是原樣的 bytes，由現在的程式碼處理：

- **client 寫進去的：** `STRAIGHT` 或 port 的 secret、stream 的編號、monitor 讀的 `Launch`、sudo 讀的密碼行。這些沒有人插話，由 monitor、sudo、server 依序讀走。
- **對面的程式寫出來的：** 到 start mark，`\0ferry: start\0`，為止。start mark 的意思是「從下一個 byte 起是 ferry 寫的」；讀的一方丟掉它之前的東西，例如經過 shell 時 startup files 印的字。

三種連線的兩端各不相同：server 與 server、client 與 server、monitor 與 monitor，合不成一條。所以每種連線各自帶一組 pipe，規則相同。

以一台主機加本機為例，全部的連線如下（2026-10-28 對過 `src/transport/`、`src/monitor/`、`src/prepare/`、`src/peer.rs`）：

| # | 連線 | 兩端 | 跨網路 | 前段 | 走 pipe |
|---|---|---|---|---|---|
| 1 | first session | client 與主機上的 `sh`，之後 exec 成 monitor | 是 | 全程：login shell 的輸出夾著標記行 | 不行 |
| 2 | link | client 的 monitor 與主機的 monitor | 是 | `STRAIGHT` 或 secret；monitor 回的 start mark | 可以 |
| 3 | server 的 control connection，主機 | client 與 server | 是 | `STRAIGHT` 或 secret、`Launch`、sudo 的密碼行；monitor 與 server 各回一個 start mark | 可以 |
| 4 | server 的 control connection，本機 | client 與 server 子程序，經 pipe | 否 | server 回的 start mark | 可以，不壓縮 |
| 5 | sudo 檢查 | client 與 `sudo ferry build-id` | 是 | 全程：sudo 讀密碼行，`ferry build-id` 印一行 | 不必 |
| 6 | fixture，只有 `ferry test` 用 | client 與 `ferry fixture` | 是 | 全程：輸入只用來叫它停，輸出讀到結束 | 不必 |
| 7 | stream，1 到 8 條 | Source 與 Destination 的 server | 看兩端在哪 | `STRAIGHT` 或 secret、stream 的編號；每個方向一個 start mark | 可以 |
| 8 | lifeline | server 或 worker 的 stderr 與 monitor | 否 | 全程：stderr | 不行 |
| 9 | 交接 | client 或 join 程序，到 server 或 monitor 的 socket | 否 | 全程：header 與 fd | 不適用 |

- **1 不行。** 對面一開始是 `sh`，shell 留下的程式隨時可能寫進來，所以 ferry 在這裡說的每句話都是帶 tag 的標記行。exec 成 monitor 之後它不再傳東西，只是開著，關掉表示 client 走了。它的 stderr 接到 client 的終端機，monitor 與 server 的 stderr 也經這裡。
- **5、6 不必。** 對面是 sudo，或只跑一次、輸出原樣讀完的程式。
- **8 不行。** stderr 誰都可以寫；它也不跨網路。
- **9 不適用。** 只傳 fd，沒有資料。
- **不是 ferry 自己的，不在這份草稿：** SSH master 的 TCP 連線（上面跨網路的都是它的 channel）、`ssh -O check`、client 與主機各自用 curl 或 wget 連 GitHub。
- **`--remote-join`。** 這時 2、3、7 經過主機的 shell；architecture.md 已寫明，shell 留下的程式在 start mark 之後仍可能寫進來。走 pipe、壓縮之後，這一樣是協定錯誤、傳輸失敗，沒有新的風險。
- **first session 的上傳**，binary 或原始碼 tar，是唯一不走 pipe 的大量資料。它在 shell 裡，要壓縮只能靠主機上的工具，不在這份草稿。

### 兩個 server 之間的 pipe

| pipe | 方向 | 記錄 | 寫的一方 |
|---|---|---|---|
| manifest | Source → Destination | root、目錄切換、項目、目錄列完、目錄沒有列、owner 的名稱、讀不到的路徑、結束 | 掃描；會被擋 |
| data | Source → Destination | 檔案、分段、分段檔案的 hash、打不開的檔案；結束就是 pipe 的結束 | Source 送資料的 thread；會被擋 |
| answers | Destination → Source | 要的檔案、要的屬性、比較到哪、失敗、needs 結束、done | 決策者的命令；會被擋，但一定會被放行（見[不變量](#不變量不互相卡死)） |
| control | 雙向 | handshake 之後的錯誤 | 不會被擋：沒有窗口，量極少 |

- 分段檔案的 hash、打不開的檔案、blocks 的結束，現在走訊息佇列，靠「訊息在 BlocksEnd 之前」維持順序。放進 data pipe，它們自然排在自己的 block 之後。
- needs 與 results 現在都在訊息佇列，Source 由同一條 thread 讀；`Done` 必須在 results 之後。合成一條 answers pipe，順序自然成立。

### client 與 server 之間的 pipe

主機上和本機都一樣，只差本機不壓縮。

| pipe | 方向 | 記錄 | 寫的一方 |
|---|---|---|---|
| reports | server → client | peer 的報告：計數、dry run 列的路徑、警告、佇列、在傳的檔案、忙什麼、等級；最後是 `Profile` 與 `Done` | 報告；會被擋，同現在 dry run 的路徑等空間 |
| control | 雙向 | client 寫 `Hello`、`Proceed`、`Snapshot`；server 寫 `Welcome` 或 `Refused`、每秒的 metrics 與 carried、`Snapshot`、`Failed`、`Error` | 不會被擋：沒有窗口 |

- **`Done` 在 reports 的最後。** 現在 server 結束時，先等報告寫完，再寫 `Profile`，最後寫 `Done`（`src/peer.rs`），三者在同一條 bytes 上，順序自然成立。拆成兩條 pipe 後，`Done` 若在 control，可能比最後幾筆報告先到。和 needs、results 合成 answers 是同一個理由。
- dry run 的路徑一筆一條，`LISTED_BYTES` 拿掉。
- 現在的兩條路，`mux::channel_for_reports` 與 `tell`，就變成這兩條 pipe。

### 兩個 monitor 之間的 pipe

| pipe | 方向 | 記錄 | 寫的一方 |
|---|---|---|---|
| control | 雙向 | client 的 monitor 寫 `Follow`、`Beat`、`Ping`、`Stop`、`Look`；主機的 monitor 寫 `Beat`、`Pong`、`Ended`、`NotStarted`、`Paused`、`Resumed`、`Awaiting`、`Looked` | 不會被擋：沒有窗口 |

- 只有一條。量很少：heartbeat 每 5 秒一次，`Ended` 帶的 stderr 最多 64 KiB。放進 pipe 是為了規則一致，沒有量過能省多少；本機跑沒有 link。
- 是否還在、多久沒聽到對方，照舊由 monitor 判斷（architecture 原則 10）：pipe 上任何 bytes 進來都算聽到，同現在。壓縮要在閒時立刻送出，否則 heartbeat 與量 RTT 的 `Ping` 會被延後。

### handshake 與 build

- `Hello` 與 `Welcome` 移進 control pipe，在前段之後；server 不再把答案夾在 start mark 與 end mark 之間，end mark 拿掉，start mark 照舊。
- pipe 的格式本身要兩端同一個 build，所以 build 要在進 pipe 之前比對：
  - **主機上幾乎都比對過了。** monitor 在 first session 說自己的 build，不同就拒絕（`src/transport/monitor.rs`）；server 是 monitor 用自己的 `current_exe()` 起的（`src/monitor/host.rs`），安裝的路徑又依 build 分開（`bin/<build>/ferry`）。只有 `--remote-ferry` 指定的檔案在執行中被換掉，才可能不同。
  - **本機沒有比對過。** server 是照路徑再執行一次 client 的 binary；執行中檔案被換掉的話，例如中途 `cargo build`，就可能是另一個 build。
  - 今天這兩種情形都由 `Hello` 的 build 擋下。改成啟動參數 `server --build <id>`，本機與主機都帶；不同時，server 在寫 start mark 之前就結束，原因寫在 stderr。

### 記錄

- 每筆記錄是長度（varint）加 postcard 編碼。一筆的上限例如 1 MiB；超過就是協定違規，讀的一方停止傳輸。讀的一方保留不完整的尾巴，等下一次 `read`。
- 一次 `write` 可以有任意多筆記錄：一個目錄的全部項目可以一次寫，也可以一筆一筆寫。
- **manifest 的記錄：**
  - `Root(Entry)`：第一筆，ID 0。
  - `In { dir }`：之後的項目屬於目錄 `dir`，`dir` 是那個目錄自己的 entry ID，root 是 0。K=1 時每個目錄一次；B2 的 K>1 時，不同目錄的項目可以交錯，每次換目錄寫一次。
  - `Entry(Entry)`：同一個目錄裡名稱嚴格遞增；ID 全域連續，照寫入的順序。
  - `Listed { dir }`：這個目錄的項目都送了。
  - `Unlistable { dir }`：這個目錄沒有列，裡面的東西 Destination 照舊留著：列不出、`-x` 時在另一個檔案系統、或是 Destination 的根。取代現在「列不出的目錄以空目錄送出，Destination 由 manifest 離開了它推斷」。
  - `User { id, name }`、`Group { id, name }`：第一次用到之前送。
  - `Problem { path }`：讀不到的路徑，Destination 計數。
  - `End`。
  - 檢查逐筆做：`In` 只能指向宣告過、還沒結束的目錄；每個宣告過的目錄恰好一個 `Listed` 或 `Unlistable`；`End` 時沒有未結束的目錄。其餘同現在的 `manifest::Check`。
- **answers 的記錄：** `File(id)`、`Attributes(id)`、`Compared { first, last }`、`Failed { … }`、`NotDeleted { … }`、`NeedsEnd`、`Done`。
- **data 的記錄：**
  - `File { id, mtime, bytes, hash }`：一個不分段的檔案，內容與 BLAKE3 hash；讀不完整的，帶著原因而不是 hash，同現在 pack 裡的檔案。
  - `Segment { id, offset, size, mtime, bytes, value }`：大檔的一段，內容與它的 chaining value；讀不完整的同上。
  - `Hash { file, hash }`：分段檔案的 hash，由各段的 chaining value 合成，在它的最後一段之後。
  - `Unopened { file, path }`：打不開的檔案。
  - 不再有壓縮、序號與 pack：記錄沒有壓縮，壓縮是 transport 的事；順序由 pipe 保證；小檔不必湊成 pack，transport 切塊時，連續的小檔自然落在同一塊裡。

### transport 怎麼送

- **送出端。** 每條 pipe 一個送出 buffer，上限由 transport 決定，例如 data 的小一些，留在 cache 裡。pump 從各 buffer 取 bytes，組成封包 `[pipe][length][bytes]`；封包多大是 transport 的事，例如最多 64 KiB。pump 忙的時候，bytes 在 buffer 裡累積，空下來時一次拿走一大塊，所以忙時打包自然發生；閒時什麼時候 flush，見下面的壓縮。
- **排程**沿用現在的規則，它本來就是 transport 的決定：control 最先；answers 其次；data 與 manifest 之間，data 的 buffer 至少四分之一滿時 data 先，否則 manifest 先。client 與 server 之間：control 先，reports 後。
- **壓縮全部在 transport 裡，每條 pipe 怎麼壓由 transport 決定。** peer 從不壓縮。要不要壓、壓縮等級（現在的 `--compress` 與它的等級）是 transport 的設定；本機到本機照舊不壓。
  - **manifest：一條 zstd 串流。** 一條 thread 從送出 buffer 拿到多少就壓多少，buffer 空了就 flush：忙時一大塊，閒時立刻送出。記錄很小，前面的內容就是後面的字典。接收端在讀的一方呼叫 `read` 時解壓。manifest 壓縮每個項目不到 1 µs（`pipeline.md` 的 Compression），搬到這裡成本不變。
  - **data：切成一塊塊，平行壓縮。** transport 把 data pipe 的 bytes 切成一塊塊，例如每塊 2 MiB，每塊各自壓縮，由一組 thread 平行處理，像 pigz 那樣；壓了不會變小的塊原樣送。每塊帶序號；接收端由一組 thread 平行解壓，照序號排回順序，讀的一方看到的仍是一條有序的 pipe。不滿一塊的部分，在寫的一方閒下來時送出，就像現在 pack 在閒著時封口。
  - **其他 pipe 也壓，沒有例外：** answers、reports、control，三種連線上都是，各一條 zstd 串流，同 manifest。量少的時候省得少，但不必為它們另訂規則；量多的時候，例如 dry run 的路徑、大量的警告，有 5 到 8 倍（問題第 6 點）。
  - **不能每次寫入都 flush。** 小而頻繁的寫入每次 flush，反而變大（問題第 6 點的 needs：1.25 倍）。「buffer 空了就 flush」在寫入稀疏時就等於每次 flush，所以 flush 的規則還要再定，見待決定。
- **data 跨好幾條 stream。** 一個傳輸有好幾條 stream 時，transport 把 data 的塊分到各條，就是每塊帶的那個序號讓接收端排回順序。其他 pipe 都走第一條。
- **窗口。** 每條 pipe 有一個窗口，單位是 stream 上的 bytes（壓縮後），由讀的一方給：
  - 寫的一方用完窗口就阻塞；
  - 讀的一方的 peer 每讀走一些，transport 就還回去，攢到窗口的四分之一再送；
  - 初始值：data 用現在的資料額度 64 MiB，要不少於頻寬乘以 RTT；manifest、answers 與 reports 待定（見待決定）；control 沒有窗口，三種連線都是。
- **接收端。** 一條 thread 讀 stream，照 pipe 分進各自的接收 buffer。對方不會超過窗口，所以 buffer 不會滿，這條 thread 從不必等任何 peer。

![a，manifest：掃描寫進來的記錄，In、項目、Listed，由 transport 的一條 thread 壓成一條 zstd 串流；送出 buffer 一直有東西時，一次壓一大塊；buffer 空了就 flush，最後幾筆立刻送出。b，data：檔案與分段的記錄一筆接一筆，transport 把這些 bytes 切成每塊 2 MiB，切點不管記錄的邊界；每塊各自壓縮，由一組 thread 平行處理，帶序號，分到各條 stream；接收端由一組 thread 平行解壓，照序號排回，驗證讀到的仍是一條有序的 bytes。壓了不會變小的塊原樣送；不滿一塊的，寫的一方閒下來時送出。peer 從不壓縮。](assets/pipes-compress.svg)

### 不變量：不互相卡死

兩個方向都會阻塞的 pipe，有互相卡死的風險，就像兩個 process 各自寫給對方、buffer 都滿了。這份設計靠三條不變量避免：

1. **transport 的接收端從不等 peer。** 它只把 bytes 放進 pipe 的接收 buffer，而那裡一定放得下。
2. **窗口在讀走時還，不在處理完時還。** 處理得慢，讀的一方自然少讀，窗口就不還。資料的端到端回壓照樣成立：Destination 讀 data pipe 的 thread 把記錄交給有上限的驗證 pool，pool 滿了它就停下來不讀，窗口不還，Source 就停；解壓的 thread 在 transport 裡，等的也只是讀的一方，不是接收端。這就是現在的資料額度在做的事，只是不必另一種訊息。
3. **等待不成環。** 會被擋的寫入者，只能等一個「從不等它下游」的讀者：
   - Source 讀 answers 的 thread 從不阻塞：收到的交給 owner，用沒有上限的佇列；量由 manifest 的節奏限制，同現在。
   - 所以 Destination 的決策者寫 answers 時，就算一時被擋，也一定會被放行。
   - client 與 server 之間，reports 的讀者是 client，它不等任何 server；peer 寫報告被擋時，最後等的是終端機，同現在 dry run 的路徑等空間。link 上只有 control，沒有窗口。
   - 反例，也是 `BACKLOG` 為什麼要留在 peer：如果 Source 等著讀的檔案太多時，owner 就不讀 answers，那麼決策者寫 answers 會卡住，解碼交不出 block，data pipe 沒人讀，Source 的 data 窗口用完，檔案送不出去，等著讀的檔案永遠減不下來。所以 `BACKLOG` 照舊由 owner 讓掃描等。

![a，允許的等待：箭頭是寫的一方可能等讀的一方。掃描等 manifest 的窗口，也就是比較；送資料等 data 的窗口，也就是驗證；比較與驗證都交給決策者；決策者等 answers 的窗口，也就是 Source 讀 answers 的 thread，而它從不等：收到的交給 owner，佇列沒有上限。每條等待最後都走到一個從不等的讀者，不成環。b，反例：如果等著讀的檔案太多時，owner 就不讀 answers，那麼決策者寫 answers 卡住，驗證交不出記錄，data pipe 沒人讀，Source 的 data 窗口用完、送不出檔案，等著讀的檔案就減不下來，owner 也就一直不讀：成環，誰都等不到。所以 BACKLOG 照舊由 owner 讓掃描等。](assets/pipes-waits.svg)

被擋的寫入者也要能被停止：等窗口和現在等 stream 一樣，都在 scope 裡（architecture 原則 7），scope 結束就返回。

### 拿掉的

- `PART`、`PART_BYTES`、`bound()`、`in_bytes()`，以及 `Manifest::Directory` 的 `more`；
- 每段一則的 Needs；
- `Credit` 封包：由 data pipe 的窗口取代，`DATA_CREDIT` 變成那個窗口的初始值；
- `IN_FLIGHT_ENTRIES`、`THROUGH_EVERY`、`through_every()`、`Wait::InFlight`，以及 hello 裡對應的欄位：流量控制由 manifest pipe 的窗口取代；
- mux 的三個佇列、`PACED_BYTES`、`DATA_QUEUE`：變成 transport 裡各 pipe 的送出 buffer；
- control connection 上的 `mux::channel_for_reports`、`tell` 的鎖與 `LISTED_BYTES`：變成 reports 與 control 兩條 pipe；
- handshake 的 end mark，以及 `Hello` 裡的 build：build 改由啟動參數比對；
- `MAX_PAYLOAD` 註解列的各種上限，只剩一筆記錄的上限；
- 所有壓縮：Source 的 `ManifestEncoder`、Source 壓 block 的 8 條 thread、Destination 的 `ManifestDecoder` 與 decoder 裡的解壓；
- block 的格式：它的序號、`Assembler`、`BlocksEnd` 的計數，以及 pack 與它封口的規則。

### 留在 peer 的

- **完整性檢查。** 每個檔案的 BLAKE3 hash、每個分段的 chaining value，是「檔案完整送到了」的端到端檢查，屬於語意，和怎麼送無關：Source 讀檔時算，Destination 的 peer 驗證，照舊由一組 thread 平行做。現在 Destination 的 decoder 一次做完解壓與驗證；改了之後解壓在 transport、驗證在 peer，中間多一次交接。
- **比較到哪。** `Compared { first, last }` 讓 Source 放掉 `Known` 裡那些項目的路徑，和流量無關。Destination 每比完一批、要的檔案都寫進 answers 之後就說。B2 的 K>1 時，比完的批次不必連續，所以是一段範圍，不是前綴。
- **`BACKLOG`。** 見不變量 3 的反例。
- **stat 的批次。** 掃描自己決定，例如 256 個名稱一批，和訊息無關；結果照名稱的順序寫進 manifest pipe。
- **不在這份草稿的語意問題。** 決策者等一個目錄的最後一段才要檔案，所以大目錄讓管線停下來；兩邊的記憶體和最大的目錄成正比。這兩個不是 transport 能解決的，見待決定。

### 和 B2 的關係

B2 第 1 步（page 與 `Unlistable`）與第 2 步（以 page 確認）的協定部分，由這裡的記錄（`In`、`Listed`、`Unlistable`、`Compared`）取代。B2 從 2026-10-28 起暫停；這份草稿決定之後，再重排 B2 的步驟。

## 其他做法

- **transport 把大訊息切成好幾個封包（fragmentation）。** 一個目錄照舊是一則訊息，由 transport 切開送。接收端要收齊整則才能解碼，記憶體一次衝高，Destination 也要等整個目錄到齊才開始比較，比現在的段還糟。
- **保留段，只讓 stat 從整個目錄認領。** 只解決 stat 的粒度；訊息大小、Needs、`Known`、流量控制仍綁在一起。
- **每個項目一則訊息，不打包。** 每則都付封包與派送的成本；打包本來就該是 transport 的事。
- **資料留在 peer 壓縮，只有 manifest 由 transport 壓（這份草稿的第一版）。** 留著的理由有二，都站不住：transport 的壓縮若是一條串流，只有一條 thread，資料的吞吐量會被它卡住，但這只說明 transport 的壓縮要能平行；block 各自壓縮是為了單獨重送與排回順序，前者已經拿掉，後者由 pipe 保證。2026-10-28 使用者決定壓縮全部放進 transport。
- **每種內容一條 SSH channel，用 OpenSSH 的 channel 窗口。** 本機到本機沒有 SSH，是 socket pair；channel 的窗口固定 2 MiB，高 RTT 時不夠（architecture 原則 4）；兩台 host 之間的 stream 是兩條 channel 接起來的，窗口不是端到端。
- **gRPC、HTTP/2、QUIC。** 同[架構重新設計的其他做法](architecture-redesign.md#其他做法)，不採用。

## 待決定的問題

- [x] **範圍。** 2026-10-28 使用者同意：兩端都是 ferry 的連線，前段一過都走 pipe，也就是 stream、control connection 與 link 三種（見[哪些連線走 pipe](#哪些連線走-pipe)）。原本的建議「先做 stream」，是把範圍切小的人為分法，沒有理由。
- [ ] **每條 pipe 都壓。** 草稿原本寫 answers、control 量少，不壓。建議沒有例外：量少的省得少，但不必另訂規則；量多時有 5 到 8 倍（問題第 6 點）。
- [ ] **flush 的規則。** 小而頻繁的寫入每次 flush 會變大，「buffer 空了就 flush」在寫入稀疏時正是如此。做法例如：閒下來後再等一小段時間才 flush，或攢到一定大小；代價是延遲，heartbeat 與 `Ping` 不能被延後。量測決定。
- [ ] **control connection 上的 `Done`。** 建議放在 reports pipe 的最後，在 `Profile` 之後。
- [ ] **build 的比對。** 建議改成啟動參數 `server --build <id>`，本機與主機都帶，`Hello` 拿掉 build（見[handshake 與 build](#handshake-與-build)）。
- [x] **data pipe 帶 hash、打不開的檔案與結束。** 壓縮全部搬進 transport 之後，block 本身就拿掉了，這些都是 data pipe 上的記錄，`BlocksEnd` 的計數也跟著拿掉（2026-10-28，併入壓縮的決定）。
- [ ] **needs 與 results 合成一條 answers pipe。** 建議合。
- [ ] **窗口的初始值。** manifest、answers 與 reports 的窗口多大，要不要隨 RTT 調整。量測決定。
- [x] **壓縮放在 transport，還是寫的一方。** 2026-10-28 使用者決定：全部放在 transport，包括資料；manifest 一條串流，data 切塊平行壓縮（見[transport 怎麼送](#transport-怎麼送)）。
- [ ] **data 的塊多大，壓縮與解壓各幾條 thread。** 建議先用 2 MiB 與 8 條，同現在的 block 與 pool；壓縮率與吞吐量和現在比過再定。
- [ ] **pack 整個拿掉。** 建議拿掉：小檔直接是 data pipe 上的一筆記錄，湊在一起是 transport 切塊的事。
- [ ] **一筆記錄的上限。** 建議 1 MiB。
- [ ] **兩個語意問題。** 決策者邊比較邊要檔案，只把建立與 link 留到多出來的項目刪完之後（草稿第 8 節的方向）；增量同步時，沒變的項目比完就丟，不必留到最後一段。建議另外處理，或在重排 B2 時一起排。

## 計畫

1. **transport 加 pipe。** 封包帶 pipe 的編號；每條 pipe 有送出 buffer、窗口與接收 buffer；接收端照 pipe 分開。現有的每種訊息先原樣當作它那條 pipe 上的一筆記錄，peer 的內容不改；`Credit` 封包由 data 的窗口取代。測試：窗口用完時寫入者阻塞、讀走之後放行；接收端從不阻塞；一條 pipe 讀得慢，不擋其他 pipe（就是現在 `parts_in` 的情形）；停止時被擋的寫入者會返回。
2. **control connection 與 link 改走 pipe。** `Hello` 與 `Welcome` 移到前段之後，end mark 拿掉，build 改由 `server --build <id>` 比對；client 與 server 之間是 reports 與 control 兩條 pipe，`Done` 在 reports 的最後；`mux::channel_for_reports`、`tell` 的鎖與 `LISTED_BYTES` 拿掉；link 的訊息進它的 control pipe。測試：dry run 的路徑在等 reports 的窗口時，metrics 與錯誤照常送到；`Done` 不比最後的報告先到；本機 server 的 build 不同時，不寫 start mark 就結束，並說出原因。
3. **manifest 改成記錄。** `In`、`Entry`、`Listed`、`Unlistable`；掃描拿掉段，stat 的批次分開；Destination 邊讀邊解；檢查改成逐筆；`Compared` 取代 `Through`；`IN_FLIGHT_ENTRIES` 拿掉。K=1 時 dry run 的輸出不變。
4. **answers 改成記錄。** 一個 ID 一筆。
5. **壓縮搬進 transport，data 改成記錄。** manifest 的串流壓縮搬進 transport，其他 pipe 照待決定的答案也壓，flush 照它的規則；data 切塊、平行壓縮與解壓、照序號排回；data pipe 帶檔案、分段、hash 與打不開的檔案；block 的格式、序號、`BlocksEnd` 與 pack 拿掉（照待決定的答案）；Destination 的驗證留在 peer。
6. **驗收與文件。** 本機、fatcat 與 nano4 的 `ferry test`。和改之前的版本輪流量：
   - 一個目錄 100 萬個檔案的首次複製與 dry run；
   - `/usr` 形狀的增量同步；
   - 4 GiB 的單檔、20 萬個 4 KiB 的小檔，以及大小混合的樹：吞吐量；
   - 經 SSH 到 fatcat 時，壓縮等級 1 送出的 bytes：例如 crate registry 的原始碼，和現在的 block 比壓縮率；
   - 經 SSH 到 fatcat 時，dry run 與 1 萬個讀不到的檔案，control connection 上送出的 bytes；
   - 兩邊的峰值記憶體。

   改寫 architecture 的 Transport（含 control connection 那一段）、Monitors 與 Packets over the stream，以及 pipeline 的 Scan、Compare、Read and hash、Serialise and compress 與 Decode，中英兩版。
