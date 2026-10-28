# Peer 寫 pipe，transport 決定怎麼送

- 狀態：草稿
- 開始：2026-10-28；最後修改：2026-10-28
- 會改動：`docs/design/architecture.md` 的「Transport」（原則 2、6、8）與「Packets over the stream」；`docs/design/pipeline.md` 的「Principles」（stage 與上限）、「1. Scan」（manifest 的單位、掃描在什麼時候等）、「2. Compare」、「4. Serialise and compress」的 Compression；以上兩份的 `.zh-TW.md`。
- 相關：[架構重新設計](architecture-redesign.md) 的 R4（流量控制散在十幾個常數裡）與 B2（以目錄為單位的 page）；2026-10-28 討論 B2 時，談「大目錄為什麼要切段」談到這裡。

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

**3. 流量控制散在各處，單位各不相同。**

| 上限 | 值 | 單位 | 在哪裡 |
|---|---|---|---|
| 資料額度 | 64 MiB | block 的 bytes，Destination 寫完才還 | `DATA_CREDIT`，`Credit` 封包 |
| 在途的項目 | 262,144 | manifest 的項目，Destination 比完才說 | `IN_FLIGHT_ENTRIES`，`Needs::Through` |
| 送出端的 manifest 佇列 | 8 MiB | bytes | `PACED_BYTES` |
| 送出端的資料佇列 | 256 個 | 64 KiB 的封包 | `DATA_QUEUE` |
| 等著讀的檔案 | 100,000 | 檔案 | `BACKLOG` |

**4. `Through` 一個訊息做兩件事。** 它是流量控制（在途的上限），也讓 Source 放掉 `Known` 裡的路徑。而且只在一個目錄結束時才說，所以一個大目錄裡，在途的上限要到下一個目錄才擋得住。

## 提案

### 原則

像寫檔：程式有什麼就寫什麼，`write` 寫不下就把呼叫它的 thread 卡住，等到寫得下為止；程式不知道一次寫得下多少。讀的一方有什麼就讀什麼，自己解出一筆筆記錄，不必等「一則訊息」到齊，因為沒有訊息邊界，只有記錄自己的長度。

切成多大的封包、什麼時候送、怎麼和其他內容交錯、壓不壓縮、對方還收不收得下，都是 transport 的事，peer 看不到。

### pipe

- **pipe** 是兩個 peer 之間單向、有序、可靠的 byte 串流，一種內容一條。
  - 寫：`write(&[u8])` 全部收下才回來，期間可能阻塞；一次寫多少都可以。
  - 讀：`read(&mut [u8])` 有就回，沒有就等；pipe 結束時回 0。
  - 寫的一方關閉時，transport 送出這條 pipe 的結束。讀的一方讀到結束，才算這條 pipe 完整；stream 先斷了，照舊是傳輸失敗。
- pipe 和 transport 的 **stream** 不同。stream 是實體的連線（SSH channel 或 socket pair），一個傳輸有 1 到 8 條（architecture 原則 4）；pipe 是 peer 看到的邏輯單位，全部多工在這些 stream 上。

### 兩個 server 之間的 pipe

| pipe | 方向 | 記錄 | 寫的一方 |
|---|---|---|---|
| manifest | Source → Destination | root、目錄切換、項目、目錄列完、目錄沒有列、owner 的名稱、讀不到的路徑、結束 | 掃描；會被擋 |
| data | Source → Destination | block、分段檔案的 hash、打不開的檔案；結束就是 pipe 的結束 | block sender；會被擋 |
| answers | Destination → Source | 要的檔案、要的屬性、比較到哪、失敗、needs 結束、done | 決策者的命令；會被擋，但一定會被放行（見[不變量](#不變量不互相卡死)） |
| control | 雙向 | handshake 之後的錯誤 | 不會被擋：沒有窗口，量極少 |

- 分段檔案的 hash、打不開的檔案、blocks 的結束，現在走訊息佇列，靠「訊息在 BlocksEnd 之前」維持順序。放進 data pipe，它們自然排在自己的 block 之後。
- needs 與 results 現在都在訊息佇列，Source 由同一條 thread 讀；`Done` 必須在 results 之後。合成一條 answers pipe，順序自然成立。

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
- **data 的記錄：** `Block(bytes)`、`Hash { file, hash }`、`Unopened { file, path }`。

### transport 怎麼送

- **送出端。** 每條 pipe 一個送出 buffer，上限由 transport 決定，例如 data 的小一些，留在 cache 裡。pump 從各 buffer 取 bytes，組成封包 `[pipe][length][bytes]`；封包多大是 transport 的事，例如最多 64 KiB。pump 忙的時候，bytes 在 buffer 裡累積，空下來時一次拿走一大塊，所以打包自然發生，不必另訂 flush 的規則。
- **排程**沿用現在的規則，它本來就是 transport 的決定：control 最先；answers 其次；data 與 manifest 之間，data 的 buffer 至少四分之一滿時 data 先，否則 manifest 先。
- **data 跨好幾條 stream。** 一個傳輸有好幾條 stream 時，transport 可以把 data pipe 的封包分到各條，封包帶序號，接收端照序號放回順序，peer 看到的仍是一條有序的 pipe。block 自己的編號就不再用來排順序。其他 pipe 都走第一條。
- **壓縮。** pipe 開啟時宣告要不要壓：manifest 壓；data 不壓，block 自己壓過了。要壓的 pipe 在 transport 裡有一條 thread，從送出 buffer 拿到多少就壓多少，buffer 空了就 flush：忙時一大塊，閒時立刻送出。接收端在讀的一方呼叫 `read` 時解壓。manifest 壓縮每個項目不到 1 µs（`pipeline.md` 的 Compression），搬到這裡成本不變。
- **窗口。** 每條 pipe 有一個窗口，單位是 stream 上的 bytes（壓縮後），由讀的一方給：
  - 寫的一方用完窗口就阻塞；
  - 讀的一方的 peer 每讀走一些，transport 就還回去，攢到窗口的四分之一再送；
  - 初始值：data 用現在的資料額度 64 MiB，要不少於頻寬乘以 RTT；manifest 與 answers 待定（見待決定）；control 沒有窗口。
- **接收端。** 一條 thread 讀 stream，照 pipe 分進各自的接收 buffer。對方不會超過窗口，所以 buffer 不會滿，這條 thread 從不必等任何 peer。

### 不變量：不互相卡死

兩個方向都會阻塞的 pipe，有互相卡死的風險，就像兩個 process 各自寫給對方、buffer 都滿了。這份設計靠三條不變量避免：

1. **transport 的接收端從不等 peer。** 它只把 bytes 放進 pipe 的接收 buffer，而那裡一定放得下。
2. **窗口在讀走時還，不在處理完時還。** 處理得慢，讀的一方自然少讀，窗口就不還。資料的端到端回壓照樣成立：Destination 讀 data pipe 的 thread 把 block 交給有上限的解碼 pool，pool 滿了它就停下來不讀，窗口不還，Source 就停。這就是現在的資料額度在做的事，只是不必另一種訊息。
3. **等待不成環。** 會被擋的寫入者，只能等一個「從不等它下游」的讀者：
   - Source 讀 answers 的 thread 從不阻塞：收到的交給 owner，用沒有上限的佇列；量由 manifest 的節奏限制，同現在。
   - 所以 Destination 的決策者寫 answers 時，就算一時被擋，也一定會被放行。
   - 反例，也是 `BACKLOG` 為什麼要留在 peer：如果 Source 等著讀的檔案太多時，owner 就不讀 answers，那麼決策者寫 answers 會卡住，解碼交不出 block，data pipe 沒人讀，Source 的 data 窗口用完，檔案送不出去，等著讀的檔案永遠減不下來。所以 `BACKLOG` 照舊由 owner 讓掃描等。

被擋的寫入者也要能被停止：等窗口和現在等 stream 一樣，都在 scope 裡（architecture 原則 7），scope 結束就返回。

### 從 peer 拿掉的

- `PART`、`PART_BYTES`、`bound()`、`in_bytes()`，以及 `Manifest::Directory` 的 `more`；
- 每段一則的 Needs；
- `Credit` 封包：由 data pipe 的窗口取代，`DATA_CREDIT` 變成那個窗口的初始值；
- `IN_FLIGHT_ENTRIES`、`THROUGH_EVERY`、`through_every()`、`Wait::InFlight`，以及 hello 裡對應的欄位：流量控制由 manifest pipe 的窗口取代；
- mux 的三個佇列、`PACED_BYTES`、`DATA_QUEUE`：變成 transport 裡各 pipe 的送出 buffer；
- `MAX_PAYLOAD` 註解列的各種上限，只剩一筆記錄的上限。

### 留在 peer 的

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
- **每種內容一條 SSH channel，用 OpenSSH 的 channel 窗口。** 本機到本機沒有 SSH，是 socket pair；channel 的窗口固定 2 MiB，高 RTT 時不夠（architecture 原則 4）；兩台 host 之間的 stream 是兩條 channel 接起來的，窗口不是端到端。
- **gRPC、HTTP/2、QUIC。** 同[架構重新設計的其他做法](architecture-redesign.md#其他做法)，不採用。

## 待決定的問題

- [ ] **範圍。** 只做兩個 server 之間的 stream，還是 server 與 client 之間的 control connection（報告、dry run 列的路徑）也照做？建議先做 stream。
- [ ] **data pipe 帶 hash、打不開的檔案與結束。** 這樣 block 的編號只留作檢查，`BlocksEnd` 的計數也可以拿掉。建議這樣做。
- [ ] **needs 與 results 合成一條 answers pipe。** 建議合。
- [ ] **窗口的初始值。** manifest 與 answers 的窗口多大，要不要隨 RTT 調整。量測決定。
- [ ] **壓縮放在 transport，還是寫的一方。** 建議 transport：只有它知道什麼時候閒下來，該 flush。
- [ ] **一筆記錄的上限。** 建議 1 MiB。
- [ ] **兩個語意問題。** 決策者邊比較邊要檔案，只把建立與 link 留到多出來的項目刪完之後（草稿第 8 節的方向）；增量同步時，沒變的項目比完就丟，不必留到最後一段。建議另外處理，或在重排 B2 時一起排。

## 計畫

1. **transport 加 pipe。** 封包帶 pipe 的編號；每條 pipe 有送出 buffer、窗口與接收 buffer；接收端照 pipe 分開。現有的每種訊息先原樣當作它那條 pipe 上的一筆記錄，peer 的內容不改；`Credit` 封包由 data 的窗口取代。測試：窗口用完時寫入者阻塞、讀走之後放行；接收端從不阻塞；一條 pipe 讀得慢，不擋其他 pipe（就是現在 `parts_in` 的情形）；停止時被擋的寫入者會返回。
2. **manifest 改成記錄。** `In`、`Entry`、`Listed`、`Unlistable`；掃描拿掉段，stat 的批次分開；Destination 邊讀邊解；檢查改成逐筆；`Compared` 取代 `Through`；`IN_FLIGHT_ENTRIES` 拿掉。K=1 時 dry run 的輸出不變。
3. **answers 與 data 改成記錄。** 一個 ID 一筆；data pipe 帶 hash、打不開的檔案與結束（照待決定的答案）。
4. **驗收與文件。** 本機、fatcat 與 nano4 的 `ferry test`。和改之前的版本輪流量：
   - 一個目錄 100 萬個檔案的首次複製與 dry run；
   - `/usr` 形狀的增量同步；
   - 兩邊的峰值記憶體。

   改寫 architecture 的 Transport 與 Packets over the stream，以及 pipeline 的 Scan、Compare 與 Compression，中英兩版。
