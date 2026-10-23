# 架構重新設計：端到端可靠、只保留必要的順序、每個 peer 一個擁有者

- 狀態：已採納（2026-10-23：兩個核心問題都決定做）；B1 已做完（2026-10-23）：四步與驗收；計畫中錯誤的分類與合成事件的單元測試併到 A1，模擬的高延遲檔案系統不做，見 [B1](#b-線metadata-的並行)；隨時可以做的三件小事也都做了（2026-10-23）
- 開始：2026-10-22；最後修改：2026-10-23
- 基準：HEAD `20b7586`（0.16.0）。以下的程式位置都以它為準。
- 會改動（實作時；在那之前，這些文件以 Proposed 指向這份草稿）：
  - `docs/design/architecture.md`：「Client and servers」、「The client relays between the servers」、「Transport」（原則 2、4、7）、「Connections」（heartbeat、end marker、server 綁在第一條 SSH 連線）、「Packets on one connection」；
  - `docs/design/pipeline.md`：「Principles」（stage 與上限）、「1. Scan」（一個 lister、固定順序）、「2. Compare」（applier）、「3. Read and hash」到「6. Write」（record 串流、frame、parser）、「8. Finish」；
  - `docs/design/overview.md`：「Terms」（record、frame、packet）、「Scope: one-way only」中的續傳；
  - 以上三份的 `.zh-TW.md` 版本。
- 相關：
  - [目標架構](../review/2026-10-17/target-architecture.md)：異同見[「和 10-17 目標架構的異同」](#和-10-17-目標架構的異同)；
  - [ARC-04](../review/2026-10-17/architecture.md) 與 D16：分段的 record，決定先不做；
  - `TODO.md` 的「Set aside」：Reconnecting；
  - `docs/design/overview.md` 的兩個 open question：典型的工作負載、rsync 在哪裡痛。

這份草稿從高層重新審視 Ferry 的架構：client 在中間、兩邊各一個 server 的拓撲，資料怎麼流動，可靠性放在哪一層。它允許大幅度的重新設計。結論是：現在限制速度與穩定的，已經不是個別的程式碼，而是幾個互相綁住的結構決定。提案換掉其中兩個（全序、可靠性放在 transport），保留第三個（client 在中間、建立所有連線），並說明怎麼一步步走過去。這不是決定；需要使用者決定的事列在[「待決定的問題」](#待決定的問題)。

**目錄**

- [摘要](#摘要)
- [問題](#問題)
  - [現在的架構](#現在的架構)
  - [應該保留的](#應該保留的)
  - [R1：全序造成單執行緒點與 head-of-line blocking](#r1全序造成單執行緒點與-head-of-line-blocking)
  - [R2：可靠性在 transport，斷了就整體失敗](#r2可靠性在-transport斷了就整體失敗)
  - [R3：資料面綁在以 client 為中心的拓撲上](#r3資料面綁在以-client-為中心的拓撲上)
  - [R4：流量控制散在十幾個常數裡](#r4流量控制散在十幾個常數裡)
  - [R5：高延遲檔案系統上，metadata 操作是串行的](#r5高延遲檔案系統上metadata-操作是串行的)
  - [三個決定互相綁住](#三個決定互相綁住)
- [提案](#提案)
  - [1. 原則](#1-原則)
  - [2. 拓撲：client 建立所有連線，不碰資料](#2-拓撲client-建立所有連線不碰資料)
  - [3. 分層與各層的保證](#3-分層與各層的保證)
  - [4. 協定：peer control 與 block](#4-協定peer-control-與-block)
  - [5. session 與 stream](#5-session-與-stream)
  - [6. 流量控制：Destination 發的三種額度](#6-流量控制destination-發的三種額度)
  - [7. Source：一個擁有者，平行的執行者](#7-source一個擁有者平行的執行者)
  - [8. Destination：一個決策者，平行的執行者](#8-destination一個決策者平行的執行者)
  - [9. manifest 以目錄為單位（看工作負載）](#9-manifest-以目錄為單位看工作負載)
  - [10. 存活、重連、續傳](#10-存活重連續傳)
  - [11. 本機到本機：只有選項不同](#11-本機到本機只有選項不同)
  - [12. 保證怎麼保住](#12-保證怎麼保住)
- [其他做法](#其他做法)
- [和 10-17 目標架構的異同](#和-10-17-目標架構的異同)
- [待決定的問題](#待決定的問題)
- [計畫](#計畫)
- [驗證](#驗證)

---

## 摘要

**三個結構決定，前兩個互相綁住，第三個被它們拖累：**

1. **全序。** manifest、record 串流、每一段 transport 的片段、applier，都要求一個全域的順序。真正需要的只有「同一目錄之內」與「同一檔案之內」的順序。全序帶來一串單執行緒點，以及 head-of-line blocking。
2. **可靠性放在 transport，而且斷了就整體失敗。** transport 保證「完整、有序，否則失敗」，資料又不是可以單獨重送的單位，所以任何一條連線斷掉，整個傳輸就失敗；重連與續傳都很難事後加上去。
3. **client 在中間。** 所有資料都經過 client、都走 SSH，client 的存活就是傳輸的存活。client 建立所有連線是刻意的選擇：兩台主機不必互通，主機上不放憑證。提案保留它；它在中間的負擔（重排、讀每則訊息、server 隨它結束）大多來自前兩個決定。

另外有一個獨立的問題：**在高延遲的檔案系統上，metadata 操作有好幾個單執行緒的 stage**（刪除、mkdir、屬性、收尾、列目錄）。

**提案，一段話：** 把可靠性移到兩端，以可重送、可驗證的 block 為單位，端到端確認；負責的 session 是 transport 的最上層，在兩端的 server 裡，peer 仍然只往介面送。只保留真正需要的順序。session 下面的 stream 會斷、只保證各自的順序；所有連線仍由 client 建立，建好就把兩端交給 server，自己不碰資料；三種組合都是 client 加兩個 server，本機到本機只差選項。流量由 Destination 發的額度決定。每個 peer 改成一個不做 I/O 的擁有者，加上依操作種類並行的執行者；整個傳輸只有 Destination 的擁有者做決策。

**得到的：**

- 斷線不再等於失敗：沒被確認的 block 改走別條，stream 在背景重建，server 等 client 重新接上。
- 連線的健康只在一處判斷：server 端是 session，client 端是連線管理。peer 只收「放棄了」與「結束了」兩個事件，沒有網路計時器；heartbeat channel 與它的一整套特例都拿掉。
- 沒有單一的 parser，大檔可以多執行緒讀寫，大檔也不再擋住小檔；整檔的 BLAKE3 照樣在。
- client 在任何組合都不碰資料，`relay.rs` 整個拿掉；本機到本機和遠端走同一條路，只差選項。
- 記憶體由三個額度管住，只在一處決定。
- 每個 peer 的狀態只有一個擁有者：不用鎖；錯誤的分類、排程、「現在在等什麼」都在一處；兩個 peer 可以在一個 process 裡對接，以固定的種子模擬斷線。
- 大量刪除、大量目錄、只改屬性的同步，在高延遲的檔案系統上可以跨目錄平行，也不再擋住資料面。

**代價：** 兩邊的資料路徑與 transport 要重寫，是 step 20 到 22 以來最大的改動。好在 client 與 server 是同一個 build，協定不需要相容舊版，而 rsync parity 測試與 `ferry test` 驗的是行為，可以直接當安全網。保留 client 在中間的代價也還在：host 到 host 的資料經過 client 這台機器的網路兩次，一條 SSH 連線的加解密都在一個 process 裡。

**先做什麼：** 先量（[計畫的第 0 步](#第-0-步量測)）。在這台機器、fatcat 與 nano4 上能量的，2026-10-23 已經量了（[驗證](#驗證)）：B1 的前提成立，整檔雜湊的分段做法成立，client 交出 stream 兩端的機制成立；多開 SSH channel 在這兩條路上沒有好處；manifest 要以一條 stream 壓；在 NFS 與 WekaFS 上，掃描的瓶頸是那一個 lister，列目錄的兩個小地方已經改了（`7e7cb0a`），B2 在目錄小而多的樹上值得做；高 RTT 以模擬量了：一個 channel 每個 RTT 只送 2 MiB，多開 channel 的效果看主機的 TCP buffer，多開連線最有效，但每條都要登入。還沒量的是真實大樹的增量同步，以及真實的長距離鏈路。

---

## 問題

### 現在的架構

![現在：client 在中間，兩邊各一個 server。Source 端 readers 讀小檔，sender 一條 thread 寫 record 串流並自己讀大檔，壓縮後由 frame sender 依序交給 pump；lister 一條 thread 深度優先掃描，manifest 依序給 ID，也交給 pump。pump 是唯一的寫入者，片段全域編號後分到 N 條路徑，client 先重排成一條，再由 relay 轉送，Destination 再重排一次。Destination 端 receive 讀所有 packet，manifest 交給 compare、applier、finisher，資料交給解壓、parser 依序讀，再交給 writers、fsync、renamers。標成琥珀色的是單執行緒點](assets/redesign-now.svg)

圖中只畫論證需要的部分；每一條 thread 與佇列見 [pipeline-workers.svg](../assets/pipeline-workers.svg)。

- **每一種組合都是 client 加兩個 server**，本機到本機也是。2026-10-22 在本機（ext4，96 核心）複製 5 萬個 4 KiB 的檔案，執行中途看到 3 個 process、約 180 條 thread：client 20、Source 40、Destination 121。這次 2.1 秒，瓶頸在 fsync，relay 幾乎一直在等，所以本機的速度不是被中繼拖慢的；中繼的代價在複雜度與失敗模式。
- **每一段都是一條有序串流。** 片段全域編號、分到 N 條路徑，接收端重排（`src/transport/striped.rs`）；client 讀每個 packet 的種類，報告留下，其他轉送（`src/relay.rs:179-260`）。server 內，peer 與 transport 之間還隔著兩條 pipe 與 inward、outward 兩條 thread（`src/peer.rs:144-210`）。
- **資料是一條 record 串流。** frame 每 2 MiB 切一刀，不管檔案邊界（`src/frame.rs:20`），所以 Destination 只能由一個 parser 依序讀（`src/destination/write.rs`）。大於 1 MiB 的檔案由 sender 這一條 thread 自己讀（`src/source.rs:700`、`:813-821`），在 Destination 由一個 writer 寫。
- **記憶體與在途的量，由十幾個常數一起決定**；見 [R4](#r4流量控制散在十幾個常數裡)。

### 應該保留的

下面這些是現在做對的事，提案不動它們：

- 用系統的 `ssh` 認證與啟動：`~/.ssh/config`、ProxyJump、OTP、Kerberos 都照常可用。
- client 建立所有連線：server 不往外連、不開 port、不放憑證，兩台主機不必互通；peer 只把 transport 當成一個介面。見[第 2 節](#2-拓撲client-建立所有連線不碰資料)。
- client 與 server 是同一個 build，自動上傳安裝；協定可以自由改。
- prepare 狀態機：一切在傳輸開始前問完、準備好；根目錄的身分與重疊檢查。
- 由 Destination 決定要什麼；邊收 manifest 邊在 Destination 掃描比較（online diff），兩邊都不必把整棵樹放進記憶體。
- 以 `openat` 加 `O_NOFOLLOW` 操作的檔案系統 API、manifest 檢查、fuzz。
- 每個檔案原子就位、BLAKE3 驗證、`O_TMPFILE`、批次 fsync 與 macOS 的 barrier。
- `Stop`／`Stages`：第一個錯誤，全體停止；panic 政策；profiling。
- Rust 加同步 thread：工作幾乎都是阻塞的檔案系統呼叫。

### R1：全序造成單執行緒點與 head-of-line blocking

| 哪裡要求全序 | 位置 | 由誰執行 |
|---|---|---|
| manifest：深度優先、依名稱 | `src/scan.rs:190`、`:235`；`docs/design/pipeline.md:196-201` | lister，1 條 |
| ID 依 manifest 順序；Source 靠 `Needs::Through(id)` 釋放 `Known` | `src/source.rs:46-138` | — |
| 比較與套用依 manifest 順序 | `src/destination/receive.rs`、`src/destination/apply.rs:323` | compare 1 條、applier 1 條 |
| record 串流：一個接一個，frame 跨檔案 | `src/source.rs`、`src/frame.rs` | sender 1 條、parser 1 條 |
| 每一段 transport：片段全域編號、重排 | `src/transport/striped.rs`；`docs/design/architecture.md:384-389` | 每個 peer 一個 pump；client 每方向一條 relay |

真正需要的順序，只有下面幾條：

- **同一目錄之內：** 先刪多餘的，再建立、link；目錄的內容都完成，才收尾。
- **目錄之間：** 父目錄建好子目錄，子目錄才能開始；子目錄都收尾，父目錄才收尾。
- **同一檔案之內：** 它的位元組屬於它，雜湊涵蓋整個檔案。

沒有一條需要跨目錄、或跨檔案的順序。

![左：現在，目錄依 manifest 的深度優先順序一個接一個，檔案依 record 串流的順序一個接一個，任何一個慢，後面的全部等。右：真正需要的只有同一目錄內的順序（先刪多餘，再建立與 link，最後收尾）、父目錄建好子目錄後子目錄才能開始、子目錄收尾後父目錄才能收尾；a/ 與 b/ 之間沒有任何箭頭，可以同時做](assets/redesign-order.svg)

本機 ext4 上複製小檔時，瓶頸是 fsync，不是這些單執行緒點；大檔是例外（下面的 a）。在高延遲的檔案系統或快速的鏈路上，它們會輪流變成瓶頸，而且會互相阻塞：

![a：一個 4 GiB 的大檔由 sender 一條 thread 讀，後面已經讀好的檔案都在排隊，Destination 只有一個 writer 在寫它，其他 15 個閒著。b：applier 一條 thread 刪除 old/ 的 100 萬個檔案，在 WekaFS 上約 11 分鐘（量到每次 0.65 ms），這段時間沒有新的 needs，資料面在送完已經要的檔案後就閒著。c：三條路徑中第 2 條慢，第 5 片還在它上面，接收端的重排緩衝收滿 256 片或 64 MiB 後，第 1、3 條路徑也停](assets/redesign-hol.svg)

- **a：一個大檔擋住後面所有檔案。** sender 讀大檔的時候，其他 reader 讀好的小檔只能排隊；Destination 上，16 個 writer 只有一個在寫。ARC-04 量過：4 GiB 的單檔，Destination 唯一的 writer 忙了 97% 的時間。
- **b：一次大的刪除擋住後面所有目錄。** `delete_extras` 在 applier 這一條 thread 裡逐一 `remove_tree`（`src/destination/apply.rs:1367-1386`）。刪除期間，後面的目錄都不套用，也就沒有新的 needs。
- **c：一條慢路徑擋住其他路徑。** 重排緩衝最多 256 片或 64 MiB；下一片在慢路徑上時，緩衝一滿，快的路徑也得等。

### R2：可靠性在 transport，斷了就整體失敗

設計文件寫明了這個取捨：

- transport 的原則 7：「It fails rather than deliver anything wrong」（`docs/design/architecture.md:151`）。
- 「Any SSH connection that drops fails the transfer」（`:227`）。
- 「Paths that acknowledged their pieces, so that a sender could resend over the other paths what a dropped one lost, were considered and not taken, to keep the transport simple」（`:233-236`）。
- server 綁在第一條 SSH 連線上，與它的控制連線同生共死（`:219-226`）。
- `TODO.md` 把 Reconnecting 擱置。

![上：現在，片段全域編號後分到三條路徑；第 2 條斷了，第 5、8 片跟著消失，接收端等第 5 片，它永遠不會來，整條連線失敗，傳輸失敗；重跑時，傳到一半的大檔從頭來。下：提案，block 各自獨立，Destination 收到就確認，Source 保留還沒被確認的 block；stream 2 斷了，B5、B8 沒被確認，就改從 stream 1、3 重送，順序不拘，傳輸照常完成，stream 2 在背景重建](assets/redesign-drop.svg)

後果：

- **失敗機率隨傳輸時間累積。** 重跑會跳過完成的檔案，但要重新掃描與比較，傳到一半的大檔從頭來。
- **重連很難事後加上去。** 資料不是可以單獨重送的單位：要嘛在每一段做位元組層級的重送緩衝，要嘛改資料格式。
- **偵測機制越來越精細，而且散在各層。** heartbeat channel、end marker、start mark、server 比 client 多等 10 秒，這一整套都是「斷了就失敗」的代價：失敗是終局，偵測就得又快又準。能重送的系統只需要「這條沒進展，就換一條」。現在判斷斷線的地方，各有各的計時器：
  - read 讀到 EOF、write 失敗：path、控制連線、client 的轉送都會遇到。
  - path 的結束標記：沒有標記就結束，算被截斷。
  - heartbeat channel：每條 SSH 連線多一個 `ssh -W`，每 5 秒一次；client 30 秒、server 40 秒沒聽到就失敗。它和資料共用 TCP，所以 path 上來的資料也要算「聽到」。
  - ssh 的 `ServerAliveInterval=15` 乘 `ServerAliveCountMax=4`：約 60 秒。只有 Ferry 自己的 master 有設；使用者自己的 ControlMaster 照使用者的設定，可能沒有。prepare 階段還沒有 heartbeat，只靠它。
  - 登入的 `ConnectTimeout=10`，與等 master 的 30 秒。
  - server 跟著控制連線結束。
  - 收尾時給 server 10 秒放開 path，失敗後給 5 秒。

### R3：資料面綁在以 client 為中心的拓撲上

![本機到本機：三個 process，之間兩段 Unix socket，每個 server 內還有兩條 pipe。推送：Source 的 server 在本機，經 Unix socket 到 client，再經 N 條 SSH 連線到 host；每個 SSH channel 每個 RTT 最多送一個 window，多一條 TCP 就要多登入一次。host 到 host：資料從 host A 經 SSH 到 client，再經 SSH 到 host B，兩次經過 client 的網路；client 睡著或斷網，傳輸就停，A 與 B 之間更快的網路用不到](assets/redesign-routes-now.svg)

- **host 到 host：** 資料兩次經過 client 的網路；client（常是筆電）睡著或斷網，傳輸就停。兩台 host 之間也許有更快的網路，用不到。
- **所有資料都走 SSH，** 於是 SSH 的上限就是資料面的上限：
  - 一個 channel 每個 RTT 最多送一個 window。設計文件寫 OpenSSH 是 2 MiB，100 ms 時約 20 MB/s（`docs/design/architecture.md:280-292`）；「每條連線開 K 個 channel」還是 Proposed。2026-10-23 的模擬證實了這個上限：250 ms 時，一個 channel 8.3 MB/s，預設只開一條連線的 Ferry 7.4 MB/s（5.2 節）。
  - 一條 SSH 連線的加解密，都在它的 master 這一個 process 裡。
  - 要多一條 TCP，就要多登入一次；主機用 OTP 時，每一條都要等下一組碼（`docs/design/architecture.md` 的「Connections」）。
- **本機到本機也要 3 個 process**，也沒辦法用 `copy_file_range`、reflink 這類 kernel 內的複製。

**哪些是刻意的。** client 建立所有連線，換來兩台主機不必互通、主機上不放憑證、server 不開 port（`docs/design/architecture.md` 的「The client relays between the servers」）。提案保留這個選擇，所以 host 到 host 的資料經過 client 這台機器的網路、一條 SSH 連線的加解密在一個 process 裡，這兩項代價也留著；本機到本機的 3 個 process 也留著，換來每一種組合同一條路（第 11 節）。可以拿掉的是不屬於它的部分：client 的 process 經手每個位元組（建立連線不必經手資料，第 2 節），client 重排、讀每則訊息（來自 R1、R2），server 隨 client 結束（來自 R2），SSH 的 window 與多次登入（每條連線 K 個 channel）。client 睡著或斷網，server 也能在寬限時間內等它回來（第 10 節）。

### R4：流量控制散在十幾個常數裡

| 常數 | 值 | 位置 | 管什麼 |
|---|---|---|---|
| `PACED_BYTES` | 8 MiB | `src/mux.rs:48` | 等著寫出的 manifest |
| `DATA_QUEUE` | 256 × 64 KiB | `src/mux.rs:41` | 等著寫出的資料 |
| `PATH_QUEUE` | 8 片 | `src/transport/striped.rs:33` | 每條路徑的佇列 |
| `EARLY`、`EARLY_BYTES` | 256 片、64 MiB | `src/transport/striped.rs:36`、`:42` | 重排緩衝 |
| `DECODED` | 8 個 frame | `src/destination/mod.rs:141` | 解壓後等 parser 的 |
| `BACKLOG` | 100,000 | `src/source.rs:440` | 需要、還沒送的檔案 |
| `IN_FLIGHT_ENTRIES` | 262,144 | `src/protocol.rs:213` | 已送、還沒比較的 entry |
| `IN_FLIGHT` | 64 | `src/source.rs:703` | reader 手上的檔案 |
| `PARTS`、`WAITING` | 4、64 | `src/destination/receive.rs:291`、`src/scan.rs:49` | 等比較、等 stat 的 part |

現在的上限是有效的：100 萬個空檔案時峰值 345,857，以前是 825,925（`docs/design/pipeline.md:212-226`）。但這個峰值是推導出來的，不是規定的。在高 BDP 的鏈路上，16 MiB 的資料佇列、64 MiB 的重排視窗、SSH 的 window，哪一個先卡住，只能逐一試；新增一個 stage，就多一個要調的常數。

### R5：高延遲檔案系統上，metadata 操作是串行的

檔案內容是平行的：16 個 writer、64 個 fsync thread、4 個 renamer。但下面這些都只有一條 thread：

- **applier：** mkdir、symlink、特殊檔、只改屬性的項目、`sync` 的刪除（遞迴的 `remove_tree`）（`src/destination/apply.rs`）。
- **finisher：** 目錄的 owner、mode、mtime、fsync（`src/destination/finish.rs:23`）。
- **列目錄：** Source 的 lister 與 Destination 的 compare，各一條（`docs/design/pipeline.md:168-187`）。2026-10-23 量到，一條 lister 在 fatcat 的 NFS 與 nano4 的 WekaFS 上，列一個目錄約 0.5 ms，每個項目再加 10–25 µs，每秒只處理 1.5 萬到 7 萬個項目；本機是幾十萬到兩百多萬（[驗證](#驗證)）。

串行時，吞吐量就是「1 ÷ 一次往返的延遲」。2026-10-23 在 nano4 的 login node 上量到，WekaFS 上一次 unlink 約 0.65 ms、mkdir 約 0.69 ms、改屬性約 0.53 ms；fatcat 的 NFS 上是 0.13、0.14、0.23 ms（[驗證](#驗證)）。所以在 WekaFS 上，一條 thread 刪 100 萬個多餘的檔案約 11 分鐘，其間 applier 不送任何 needs（[R1 的 b](#r1全序造成單執行緒點與-head-of-line-blocking)）；首次複製 10 萬個目錄，光 mkdir 就要一分多鐘；`chmod -R` 之後的同步，每個項目都要改屬性，全都排在一條 thread 上。列目錄也一樣：1,000 萬個項目、10 萬個目錄的樹，光是 Source 列目錄，WekaFS 上就要 2.5 到 3 分鐘，NFS 上沒有快取時要 2.5 到 5 分鐘。

### 三個決定互相綁住

![左：全序，使每一段都要重排成一條，一條慢路徑擋住全部，frame 跨檔案只能一個 parser，擋住多路徑互不阻塞。中：可靠性放在 transport，保證有序、不遺失，否則失敗；資料沒有確認、沒有重送，缺一片就只能整體失敗，server 也隨 client 結束，擋住重連與續傳。右：client 在中間，是刻意的選擇：連線一律由 client 建立，兩台主機不必互通，主機上不放憑證。它的負擔來自前兩個決定：每一段都要有序、可靠，client 才得重排、讀每則訊息。前兩個換掉之後，client 只建連線，不碰資料；資料經過 client 這台機器，是保留它的代價](assets/redesign-interlock.svg)

全序與「可靠性在 transport」互相綁住，只換一個，另一個還是擋住：

- **只在 transport 加確認與重送：** 片段還是要全域重排，HOL 還在；client 還是得重排、讀訊息；大檔還是一條 thread。
- **只做分段的大檔（ARC-04）：** 解決了大檔的平行，但斷線還是失敗。

所以重連與續傳一直排不進去。提案把這兩個一起換掉。

第三個，client 在中間，是刻意的選擇，提案保留它。它的負擔大多是被前兩個拖累的：每一段都要有序、可靠，client 才得重排、讀每則訊息；斷了就是終局，server 才隨 client 結束。前兩個換掉之後，client 建好 stream 就把兩端交給 server，連位元組都不必經手，server 也能在寬限時間內等它回來。剩下的代價，是 host 到 host 的資料仍經過 client 這台機器（[R3](#r3資料面綁在以-client-為中心的拓撲上)）。

---

## 提案

### 1. 原則

新增或改變的原則：

1. **可靠性端到端。** Source 與 Destination 之間，以可重送、可驗證、冪等的單位溝通，並互相確認。transport 與中繼不保證任何東西，只盡力送。
2. **只保留真正的相依。** 同一目錄之內、同一檔案之內，以及父子目錄之間；其餘都可以平行。
3. **連線一律由 client 建立；peer 只往介面送。** server 不往外連、不開 port、不放憑證，兩台主機不必互通。client 建好每條 stream，就把兩端交給 server，自己不碰資料；每一種組合都是 client 加兩個 server。
4. **記憶體與在途的量由消費者決定。** Destination 發額度，一處決定。
5. **每一種檔案系統操作的並行度，都是政策，不是 thread 的個數。**
6. **一份狀態，一個擁有者。** 每個 peer 的狀態只由一條不做 I/O 的 thread 改，I/O 都交給執行者。整個傳輸只有 Destination 做政策的決定，Source 只決定怎麼做（[3.1 節](#31-peer-的形狀一個擁有者平行的執行者)）。
7. **連線的健康只在一處判斷。**
   - server 端只由 session 判斷 stream 健不健康；client 端只由連線管理負責建立與重建連線。
   - peer 只收兩個事件：`Lost`（session 放棄了）與 `Closed`（正常結束）。事件放進擁有者的收件匣，不是 callback。
   - 其他元件都不持有網路計時器（[5.3 節](#53-連線的健康只在-session-與-client-的連線管理)）。

不變的原則：可靠性先於速度；斜線不改變行為；結果與 rsync 相同；一切需要使用者的事在傳輸開始前問完（重連時的登入是[待決定的例外](#待決定的問題)）。

### 2. 拓撲：client 建立所有連線，不碰資料

![提案：三種組合都是 client 加兩個 server。client 建立每條 stream，把兩端交給 server，自己不碰資料；server control 另走 client 與各 server 之間。本機到本機：client 開 N 對 socketpair，兩端各交給一個 server。推送：client 啟動 ssh（N 條連線、每條 K 個 channel），ssh 的 stdin、stdout 接 socketpair 的一端，另一端交給本機的 server。host 到 host：client 在自己這台機器上啟動兩邊的 ssh，用 socketpair 背對背接起來；資料經過這台機器的網路兩次，但不經過 client 的 process](assets/redesign-routes.svg)

**保留的決定：連線一律由 client 建立。** client 有使用者的 ssh 設定、agent、輸入 OTP 的終端機。server 從不往外連，只在私有的 Unix socket 上等 path 加入（`src/transport/join.rs:1-17`）：不開 port、不放憑證，兩台主機也不必互通。對 peer 來說，transport 只是一個介面：現在 Source 與 Destination 的入口只拿一個 `Read`、一個 `Write`（`src/source.rs:204-208`、`src/destination/mod.rs:146-150`），只管往裡面送。提案保留這兩點，改的是介面後面的分工（第 3 節）。

**client 建立 stream，但不握著另一端。** 現在 client 連上 server 的 socket，或啟動 ssh，自己握著另一端轉送（`src/transport/mod.rs:336-361`、`src/relay.rs`）。提案中，client 建好每條 stream，就把兩端都交出去。用的是 `join` 已經在用的機制：以 `SCM_RIGHTS` 把 fd 經 server 的私有 socket 交給它（`src/transport/join.rs:280-326`）。

| 組合 | 現在 | 提案：client 怎麼建 stream |
|---|---|---|
| 本機 → 本機 | client 連上兩個本機 server 的 socket，轉送 | 開 N 對 socketpair，兩端各交給一個 server |
| 推送、拉取 | client 握著本機 server 的 socket 與 ssh 的 stdin、stdout，轉送 | ssh 的 stdin、stdout 接 socketpair 的一端，另一端交給本機的 server；每條 SSH 連線 K 個 channel |
| host → host | 兩段 SSH；client 重排、讀訊息後轉送 | 兩個 ssh 的 stdin、stdout 用 socketpair 背對背接起來；資料經過 client 這台機器的網路兩次，但不經過 client 的 process |

- **每一種組合都是 client 加兩個 server。** peer 與 session 的程式碼只有一份，連 process 的形狀都一樣；本機到本機只差選項（第 11 節）。
- **client 不碰資料。** `relay.rs` 整個不需要；「client 從不是 peer」（`docs/design/architecture.md` 的「Client and servers」）也保住。
- **client 看不到 stream 裡的東西，** 所以 stream 的狀態（加入、斷了、RTT）由 server 經 server control 回報；client 只看 ssh 的 process 有沒有結束，負責重建（5.4 節）。

兩種連線：

- **server control**：client 與每個 server 之間的控制連線，量小。用於 prepare、握手、plan、報告（進度、警告、dry run 的清單）、stream 的狀態、metrics、snapshot。報告從 peer 的串流移到這裡，client 才能完全不碰資料。
- **peer 的 stream**：Source 與 Destination 之間的一組 stream，承載 session 的 frame（control 與 block）；見[第 5 節](#5-session-與-stream)。

### 3. 分層與各層的保證

| 層 | 內容 | 保證 |
|---|---|---|
| client | prepare 狀態機、plan、連線管理（建立與重建所有連線）、UI、政策 | — |
| peers | 各一個擁有者，加上執行者；Destination 的擁有者也是決策者（3.1 節） | 業務邏輯與檔案系統上的不變式；每個 peer 的狀態只有一個擁有者 |
| session | transport 的最上層，在兩端的 server 裡：control 的序號、確認、重送；block 的編號、保留、確認、去重；選 stream；stream 的重新加入；每條 stream 的健康，包括 server control 的 | **端到端可靠**：每則 control 恰好交付一次、依序；每個 block 恰好交付一次，順序不拘；放棄時以 `Lost` 通知 peer |
| streams | socketpair（本機）、SSH channel；host 到 host 時兩個 ssh 背對背接起來；都由 client 建立，兩端交給 server | 每條只保證自己內部的順序；斷了就回報，不保證送到 |
| runtime | `Stop`、`Stages`（已有）、pool、metrics | 第一個錯誤，全體停止 |
| fsx | 以 `openat` 加 `O_NOFOLLOW` 的 API（已有）、執行者的後端 | 不跟隨 symlink、不出根目錄 |

和現在最大的不同，在 session 與 streams 這兩層。現在「有序、不遺失」由每一段各自保證；提案把它交給 session。session 仍是 transport 的一部分，但在兩端的 server 裡，涵蓋整條路；中間的 stream 退成盡力而為，client 也不在資料路徑上。peer 仍然只往介面送，不管怎麼連、怎麼傳：

| | 現在 | 提案 |
|---|---|---|
| peer 看到的 | `Read`、`Write`；訊息完整、依序 | session 的介面：control 恰好一次、依序；block 恰好一次、順序不拘 |
| 可靠性在哪 | 每一段各自保證（server ↔ client、client ↔ server），斷了就失敗 | session，在兩端的 server 裡，涵蓋整條路 |
| client 在中間做什麼 | 重排、讀每則訊息的種類 | 不碰資料：建好 stream 就把兩端交給 server |
| 誰建立連線 | client | client，不變 |

對 peer 來說，介面只變了四點：

1. 收到的 block 沒有順序。
2. 不再靠 `send` 阻塞來限流，因為擁有者不能阻塞（3.1 節）。peer 在 Destination 給的額度內交出 block，session 照單全收；session 保留的量因此也被額度限住。
3. transport 的原則 7「寧可失敗，也不交付錯的」照樣成立；只是失敗的門檻從「一條路斷了」，變成「所有 stream 都斷了，而且寬限時間已過」。
4. 連線的事 peer 一概不管：沒有讀寫錯誤，也沒有逾時。只有 session 放棄時的 `Lost`，與正常結束的 `Closed`（5.3 節）。

session 有自己的狀態與自己的擁有者，也寫成不做 I/O 的狀態機；stream 的收送是它的執行者。peer 與 session 之間只有介面：沒有任何事件需要同時改兩邊的狀態。`Ack` 只動保留的 block，屬於 session；`Credit` 只動額度，屬於 peer；stream 斷了，動到的保留的 block 與 control 的序號，也都屬於 session。

#### 3.1 peer 的形狀：一個擁有者，平行的執行者

兩個 peer 用同一個形狀，session 也是：

- **擁有者：** 一條 thread。這個 peer 的狀態都在它手上，也只有它能改。它不做 I/O，只收事件、發命令。
- **執行者：** 依操作種類分開的 pool，做所有的 I/O 與重的計算：列目錄、stat、讀、寫、雜湊、壓縮、名稱操作、fsync；session 的執行者是 stream 的收送。做完以事件回報，不自己決定接下來做什麼。

好處來自三個條件同時成立：只有一份狀態、只有一條 thread 改它、這條 thread 不做 I/O。少一個就打折：狀態一分散，鎖就回來；擁有者一做 I/O，自己就成了瓶頸。現在的 applier 自己刪除（[R1 的 b](#r1全序造成單執行緒點與-head-of-line-blocking)），Source 的 file sender 自己串流大檔（`src/source.rs:813-821`），都是後者。

**誰決定什麼：**

| 誰 | 決定什麼 | 例子 |
|---|---|---|
| client，在 prepare 時 | 靜態的政策 | 幾條 SSH 連線與 channel、K、`-x`、`copy` 或 `sync`、要不要壓縮 |
| Destination 的擁有者，也就是決策者 | 動態的政策：傳什麼、什麼順序、傳多少 | Want、刪除、W、額度、每個項目的結果 |
| Source 的擁有者 | 只決定怎麼做 | pack 何時封口、大檔的段與小檔怎麼交錯讀 |
| session，兩端各一個 | 只決定怎麼送 | 走哪條 stream、什麼時候重送 |

整個傳輸只有一個決策者。兩邊都做政策的決定，就得互相同意，順序又回來了（R1）。Source 讀不到、讀不完整的檔案照實回報，後果由 Destination 決定。

**集中到擁有者的，和仍在別處的：**

| 方面 | 集中到擁有者 | 仍在別處，或要注意 |
|---|---|---|
| 邏輯 | 狀態怎麼變、下一步做什麼、不變式的檢查 | 操作本身（怎麼寫檔、刪一棵樹、讀檔）在執行者 |
| 錯誤 | 分類：單一項目失敗（結束狀態 23）、警告，還是停止傳輸 | 停止的機制仍是 `Stop` 與 `Stages`；卡在系統呼叫裡的執行者打斷不了，只能不再派工作給它 |
| 排程 | 優先順序、每種操作的並行度、同一個目錄一次一個、額度 | 每個工作多一趟往返，所以要成批 |
| profile | 每個工作在等什麼：額度、執行者，還是閘門；每個佇列多長 | 每條 thread 忙不忙、系統呼叫花多久，仍由各 thread 記（`profile::wait`）；擁有者本身也要量 |
| 競爭 | 狀態不用鎖；同一個目錄的名稱操作不同時派出，不在 kernel 的目錄鎖上互搶（ARC-01b） | 換成擁有者一條 thread 的吞吐量上限 |
| 測試 | 餵合成的事件序列就能測；兩個 peer 連同各自的 session 都不做 I/O，可以在一個 process 裡對接，以固定的種子模擬斷線與重連 | 執行者的 I/O 仍要整合測試 |

**規則：**

![左：會死結的寫法。擁有者把工作送進執行者的佇列，佇列滿了就等；執行者要把結果送回擁有者，擁有者的 inbox 也滿了，也在等；兩邊互等，永遠卡住。右：規則。擁有者自己數在途的工作，上限 4 個，確定有空位才派；inbox 不設上限，但裡面最多就是在途的那 4 個結果。所以擁有者從不等，執行者送結果也從不等。現在的 send_files 就是這樣做的](assets/redesign-owner.svg)

1. **擁有者絕不阻塞。** 擁有者把工作送進執行者的佇列，佇列滿了就等；執行者要把結果送回來，擁有者的 inbox 也滿了，也等：兩邊互等，永遠卡住。所以擁有者自己數在途的工作，確定有空位才派；inbox 不設上限，裡面最多就是在途的那幾個結果。現在的 `send_files` 已經這樣做：自己數 `in_flight`，回報用的 `queue` 不設上限（`src/source.rs:286`、`:763`）。從另一個 peer 來的訊息也各有上限：Want 受 W 限制，page 受 manifest 額度限制，block 受資料額度限制。
2. **位元組不經過擁有者。** 擁有者只拿 handle（`Arc` 的緩衝區、block 的編號）；組 pack、壓縮、切 fragment 都是執行者的事。
3. **事件成批。** Want 本來就是一批檔案；執行者的結果成批回報；`Credit` 一次還一批。
4. **擁有者內部再分模組。** 例如 Source 的 `Known`、分段的進度、額度；Destination 的目錄表、檔案表、閘門。每個模組各有測試，擁有者的迴圈只負責分派。每個目錄、每個檔案的狀態寫成明確的 enum：事件驅動的程式比由上往下寫的難讀，這是補償。

**代價是吞吐量，所以一定要成批。** 2026-10-23 的合成量測（執行者不做 I/O，只有擁有者是限制）：結果一個一個回報時，每秒約 27 萬個小檔，瓶頸在 thread 之間的喚醒；每 16 個成批時每秒約 430 萬個，每 64 個約 570 萬個。所以一條 thread 夠用，前提是成批；這不是最佳化，是規則。A1 與 B1 各有一項驗收，量真正的擁有者。

session 不在 peer 的擁有者裡：它是 transport 的最上層，有自己的擁有者（第 3 節）。peer 的擁有者只往它的介面送 control 與 block。

### 4. 協定：peer control 與 block

#### 4.1 peer control

可靠、有序、量小。

| 方向 | 訊息 | 內容 | 說明 |
|---|---|---|---|
| S → D | `Root` | 根目錄的 entry | 第一則 |
| S → D | `Names` | 新出現的 uid、gid 的名稱 | 在第一個用到它們的 page 之前送 |
| S → D | `Page` | `{ dir, part, entries, last }` | 一個目錄的一段：entry 依名稱排序、ID 連續；zstd 壓縮（4.4 節） |
| S → D | `Unlistable` | `{ dir, reason }` | 宣告過、但列不出的目錄：它的終點（第 9 節） |
| S → D | `Problem` | `{ path, file }` | 讀不到的項目，同現在 |
| S → D | `FileHash` | `{ file, hash }` | 分段的檔案的整檔雜湊（4.3 節） |
| S → D | `ManifestEnd` | — | 掃描完成 |
| D → S | `PageCompared` | `{ dir, part }` | 這一段比較完、它的 Want 已送出：Source 丟掉這段的 `Known`，同時歸還它占的 manifest 額度 |
| D → S | `Want` | `{ files }` | 由 builder 依筆數與位元組自動分段，沒有任何一則會超過上限 |
| D → S | `Credit` | `{ data }` | 歸還資料額度 |
| D → S | `Results` | 同現在 | 失敗的項目、刪不掉的項目 |
| 雙向 | `Error` | `{ message }` | 每一邊最多一次 |
| D → S | `Done` | — | 一切都已持久；絕不在 `Error` 之後 |

**可靠性由 session 提供，peer 看不到：**

- 每個方向各自為控制訊息編號（`seq`）；接收端在它送出的每個 frame 上順便帶累計確認（`ack`）。
- 傳送端把還沒被確認的訊息放在重送緩衝區，以位元組為上限（例如 4 MiB）；滿了，session 就請擁有者先別送，擁有者把 control 留在自己這邊，不等（3.1 節的規則 1）。
- 平常固定走一條 stream，不必重排；那條斷了，就換一條還活著的 stream，從最後確認之後全部重送；接收端丟掉已交付過的序號。

#### 4.2 block

![上：現在，檔案一個接一個寫成 record（header、內容、trailer），frame 每 2 MiB 切一刀，不管檔案邊界；要找到下一個 record，得先讀完前一個，所以只能一個 parser 從頭依序讀。下：提案，pack 裝多個完整的小檔，各帶 hash；segment 是大檔的一段，帶 offset；每個 block 自己描述自己，任何 worker 拿到就能解壓、驗證、寫入，大檔的各段由不同的 writer 以 pwrite 寫到各自的位置；block 可以單獨重送，重複的丟掉](assets/redesign-blocks.svg)

```text
block    = header ‖ payload（壓縮或原樣）
header   = { codec: Raw | Zstd,
             raw_len: u32,
             body: Pack { files: [PackFile] } | Segment(Segment) }
PackFile = { file: FileId, len: u32, mtime: Timestamp,
             check: Hash([u8; 32]) | Unread(String) }
Segment  = { file: FileId, offset: u64, len: u32,
             size: u64, mtime: Timestamp,          // 開檔時那一次 fstat
             check: Cv([u8; 32]) | Unread(String) }
```

- **pack** 把多個完整的檔案依讀完的順序串在一起，滿 B（例如 8 MiB）或閒置一下（例如 2 ms）就封口，就像現在「串流閒置時，frame 不等滿就送」。可以跨目錄。不大於段大小 S 的檔案都整個進 pack；一個 pack 可以只有一個檔案。
- **segment** 是大於 S 的檔案的一段，S 是 2 的冪（例如 8 MiB），從 S 的倍數開始；最後一段可以比較短。所以每個分段的檔案至少有兩段。
- **`len`、`size`、`mtime` 是開檔時的值**，和現在 record header 的語意相同（`src/destination/write.rs:435`）：讀到開檔時的大小，Destination 以這個 mtime 設定檔案。
- **讀不完整，不必補零。** 現在的 record 補零是為了讓串流對齊；block 自己描述自己，讀不到的檔案在 pack 裡寫成 `Unread`、`len` 為 0。一段讀不完整，整個檔案失敗。
- **編號由 session 給，冪等。** block 本身不帶編號；Source 的 session 收下時給它一個遞增的編號，帶在每個 fragment 上（5.1 節）。Destination 的 session 依編號去重，peer 只看到每個 block 一次。
- **壓縮**以 block 為單位，level 1；壓不下去就原樣送；本機到本機時不壓，同現在（ARC-05）。

#### 4.3 整檔雜湊不必放棄

![一個 27 MiB 的檔案切成四段：0–8、8–16、16–24、24–27 MiB。每段由一條 thread 以 set_input_offset 設好位置，算出非 root 的 chaining value：CV0 到 CV3。CV0 與 CV1 合併成 P01，CV2 與 CV3 合併成 P23，都用 merge_subtrees_non_root；最後 merge_subtrees_root 合併 P01 與 P23，得到的正是 blake3::hash(整個檔案)](assets/redesign-hash.svg)

BLAKE3 是樹狀的雜湊。對齊、大小為 2 的冪的段，可以各自算出子樹的 chaining value（CV），再依 BLAKE3 樹的形狀兩兩合併，得到和一次讀完完全相同的 root hash。`blake3` 1.8.7 的 `hazmat` 模組正好提供這些：`HasherExt::set_input_offset`、`HasherExt::finalize_non_root`、`merge_subtrees_non_root`、`merge_subtrees_root`，以及決定樹形狀的 `left_subtree_len`。2026-10-23 寫程式驗證過：5 種段大小、156 種長度，包括最後一段只有 17 位元組的情形，各段在不同的 thread 算 CV，合併出的 root 都等於 `blake3::hash`；改一個位元組，root 就不同。

- Source：每段由讀它的 reader 算 CV，放進 segment；擁有者收齊各段的 CV 後合併出 root，以 `FileHash` 送出。合併很便宜：n 段只要合併 n−1 次，每次只處理 64 個位元組，擁有者自己做就好。
- Destination：每段到了，先對收到的位元組以同一個 offset 重算 CV，與 segment 帶的比較，不符就讓整個檔案失敗，不寫入；全部到齊，再把 CV 合併出 root，與 `FileHash` 比較，相符才繼續設屬性、fsync、link。
- 所以「整個檔案的雜湊相符才給它名字」這條保證不變，而且雜湊值和現在一模一樣，`--checksum` 日後也能用同一個值比較。ARC-04 與 10-17 的目標架構在這裡選擇了只驗每段，不必退這一步。

#### 4.4 manifest page 壓縮

做之前，只有 record 串流有 zstd，manifest 是沒有壓縮的 postcard；2026-10-23 起，資料壓縮時 manifest 也以一條 stream 壓縮（見[隨時可以做的小事](#隨時可以做的小事)）。增量同步時，manifest 就是全部的流量：一個 entry 約 40 到 55 位元組（2026-10-23 以 Ferry 自己的掃描與編碼量），1,000 萬個就是約 500 MB；在 100 Mbps 的鏈路上要 40 秒以上。壓縮的效果依樹的形狀而定，而且要以一條 stream 壓，不要每個 page 各自壓：

| 樹 | 每個 page 各自壓（zstd 1） | 一條 stream，每個 page 後 flush（zstd 1） |
|---|---|---|
| `/usr`（5.3 萬個項目） | ×2.0 | ×3.2 |
| crate 的原始碼（4.2 萬） | ×1.5 | ×3.6 |
| 建置產物（6.7 萬） | ×1.5 | ×2.6 |
| 一個 10 萬個檔案的扁平目錄，名稱規律 | ×17.8 | ×19.2 |

小目錄多時，每個 page 都很小，各自壓幾乎沒有上下文；一條 stream 讓前面的 page 當後面的字典。control 訊息恰好交付一次、依序（第 3 節），所以 Source 可以把所有 page 壓在同一條 zstd stream 裡，Destination 依序解，重送的是同一份壓好的位元組。扁平目錄那一列是最好的情形：檔案都是空的、mtime 幾乎相同；真實資料的大小與 mtime 各不相同，會低一些。這一項不依賴其他改動，現在的架構就能做：現在的 manifest 也是依序、不遺失。

### 5. session 與 stream

#### 5.1 stream 的語意

這些 frame 都屬於 session；peer 看不到。

```text
stream 上的 frame = [kind: u8][length: u32][payload]
  Hello     { session: [u8; 16], stream: u32 }      一條 stream 的第一個 frame
  Control   { seq: u64, ack: u64, message }           peer control
  Ack       { ack: u64, blocks: [u64] }               確認 control 到 ack 為止，以及收到的 block
  Fragment  { block: u64, offset: u32, last: bool, bytes ≤ 64 KiB }
  Keepalive { }
  End       { }                                       這條 stream 正常關閉：送出的都已被確認，收到的一方不當成斷線
```

- **一個 block 的所有 fragment 走同一條 stream、依序**；Destination 的 session 依編號重組，只需要記住「正在收的那幾個 block」。
- **block 收到就確認，** 不等 peer 驗證：雜湊不符時，檔案本來就直接失敗，不會要求重送。Source 的 session 收到確認，就丟掉保留的那份。
- **控制 frame 可以插在兩個 fragment 之間，而且優先**，所以控制訊息的延遲不受大 block 影響。
- **block 交給佇列最短的 stream。** 沒有片段的全域編號，也沒有重排緩衝。
- **host 到 host 時背對背。** 兩邊的第 i 條 stream 是兩個 ssh process，它們的 stdin、stdout 由 client 用一對 socketpair 接在一起；位元組由 kernel 在兩個 ssh 之間搬，client 的 process 不碰。其中一個 ssh 結束，另一個就讀到 EOF、跟著結束，於是兩邊的 session 都看到「第 i 條斷了」。現在 `relay.rs` 讀 packet、挑報告的邏輯，以及 client 上的重排，都不再需要。

#### 5.2 stream 的兩種底層

stream 的底層由拓撲決定，不必選；不論哪一種，都由 client 建立、把兩端交給 server（第 2 節）：

1. **socketpair。** 兩側都在本機時。不經過網路，也不經過 client。
2. **SSH。** 其他時候：client 建立的 N 條 SSH 連線，每條再開 K 個 channel，解掉 window 的上限，K 條 stream 也只要登入一次；host 到 host 時，兩邊的 ssh 背對背接起來。

沒有解決的是：一條 SSH 連線的加解密都在一個 process 裡；要用到更多核心，就要更多條連線，每條各登入一次。這是連線一律由 client 建立的代價之一；不採用直連的理由見[其他做法](#其他做法)。2026-10-23 的量測（以 `ssh -W`，和 Ferry 一樣）：

- **fatcat**（LAN，RTT 1.7 ms）：一條連線約 60 MB/s，開 4、8、12 個 channel 只到 65–67 MB/s；4 條連線約 104 MB/s，接近 1 GbE。本機的 master 只用了 13–17% 的 CPU。
- **nano4**（RTT 6.5 ms）：一條連線、一個 channel 就到約 116 MB/s，已是這台機器 1 GbE 的上限，多開 channel 沒有差別。nano4 每一條新的連線都要 OTP，所以多條連線要使用者輸入多次。
- 所以在這兩條路上，多開 channel 沒有好處。channel 的 window（2 MiB）要到 RTT 約 20 ms 以上，才會卡住 1 GbE。

**高 RTT，以模擬量（2026-10-23）。** 沒有真實的長距離鏈路，所以在這台機器上模擬，不需要 root：
- **鏈路：** 兩個私有的 network namespace 以 TUN 相接，中間的程式加上單程延遲、1 Gbps 的速率與 50 ms 的佇列。
- **兩端：** 真的 kernel TCP（cubic）與 OpenSSH 9.6。
- **TCP buffer：** 先用預設值（收最多 6 MiB、送最多 4 MiB，和這台、fatcat、nano4 相同）；再調大到 64 MiB，代表為長距離調過的主機。

單位 MB/s：

| RTT | TCP buffer | 只有 TCP | 1 條連線，1 個 channel | 1 條，4 個 | 1 條，8 個 | 4 條連線 |
|---|---|---|---|---|---|---|
| 2 ms | 預設 | 121 | 119 | 119 | 119 | 121 |
| 30 ms | 預設 | 92 | 66 | 89 | 90 | 120 |
| 120 ms | 預設 | 24 | 17 | 23 | 24 | 69 |
| 250 ms | 預設 | 12 | 8.3 | 11 | 11.5 | 33 |
| 30 ms | 64 MiB | 121 | 66 | 118 | 118 | 121 |
| 120 ms | 64 MiB | 119 | 17 | 68 | 81 | 69 |
| 250 ms | 64 MiB | 63 | 8.3 | 34 | 57 | 33 |

- **一個 channel，不論 buffer，每個 RTT 正好 2 MiB。** 這是 OpenSSH 的 window。
- **預設 buffer 時，一條 TCP 每個 RTT 約 3 MiB。** 所以多開 channel 最多只快 ×1.35；要更快只能多開連線，2 條 ×2、4 條 ×4，每條各登入一次。
- **buffer 調大時，K 個 channel 幾乎等於 K 條連線。** 例如 250 ms 時，4 個 channel 34 MB/s、4 條連線 33 MB/s，而且不用多登入。
- **有掉包時，只有多條連線有用。** 0.01% 的隨機掉包下，120 ms 時一條 TCP 從 24 掉到 4 MB/s，4 條連線還有 14–16 MB/s；多開 channel 沒有幫助。

**現在的 Ferry 照著同樣的上限。** 以 256 MiB 的檔案量，含啟動時間：
- **預設 1 條連線：** 30、120、250 ms 時分別是 58、15、7.4 MB/s。
- **`--ssh-connections 4`：** 在 120 與 250 ms 時快 ×3.0；但經過 ControlMaster 的 4 條只快 ×1.15，因為底下仍是一條 TCP。
- **小檔：** 2 萬個小檔的速率也只受 window 限制，沒有逐檔的往返；新設計的 Want 與額度也必須維持這一點。
- **啟動：** 要約 12 個 RTT，250 ms 時是 3.1 秒。是哪些往返還沒查。

#### 5.3 連線的健康：只在 session 與 client 的連線管理

現在的偵測散在各層（R2）。提案把它收到兩處（第 1 節的第 7 條）：server 端只有 session 判斷 stream 的健康；client 端只有連線管理負責建立與重建。

**session 看的訊號，每條 stream 各自：**
- EOF、讀寫錯誤：立刻判定這條斷了。
- 沉默：閒置時每 5 秒送一個 `Keepalive`，收到任何位元組都算聽到；多久沒聽到算斷，見下面的計時器。
- 判定斷了，就主動關掉這條 stream，讓另一端也讀到 EOF，兩端的判斷因此一致。只有一個方向通的時候也一樣。

**沉默不會被 backpressure 誤判。** session 永遠把 stream 讀乾，送得過來的量由資料額度限住（第 6 節），所以讀的一方從不必停下來；慢鏈路上，資料一直在進來，本身就算聽到。現在的 heartbeat 要靠「資料也算聽到」補洞，是因為它和資料分開走、卻共用一條 TCP。

**誰處理、peer 看到什麼：**

| 情況 | 誰處理 | peer 看到什麼 |
|---|---|---|
| 一條 stream 斷了 | session 把沒被確認的換條 stream 重送，經 server control 告訴 client；client 重建 | 什麼都沒有 |
| 全部 stream 都斷了 | session 開始計寬限時間，等 client 重建（第 10 節） | 什麼都沒有；額度用完時，擁有者自然停下 |
| 寬限時間到了 | session 宣告放棄，之後拒絕以這個 session ID 加入的 stream | `Lost` |
| 傳輸完成 | peer 要 session 關閉；session 等自己送出的都被確認，在每條 stream 送 `End` | `Closed` |

**peer 怎麼接：** `Lost` 與 `Closed` 是擁有者收件匣裡的事件，和其他事件一起處理。用 callback 的話，它會在 session 的 thread 上碰 peer 的狀態，違反「一份狀態，一個擁有者」。`Lost` 和其他致命錯誤走同一條停止路徑（`Stop`），peer 只做收尾。peer 沒有任何網路計時器。

**server control 也由 session 管。** client 與 server 之間的控制連線，用同樣的 keepalive，斷了也能重新加入。所以：
- prepare 階段一樣有偵測，不再靠 ssh 的 ServerAlive，也不受使用者自己 ControlMaster 的設定影響。
- server 的壽命跟著 session：所有 stream，包括 server control，都斷了，才開始計寬限（第 10 節）。

**client 的連線管理**是 client 裡的一個擁有者：
- 掌管 ssh master 與每條 stream 的建立。
- 收兩種訊號：ssh process 的結束，以及 server 回報的 stream 狀態。
- 負責登入逾時（`ConnectTimeout` 等），以及重建；重建時要不要問登入，見[待決定的問題](#待決定的問題)。
- client 的其他部分（prepare、UI、報告）只收它的事件：stream 斷了、接回來了（給 dashboard），以及某個 server 已經不在了。
- 它確定一邊已經不在時，例如那邊的 server process 結束了，可以直接叫另一邊放棄，不必等寬限。

**計時器只剩兩組。**
- **session：** keepalive 的間隔、stream 的沉默門檻、寬限時間。
- **client 的連線管理：** 建連線時的逾時。

誤判的代價只是重建一條 stream、重送一點資料，所以沉默門檻可以比現在的 30、40 秒短，例如 20 秒，斷線反而更早發現。數字之後量測決定。

**拿掉的：**
- heartbeat channel，以及每條 SSH 連線因此多開的 `ssh -W`；
- 「資料也算聽到」的補丁；
- path 的結束標記與它的特例：序號與確認取代了它；
- 收尾時的 10 秒（`LET_GO`）與失敗後的 5 秒；
- prepare 對 ServerAlive 的依賴。

Ferry 自己的 master 仍然設 `ServerAliveInterval`，只是讓死掉的 master 早點結束；偵測不再靠它。

**測試：** session 是不做 I/O 的狀態機，時間由外面給，所以下面這些都能以合成的事件序列測（A2）：沉默、只有一個方向通、時斷時續、寬限到期、寬限到期後才回來的 stream。

#### 5.4 斷線時

1. session 把那條 stream 上沒被確認的 control 訊息，換一條 stream 重送；沒被確認的 block，交給其他 stream 重送。Source 的 session 保留壓縮後的 block，直到對方確認收到，所以重送的是同一份位元組，不重讀檔案，語意和現在一樣；保留的量不超過資料額度（第 6 節）。
2. client 的連線管理從 server 的回報或 ssh 的結束，知道哪條斷了，在背景重建它：SSH 的 master 還在，就開一個新的 channel；不在，就以 `BatchMode=yes` 重新登入（以 OTP 登入的主機，例如 nano4，這一步會失敗，見[待決定的問題](#待決定的問題)）。新的 stream 一樣把兩端交給 server，以 `Hello` 帶著 session ID 加入同一個 server 的 session，就像現在 path 加入一樣；server 不必知道怎麼連。
3. 只要還有一條 stream，傳輸就繼續，只是慢一點；全部斷光時，在寬限時間內等（第 10 節），逾時才失敗。peer 看不到這一切，只在 session 放棄時收到 `Lost`（5.3 節）。

### 6. 流量控制：Destination 發的三種額度

![manifest：Source 的擁有者送 manifest page，用掉 manifest 額度；Destination 比較完、送出 Want 後還回去；額度用完，擁有者就先不收掃描的 page，掃描跟著停。Want：Destination 自己限制已 want、還沒完成的檔案數 W，到了上限就先不處理下一個比較結果。資料：Source 的擁有者把 block 交給 session，用掉資料額度；Destination 寫完就以 Credit 還回去。兩邊的記憶體因此各有一個明確的上限](assets/redesign-credits.svg)

| 額度 | 由誰決定 | 初始值 | 用掉 | 歸還 | 用完時 |
|---|---|---|---|---|---|
| manifest 額度 | Destination | 8 MiB，同現在的 `PACED_BYTES` | Source 送出一個 page | `PageCompared` | 擁有者先不收掃描的 page，掃描跟著停 |
| want 視窗 W | Destination 自己 | 10 萬個檔案，同現在的 `BACKLOG` | 送出 Want | 檔案定案（committed 或 failed） | 先不處理下一個比較結果，manifest 額度也就不再歸還 |
| 資料額度 | Destination | 64 MiB；上限例如 512 MiB | Source 送出一個 block（壓縮後的大小） | `Credit`：block 寫完、記憶體放掉 | 擁有者先不把 block 交給 session |

每一種記憶體都有一個額度管住：

| 位置 | 內容 | 上限 |
|---|---|---|
| Source | `Known`（已送、還沒比較的 entry 的路徑） | manifest 額度 |
| Source | 待讀的檔案 | W |
| Source 的 session | 保留到對方收到的 block | 資料額度 |
| Source | 讀取與壓縮中的緩衝區 | reader 數 × B，加上壓縮 thread 數 × B |
| host 到 host 時，client 這台機器 | 每對 socketpair 的 kernel 緩衝 | 固定，socket 緩衝的大小 |
| Destination | 待比較的 page | manifest 額度 |
| Destination | 一個目錄的清單 | 最大的一個目錄（同現在） |
| Destination | 已 want、還沒定案的檔案 | W |
| Destination | 收到、還沒寫完的 block | 資料額度 |

這些取代現在的 `PACED_BYTES`、`DATA_QUEUE`、`BACKLOG`、`IN_FLIGHT_ENTRIES`、`EARLY`、`DECODED`。

**資料額度至少要涵蓋頻寬 × RTT**，否則新設計自己就會被 RTT 卡住；現在的 Ferry 沒有跨線的資料額度，所以沒有這個問題。
- 1 Gbps、250 ms 時，在途的資料約 31 MB。模擬中最快的組合（250 ms、buffer 調大、8 個 channel）是 57 MB/s，在途約 14 MB（5.2 節）。
- 64 MiB 的初始值，在 1 Gbps 時涵蓋到約 500 ms 的 RTT；10 Gbps 時只涵蓋到約 50 ms。
- 所以先用固定值，加一個隱藏的旗標供量測；要不要依 RTT 自動調整，等有 10 Gbps 的長距離需求再說。

### 7. Source：一個擁有者，平行的執行者

![提案中的 Source。中間是擁有者，一條 thread，不做 I/O；Known、待讀的檔案、分段的進度、正在裝的 pack、兩種額度都在它手上。執行者回報的事件：目錄列完與 stat 完、讀完一個檔案或一段、block 組好了。session 交來的事件：Destination 的 Want 與 PageCompared、Credit、Done 與 Error，以及寬限後仍接不回來。它發出的命令：交給 lister 與 stat 列目錄，交給 reader 讀小檔或大檔的一段，交給壓縮組成 block，把 control 與 block 交給 session，經 server control 送報告。位元組不經過擁有者，只傳 handle；怎麼送、斷了怎麼重送，是 session 的事。下方：現在的 Source，Known 在 Mutex 裡，由掃描與讀回覆兩條 thread 共用；Backlog 以 Mutex 加 Condvar 由三條 thread 共用；file sender 擁有待讀的佇列，卻也自己串流大檔](assets/redesign-source.svg)

形狀見 [3.1 節](#31-peer-的形狀一個擁有者平行的執行者)。

**現在已經做了一半。** `send_files`（`src/source.rs:714-852`）是一個迴圈，自己擁有待讀的佇列與在途的數量，從同一個佇列收 `Send`、`End`、`Ready` 三種事件：它就是擁有者。缺的是另外兩個條件：

- 它也做 I/O：每個大檔都由它自己串流（`:813-821`）。
- 狀態沒有全在它手上：`Known` 放在 `Mutex` 裡（`:223`），由掃描與讀回覆的兩條 thread 共用；`Backlog` 是 `Mutex` 加 `Condvar`（`:453-557`），三條 thread 都會碰。

提案留下擁有者，把 I/O 移出去，把狀態收進來。

**擁有者的狀態，與會改到它的事件：**

| 狀態 | 會改到它的事件 |
|---|---|
| `Known`（以 page 為單位） | page 組好時加入；`Want` 查路徑；`PageCompared` 時丟掉 |
| manifest 額度 | 送出 page 時用掉；`PageCompared` 時歸還 |
| 待讀的檔案，依 Want 的順序 | `Want` 加入；有 reader 空出來就派出 |
| 待列的目錄（K>1 時） | 一個目錄列完；某個 lister 空出來；列不出來（`Unlistable`） |
| 每個分段檔案的進度 | 開檔（大小、段數）；一段讀完（CV）；一段讀不完整（整個檔案失敗）；全部讀完（合併出 root，送 `FileHash`） |
| 正在裝的 pack | 一個小檔讀完；滿 B，或閒置計時到了，就封口 |
| 資料額度 | block 交給 session 時用掉；`Credit` 時歸還 |

有幾個事件要同時動兩份以上的狀態：`Want` 要查 `Known`，再進待讀的佇列；`PageCompared` 要丟掉 `Known` 的一段，也歸還 manifest 額度；一段讀不完整，要動分段的進度，也不能再送 `FileHash`。分給幾條 stage 各管一部分，這些就變成鎖，或 stage 之間來回的訊息與它們的先後（見[其他做法](#其他做法)）。stream 斷了則不是擁有者的事：session 自己重送，擁有者看不到。

**執行者：**

| pool | 做什麼 | 並行度 |
|---|---|---|
| lister、stat | 列目錄、stat；`openat` 加 `O_NOFOLLOW`，`-x`、avoid 照舊 | lister K（第 9 節；K=1 就是現在），stat 8 |
| reader | 開檔，`fstat` 一次，大小與 mtime 以這次為準；小檔整個讀並算 BLAKE3；大檔以 `pread` 讀一段並算 CV | 16，同現在 |
| 壓縮 | 依擁有者給的清單組成 block（pack 或 segment）、壓縮；本機到本機時不壓 | 8，同現在 |

stream 的收送是 session 的執行者（第 3 節）。

**一個小檔、一個大檔走過的路：**

- **小檔：** `Want` 到了，擁有者查 `Known`，把路徑交給 reader；reader 讀完、算好 BLAKE3，回報緩衝區的 handle；擁有者把它記進正在裝的 pack，滿 B 或閒置到了就封口，交給壓縮；壓縮完，有資料額度就交給 session；之後怎麼送、斷了怎麼重送，是 session 的事。
- **大檔：** reader 開檔、`fstat` 後發現大於 S，回報大小與 mtime，檔案以 `Arc<File>` 共用；擁有者切成 ⌈大小 ÷ S⌉ 段，任何 reader 都可以用 `pread` 讀其中一段、算它的 CV；每段讀完就去壓縮、送出，不進 pack；各段的 CV 都到了，擁有者合併出 root，送 `FileHash`（4.3 節）。

**決定什麼、不決定什麼：** 擁有者只決定怎麼做：pack 何時封口、大檔的段與小檔怎麼交錯讀；block 走哪條 stream，由 session 決定。傳哪些檔案、什麼順序、傳多少，來自 Destination 的 Want 與額度；K、`-x`、avoid 由 client 在 prepare 時決定。讀不到的檔案以 `Problem` 回報，讀不完整的在 block 裡標成 `Unread`，後果都由 Destination 決定。

**完成與停止：**

- 完成是狀態上的一個條件：`ManifestEnd` 已交給 session、每個 Want 的檔案都交出或回報了；之後等 Destination 的 `Done`，它表示一切都已持久，自然也都收到了。不再靠幾條 thread 依序關掉 channel。
- 停止照舊：第一個錯誤，全體停止（`Stop`）。擁有者收到 Destination 的 `Error`，或認定自己的錯誤要停止傳輸時，由它觸發；session 在寬限時間後仍接不回來，也以失敗交給擁有者。

**掃描可以晚一步。** K=1 時，掃描可以先維持現在的樣子：照順序給 ID、組成 page（同時以筆數與位元組為上限，同現在的 `PART`、`PART_BYTES`），交給擁有者；擁有者沒有 manifest 額度時就先不收，掃描自然停下。B2 要 K>1 時，給 ID 與待列的目錄才移進擁有者，lister 與 stat 也才變成純粹的執行者。

### 8. Destination：一個決策者，平行的執行者

![提案中的 Destination：中間是決策者，一條 thread，不做任何 I/O，只保存目錄與檔案的狀態並做決定。它收到的事件：manifest page 到了、比較完成、block 驗證好了、操作完成、FileHash 與 Unlistable。它發出的命令：交給比較的 pool 列目錄與 stat，交給執行者做名稱操作、寫內容與屬性、fsync、目錄收尾，並經 session 送出 Want、Credit、Results。下方：現在 applier 一條 thread 刪 100 萬個檔案，期間沒有任何 Want；提案中不同目錄的刪除由 K 條平行做，Want 照常送出](assets/redesign-destination.svg)

**保留 ARC-03 的單一擁有者，拿掉單一執行者。** 形狀同 [3.1 節](#31-peer-的形狀一個擁有者平行的執行者)；不同的是，這個擁有者也是整個傳輸唯一的決策者。

- **決策者**（coordinator）是一條 thread，所有目錄與檔案的狀態都只在它手上，不用鎖。它不做任何 I/O：收事件、發命令。
- **執行者**是依操作種類分開的 pool：

| pool | 做什麼 | 並行度 |
|---|---|---|
| 比較 | 列 Destination 的目錄、merge、stat | 8，同現在 |
| 解碼 | 解壓、驗證 pack 內每個檔案的雜湊或 segment 的 CV | 8 |
| 名稱操作 | 刪除（`remove_tree` 依子目錄拆開）、mkdir、symlink、特殊檔、link、rename | K；同一個目錄一次一個 |
| 內容 | 寫入沒有名稱的暫存檔（大檔以 `pwrite` 寫各段）、owner、mode、mtime | 16 |
| fsync | 只是在等磁碟 | 64，同現在 |
| 收尾 | 目錄的 owner、mode、mtime、fsync | K |

- **名稱操作的平行來自不同的目錄。** 同一個目錄裡的名稱操作，kernel 以那個目錄的鎖一個一個做。2026-10-23 量到（[驗證](#驗證)）：16 條 thread 各自在不同的目錄，WekaFS 上各種名稱操作快 ×14 到 ×16，NFS 上 ×3 到 ×10；16 條 thread 擠在同一個目錄，WekaFS 上只快 ×1.2 到 ×1.3，NFS 上沒有變快。所以一個目錄裡就有 100 萬個多餘的檔案時，刪除還是那麼慢；改變的是它不再擋住別的目錄與資料面（下面的閘門）。
- **錯誤的分類收進決策者。** 現在一個檔案寫入失敗時，write stage 自己判斷要不要停止傳輸（`ends_the_transfer`），自己送 `Results::Failed`、加共用的計數（`src/destination/write.rs:396-411`）；applier 只收到 `Settled`。提案中，執行者只回報結果，由決策者決定這是單一項目失敗、警告，還是停止。
- **決策者寫成不做 I/O 的狀態機**（sans-IO）。目錄的生命週期、刪除與建立的順序、失敗的分類，都能用合成的事件序列與模擬的檔案系統測試，比整合測試更能涵蓋到達順序與競態。Source 的擁有者也這樣寫，兩者就能在一個 process 裡對接，以固定的種子模擬（A2）。
- 日後 Linux 可以把某些 pool 換成 io_uring 的後端，決策者不用改。

**一個目錄與它的檔案：**

![上排是目錄的狀態：已宣告、已比較（送出這個目錄的 Want）、刪多餘（平行）、閘門「多餘已刪」、建立與修屬性（平行）、收尾、完成並通知父目錄；收尾的條件是子目錄都完成、檔案都定案、最後一段 page 已到。下排是一個檔案：block 到了，寫入沒有名稱的暫存檔、驗證、設屬性、fsync，這些都可以在刪除完成前做；只有 link 要等閘門，link 之後定案，計入目錄的收尾](assets/redesign-directory.svg)

- **比較完就送 Want，不等這個目錄的刪除。** 檔案寫進沒有名稱的暫存檔，不碰目錄的名稱空間；只有最後的 link 要等「多餘已刪」這道閘門。大量的刪除因此只擋這個目錄的 link，不擋資料面，也不擋別的目錄。
- **「先刪多餘、再建立」照樣成立：** 閘門之前，這個目錄裡不建立、不 link 任何名稱。所以不分大小寫的檔案系統上，與讀不到的名稱互為別名的項目，處理方式同現在。
- **目錄收尾由下往上數：** 自己的 page 都到了、子目錄都完成了、檔案都定案了，就收尾，再通知父目錄。由深到淺自然成立，不需要現在的 stack。
- **macOS 的 barrier：** 執行者把 link 依磁碟分批，每批先設一道 barrier，同現在的 renamer。

### 9. manifest 以目錄為單位（看工作負載）

這一節只有在「大型樹的增量同步」是主要工作負載時才值得做：那時掃描本身就是傳輸的時間。

![例子：根/ 下有 a/、b/、c/；a/ 下有 a1/、a2/；b/ 下有 b1/；c/ 下有 c1/，共 8 個目錄，每列一個要一趟往返 L。左：現在一個 lister 深度優先，8 個目錄一個接一個，約 8L；Destination 也只有一條 compare 照同樣的順序列自己的目錄。右：K=3 個 lister，根/ 列完就同時列 a/、b/、c/，再同時列 a1/、a2/、b1/，最後 c1/，約 4L；page 可以任意順序到。Destination 由下往上判斷完成：自己的 page 都到、子目錄都完成、檔案都定案；列不出的目錄由 Source 送 Unlistable](assets/redesign-pages.svg)

- **現在只有一個 lister，是刻意為 NFS 的負載選的**（`docs/design/pipeline.md:168-187`）。之前平行掃描試過又放棄，主因是要維持全域順序：得重排，得為記憶體加越來越多特例。
- **2026-10-23 量到的（[驗證](#驗證)）：** 三種合成的樹，各 5 萬個空檔；每次都是 cold：目錄改過，快取的屬性也過期了。
  - **一個 lister 就是瓶頸。** 目錄小的時候，lister 佔掃描時間的 60–98%，8 條 stat thread 大多閒著。例外是 NFS 上的超大目錄：每個 stat 都變成一次 GETATTR，瓶頸在 stat。
  - **K 個 lister。** 用一支獨立的小程式量：K 個 lister 共用一個目錄佇列，配 8 條 stat。它不建 page、不送 manifest，所以量到的是上限。比起 K=1：

| | 5,000 個目錄 × 10 | 500 × 100 | 10 × 5,000 |
|---|---|---|---|
| WekaFS，K=4 | ×4.1 | ×1.5 | ×1.9 |
| WekaFS，K=16 | ×7.2 | ×1.5 | ×2.5 |
| NFS，K=4 | ×2.3 | ×3.6 | ×1.0 |
| NFS，K=16 | ×2.3 | ×2.7 | ×1.0 |

- **WekaFS 上，一般大小的目錄卡在 stat。** 只列不 stat 時，K=16 幾乎是線性的（×11 與 ×6.9），配上 stat 只剩 ×1.5，可見卡在 8 條 stat。給 32 條也只到 ×1.8，因為每次 stat 從 51 µs 變成 170 µs。
- **NFS 上，伺服器的負擔約加倍。** K=4 之後，這台 client 的請求率從每秒 1.3–1.8 萬升到 2.0–3.4 萬（含別人的流量），和上次的結果一樣。K=16 沒有更快，500 × 100 時 READDIR 的延遲反而從 0.47 ms 升到 2.8 ms。
- **結論。** B2 在目錄小而多的樹上值得做；一般大小的目錄，要連 stat 的吞吐量一起解決。列目錄的兩個小地方已經改了（[隨時可以做的小事](#隨時可以做的小事)，`7e7cb0a`）：一個 lister 列一個目錄快了 1.1 到 4.6 倍（實測，看檔案系統與目錄的大小），送給伺服器的請求也變少。
- **提案：page 以目錄為單位，可以任意順序到。**
  - 每個目錄以自己的 entry ID 當 `dir`；page 說的是「目錄 `dir` 的第 `part` 段」。
  - Source 的擁有者把待列的目錄分給 K 個 lister，自己給 ID、組 page；同一個目錄只由一個 lister 列，所以同一個目錄的各段依序到，不同目錄的 page 可以交錯。
  - Destination 每個 page 獨立比較，由比較的 pool 平行做。
- **manifest 檢查改成：**
  - 根目錄最先，只一次；
  - 目錄 d 的 page，只能在 d 被父目錄的某個 page 宣告之後到（Source 的擁有者給了 d 的 ID 才派人列 d，page 也依組成的順序送，所以這條在 Source 自然成立）；
  - 同一個目錄的各段依 `part` 遞增，名稱嚴格遞增；ID 在 page 內連續，跨 page 遞增；
  - 每個宣告過的目錄恰好有一個終點：最後一段（`last`），或 `Unlistable`；
  - 其他一律是協定違規，停止傳輸。fuzz 要涵蓋任意順序的 page。
- **完成**改成由下往上數（第 8 節）。現在的 `Open` 已經有 `children` 與 `unseen`，只有 `left` 依賴深度優先的順序；`Unlistable` 讓「列不出」變成明確的訊息，不再靠「manifest 離開了它」推斷。
- **代價：** dry run 的列出順序在 K>1 時不再固定（K=1 時與現在相同）；改動大。
- **K 是對共用檔案系統的禮貌問題，是政策，不是架構。** 同時發出 K 個列目錄的請求，伺服器的負擔跟著加重：NFS 上 K=4 就拿到大部分的好處，請求率約加倍；WekaFS 上目錄小而多時，K=16 還在變快。預設值由使用者決定；可以依 `statfs` 判斷檔案系統的種類。

### 10. 存活、重連、續傳

![網路斷了一下：現在，heartbeat 30 秒沒有聲音，連線失敗，傳輸以 1 結束；重跑時要重新掃描與比較，完成的檔案跳過，傳到一半的大檔從頭來。提案：stream 失效，server 進入寬限時間；client 重連，SSH master 還在就開新 channel，不在就以 BatchMode 重新登入；新的 stream 以 session ID 加入同一個 server，就像現在 path 加入一樣；雙方的 session 重送還沒被確認的 control 與 block，傳輸接著做。寬限時間內接不回來，才失敗並清理，和現在一樣](assets/redesign-reconnect.svg)

**server 的存活：**

- server 不再跟第一條 SSH 連線同生共死，壽命改成跟著 session：所有 stream，包括 server control，都斷了，才開始計寬限時間（例如 5 分鐘）。最初那個 SSH session 的標準輸入結束，只是其中一條斷了。
- 寬限時間內，client 可以以 token 與 session ID 重新接上控制連線與 stream。
- 逾時，session 宣告放棄，peer 收到 `Lost`、收尾，server 結束。清理和現在 `interrupt::stop` 的相同。
- 限制：主機設了 systemd-logind 的 `KillUserProcesses=yes` 時，session 一結束 server 就被殺掉；那時重連失敗，退回現在的行為：重跑。

**同一次執行內的重連：** 就是 5.4 節的重送與重建。

**跨次執行的續傳（大檔，之後再做）：**

- 大於某個門檻（例如 64 MiB）的檔案，改用有名稱的暫存檔，例如 `.name.ferry-partial`；旁邊記一份狀態：Source 的大小與 mtime、段大小 S、已驗證的段與它們的 CV。
- 重跑時，Destination 比較到這個名稱，若 Source 的大小與 mtime 相同，就只 Want 缺少的段（`Want` 帶段的 bitmap）；不同，就刪掉舊的暫存檔，整個重傳。
- 這和 `O_TMPFILE` 的「被 `kill -9` 也不留東西」互斥，所以只對大檔使用；小檔照舊。這就是 D16 延後的那一項；在 block 格式下，它只是加上去的功能，不必再改格式。

### 11. 本機到本機：只有選項不同

- **和其他組合同一個架構。** 一樣是 client 加兩個 server，一樣的 peer 與 session；client 開 N 對 socketpair，把兩端交給兩個 server（第 2 節）。不同的只有選項：不壓縮（同現在，ARC-05）、stream 開幾條。本機的測試走的，就是遠端用的同一條路。
- **代價：** 仍是 3 個 process，只是 client 閒著；資料經過 kernel 的 socket 緩衝。2026-10-23 在這台機器上量到：經過一對 socketpair，每 GB 用 0.25 CPU 秒，一條約 8 GB/s；在同一個 process 裡複製，每 GB 0.03 CPU 秒；現在的路（兩段 socket，中間 client 轉送）每 GB 0.45 CPU 秒。所以提案比現在省；比 in-process 多花的 CPU，在這台 ext4 RAID 約 0.5 GB/s 的寫入速度下，約十分之一個核心。不把兩個 peer 放進同一個 process 的理由，見[其他做法](#其他做法)。
- **同一個檔案系統上的大檔，可以讓 kernel 複製：** Linux 的 `copy_file_range`（NFS 4.2 會在伺服器端直接複製，資料不經過這台機器），btrfs、xfs 的 reflink（`FICLONE`），macOS APFS 的 `clonefile`。kernel 複製要兩個 fd 在同一個 process：Source 可以經本機的 socketpair，同樣以 `SCM_RIGHTS` 把開好的檔案 fd 傳給 Destination，所以只有本機到本機能用。它是 block 之外的一個選項，不是另一套架構。但這會改變「端到端驗證」的意思，因為 Ferry 沒有讀到資料。三個選擇：信任 kernel 與檔案系統；複製後讀回目的地，與 Source 的雜湊比較；或預設不開。這是[待決定的問題](#待決定的問題)。

### 12. 保證怎麼保住

| 保證（`docs/design/overview.md`） | 現在 | 提案 |
|---|---|---|
| 每個檔案原子就位 | 沒有名稱的暫存檔或暫存名稱，完成後 link 或 rename | 同；大檔續傳時改用有名稱的暫存檔（待決定） |
| 端到端驗證 | 每個 record 的 BLAKE3 | pack 內每個檔案的 BLAKE3；大檔每段的 CV，加上整檔的 root，與現在是同一個雜湊值 |
| 預設持久 | 批次 fsync、macOS 的 barrier、目錄 fsync、`make_durable` | 同，由執行者分批 |
| 讀到開檔時的大小 | record header 的 size 與 mtime | block 的 `len`、`size`、`mtime`；重送的是保留的同一份位元組，不重讀 |
| 不跟隨 symlink、不出根目錄 | `openat`、`O_NOFOLLOW`、`one_name`、manifest 檢查 | 同；fsx 的 API 不變；manifest 檢查改寫，照樣 fuzz |
| `sync` 不刪讀不到的 | `Unreadable`；列不出的目錄不刪其中的東西 | 同；`Unlistable` 讓列不出變成明確的訊息 |
| 先刪多餘，再建立 | applier 在套用前刪 | 每個目錄的閘門；只有 link 要等 |
| 目錄最後收尾、由深到淺 | stack 加 finisher | 由下往上數 |
| Source 的根不進、不刪 | `Guard` 的 avoid 與 protect | 同 |
| 一切需要使用者的事在開始前問完 | prepare 狀態機 | 同；重連需要登入時，是否詢問待決定 |
| 第一個錯誤，全體停止 | `Stop`、`Stages` | 同；錯誤由擁有者分類；stream 斷了不再是錯誤，而是重送的事件；session 放棄時的 `Lost` 也走同一條停止路徑 |

---

## 其他做法

- **只在每一段 transport 加確認與重送（hop-by-hop）。** 在現在 striped 連線的片段上加累計確認、重送緩衝與重新加入，再加 server 的寬限時間。這樣可以重連，而且不必動資料格式。但 HOL、單一 parser、大檔一條 thread、跨次續傳都還在；之後改成 block 時，這一層會變成多餘。只想先要重連時，這是過渡的選項，見[計畫](#不想一次動這麼多時)。
- **直連：host 之間直接以 TCP 連，金鑰經 SSH 發，像 mosh。** 好處是 host 到 host 的資料不經過 client、client 可以中途離開、每條 stream 各自加密而用到多個核心、斷了用金鑰重連不必再登入。但 server 得自己 listen 或往外連、開一個 port，兩台主機得互通，還要多維護一層加密（Noise 或 TLS）。這違反「連線一律由 client 建立」（第 2 節）。window 的上限與多次登入，每條 SSH 連線 K 個 channel 已經解決；剩下的，是保留這個原則的代價。2026-10-23 決定不做。
- **兩側都在本機時，把兩個 peer 放進 client 的 process（in-process）；推送、拉取時，本機的 peer 也放進 client。** 少兩個 process，資料不經過 socket，也不必序列化。但 client 得跑 peer，牴觸「It is never a peer itself」；server control、中斷時清暫存檔、`Stages` 都要在 client 裡另做一套；session 也得多一條不走 frame 的路；本機的測試也不再走遠端用的路。2026-10-23 決定不做：client 改成把 stream 的兩端交給 server（第 2 節），本機到本機只差選項（第 11 節）。
- **QUIC（例如 quinn）。** 多條 stream 互不阻塞、內建 TLS、連線可以遷移。但它要在主機上開 UDP 的 port，同樣違反連線一律由 client 建立；它也需要 async runtime（tokio），和現在的同步 thread 不合；UDP 比 TCP 更常被擋；單一連線的封包處理在使用者空間，CPU 成本高。
- **保留全序，以重排緩衝平行掃描。** 試過：NFS 上快 1.5 到 2 倍，但最忙時對共用伺服器的請求加倍；本機反而變慢；記憶體的上限要靠越來越多特例（`docs/design/pipeline.md:178-187`）。伺服器的負擔是政策的問題，可以讓 K 由使用者決定；重排與特例，才是全序的代價。
- **依目錄分 shard（10-17 的目標架構）。** shard 是為了消除同一目錄的鎖爭用；`O_TMPFILE` 之後，寫入已經不碰目錄的名稱空間。決策者加執行者不必分 shard，就能得到同樣的平行，而且狀態只有一個擁有者。
- **Source 維持各管一部分的 pipeline（10-17 的目標架構）。** manifest builder、want handler、packer、block sender 各一條 thread，每一條都簡單好讀。但 `Known` 要由兩條 thread 共用，stream 斷了要同時動保留的 block 與 control 的序號，大檔各段的 CV 要有人收齊；這些在 stage 之間就是鎖，或來回的訊息與它們的先後。Source 那一半也就沒辦法和 Destination 的擁有者對接，成為可以重現的模擬。
- **讓 host 之間直接用 ssh（agent forwarding）。** 要在主機上放憑證或轉送 agent，牴觸「不在任何主機上放憑證」；OTP 也無法轉送。
- **SSH library、async。** 理由同現在的設計，不採用。
- **以目錄摘要（Merkle 樹）比較。** 能省增量同步的頻寬，但要掃完一棵子樹才知道它的摘要，破壞了「掃描與傳輸重疊」。4.4 節的 manifest 壓縮，已經能拿到大部分的好處。

---

## 和 10-17 目標架構的異同

| 主題 | [目標架構](../review/2026-10-17/target-architecture.md) | 這份草稿 |
|---|---|---|
| 控制面與資料面分開；block（pack、segment）；額度；重連 | 有 | 同意。但把可靠性明確放在兩端的 session，中間的 stream 什麼都不保證，於是 client 只建連線，不碰資料 |
| D16（不做分段） | — | 2026-10-23 重議，決定做：D16 只看單一檔案的吞吐量，block 主要的價值在穩定：重連、續傳、不互相阻塞 |
| 整檔雜湊 | 只驗每個 block，不合併 BLAKE3 的樹 | 保留整檔 BLAKE3，以 `hazmat` 合併子樹 |
| control 走哪條 path（Q5） | 開放 | 平常固定一條，斷了才換 |
| client 永遠在中間、不開 port | 保留 | 同樣保留；client 只建連線，建好就把兩端交給 server，不碰資料 |
| Destination 的平行 | 依目錄分 shard，每個目錄的名稱空間只有一個寫入者 | 一個決策者，加上依操作種類並行的執行者；重點在刪除、mkdir、屬性、收尾也平行，Want 不等刪除 |
| Source 的結構 | 四條 thread（manifest builder、want handler、packer、block sender），各管一部分狀態 | 一個擁有者加執行者，和 Destination 同一個形狀；只有 Destination 做決策 |
| 掃描 | 保留一個 lister、固定的順序 | 看工作負載：以目錄為單位的 page，並行度是政策 |
| 兩側都在本機（Q6） | 量過 relay 不是瓶頸，預設不做 | 同意不放進同一個 process；client 把 socketpair 的兩端交給兩個 server，本機到本機只差選項 |
| 測試 | 故障注入的模擬 | 同意；兩個 peer 的擁有者都寫成 sans-IO 的狀態機，可以對接，以固定的種子重現 |

---

## 待決定的問題

下面每一項都牴觸現在的某個 Decided 或 Not chosen、需要量測，或是這份草稿內部的取捨：

- [x] **端到端確認與重送；stream 斷了不再等於傳輸失敗。** 牴觸 `docs/design/architecture.md:227-236`。2026-10-23：使用者決定做。
- [x] **資料改成 block：pack 與 segment（重議 D16）。** 2026-10-23：使用者決定做。D16 當時只看單一檔案的吞吐量；這次的理由是可以單獨重送、不互相阻塞、續傳不必再改格式。
- [x] **session 嵌在各自的擁有者裡，還是獨立成一層。** 2026-10-23：獨立成一層，是 transport 的最上層，在兩端的 server 裡，有自己的擁有者；peer 只往它的介面送（第 3 節）。原本建議嵌入，理由是 Ack 與額度要和 `Known` 一起改；逐一對過之後，沒有任何事件需要同時改兩邊。
- [x] **連線的健康檢測集中。** 2026-10-23：server 端只由 session 判斷，client 端只由連線管理建立與重建；peer 只收 `Lost` 與 `Closed`；其他元件不持有網路計時器（第 1 節、5.3 節）。
- [ ] **server 的寬限時間多長；要不要可以設定。** 2026-10-23 已決定：server 的壽命改成跟著 session，所有 stream（包括 server control）都斷了才開始計寬限，不再是控制連線斷了就結束。這牴觸「The server lives as long as its control connection」。建議 5 分鐘。長度與要不要能設定，由使用者決定。
- [x] **本機的 peer 在 client 的 process 內。** 2026-10-23：不做；client 把 stream 的兩端交給 server，本機到本機只差選項（第 2、11 節）。理由見[其他做法](#其他做法)。
- [x] **直連。** 2026-10-23：不做；連線一律由 client 建立（第 2 節）。理由見[其他做法](#其他做法)。
- [ ] **manifest 以目錄為 page；K 的預設值；K>1 時 dry run 的順序。** 2026-10-23 以合成的樹量了（第 9 節）：目錄小而多時值得。NFS 上 K=4 就拿到大部分的好處，請求率約加倍；WekaFS 上 K=16 還在變快。由使用者決定。
- [ ] **重連需要登入時，有終端機要不要問。** 這是「傳輸開始後不再問」的例外；沒有終端機時一律不問，以剩下的 stream 繼續，全部斷光就在寬限後失敗。這會真的發生：nano4 每一條新的 SSH 連線都要 OTP（2026-10-23 確認），master 還在時開新的 channel 不必登入，master 斷了重連就一定要使用者。使用者決定。
- [ ] **高 RTT 時，要不要自動多開 SSH 連線。**
  - 背景：主機用預設的 TCP buffer 時，或路上有掉包時，只有多條 TCP 連線才會變快（模擬，5.2 節）。但每條連線都要登入，現在要使用者自己下 `--ssh-connections`，預設是 1。
  - 建議：client 依握手時量到的 RTT 決定要幾條，只自動開「不用輸入就登得進去」的那幾條（以 `BatchMode=yes` 試）。登不進去的，例如要 OTP 的 nano4，就留在原本的 master 上多開 channel，並告訴使用者多開連線能快多少。
  - 使用者決定。
- [ ] **大檔續傳：有名稱的暫存檔、門檻、續傳時要不要從磁碟重算已驗證段的 CV。** 使用者決定，在 A4 之前。
- [ ] **同一個檔案系統時讓 kernel 複製，與驗證的方式。** 只有本機到本機能用（第 11 節）。建議預設不開。使用者決定。
- [ ] **block 大小 B、段大小 S、pack 的閒置時間；資料額度的初始值、上限、要不要自動調整。** 量測決定。
- [x] **名稱操作的並行度 K。** 2026-10-23 量測：WekaFS 上 16 條幾乎線性（×14–16），NFS 上 ×3–10，同一個目錄都不會變快。建議預設 16，同一個目錄一次一個；可以設定。

---

## 計畫

![路線：第 0 步先量測，決定哪些值得做。A 線是資料面與穩定性：A1 block 資料面、額度與 Source 的擁有者，A2 session：端到端確認、重送與重連，client 把 stream 的兩端交給 server，A3 每條 SSH 連線 K 個 channel，好處看主機的 TCP buffer，A4 續傳、sparse、粗粒度 delta。B 線是 metadata 的並行：B1 決策者與執行者（已做），B2 以目錄為單位的 page 與平行掃描，目錄小而多時值得。另有三件隨時可以做的小事：manifest 以一條 stream 壓縮（已做）、刪除交給 pool（已做，B1 的第一步）、列目錄不重開並加大緩衝區（已做）](assets/redesign-roadmap.svg)

每一步結束時，都要能執行、測試過；rsync parity 測試與 `ferry test` 的情境，就是行為不變的安全網。A 線與 B 線大致獨立，可以平行，但都會改 `src/destination/`，要錯開。整個傳輸的可重現模擬，得等 B1 讓 Destination 也有擁有者；session 自己的模擬不必等。

### 第 0 步：量測

先回答 overview 的兩個 open question。

| 量什麼 | 在哪裡 | 怎麼量 | 決定什麼 | 2026-10-23 |
|---|---|---|---|---|
| 大型樹的增量同步，幾乎沒有變動 | nano4 的 WekaFS（login node）、fatcat 的 NFS | 例如 1,000 萬個項目、10 萬個目錄；`--profile-file`，看 lister 與 compare 的忙碌程度；另以獨立的小程式量 K 個 lister | 第 9 節（B2）值不值得做 | 以合成的樹量了（每種 5 萬個項目）：一個 lister 是瓶頸，B2 在目錄小而多時值得（第 9 節）。還差真實的大樹，以及伺服器沒有快取的情況 |
| 大量刪除的 `sync` | 同上 | `old/` 裡 100 萬個多餘的檔案 | B1 的優先順序 | 以每次操作的時間推算：WekaFS 上一條 thread 約 11 分鐘，16 條跨目錄約 40 秒 |
| 大量目錄與 symlink 的首次複製 | 同上 | 10 萬個空目錄、10 萬個 symlink | B1 | 同樣推算：WekaFS 上 mkdir 0.69 ms、symlink 0.66 ms，16 條跨目錄 ×14–15 |
| 單一 SSH 連線的吞吐量與 RTT | 實際常用的鏈路（國網、跨國） | 大檔：1 條與 4 條 SSH 連線，每條 1 個與 K 個 channel | K 個 channel 的效果；一個 ssh process 的加解密上限 | fatcat、nano4 量了，多開 channel 沒有好處。高 RTT 以模擬量了（5.2 節）：預設 buffer 時 channel 最多 ×1.35，多條連線 ×C；buffer 調大時 channel 約等於連線。真實的長距離鏈路還沒量 |
| 登入方式 | 常用的 host | OTP、金鑰、Kerberos | 重連時需不需要使用者（待決定的問題） | fatcat 用金鑰；nano4 每條新連線都要 OTP |

### A 線：資料面與穩定性

**A1：block 資料面，加上資料額度。**

- 做什麼：
  - 新的 block 格式（pack、segment），取代 `record.rs` 與 `frame.rs` 的資料串流；
  - Source 改成擁有者加執行者（第 7 節）：reader 讀小檔與大檔的段，壓縮的 pool 組成 block，擁有者只收事件、發命令；拿掉 `Known` 與 `Backlog` 的鎖；掃描先不動；
  - Destination 的解碼 pool 直接交給寫入，拿掉 parser；segment 以 `pwrite` 寫；
  - 整檔雜湊以 `hazmat` 合併；
  - 資料額度取代 `DATA_QUEUE` 與 `DECODED`。
  - Destination 錯誤的分類收進決策者，執行者（建立、刪除、收尾、寫入）只回報結果；從 B1 移來（2026-10-23）。
  - transport 不動，仍然有序、斷了就失敗；session 留到 A2。
- 結束時：所有測試與 `ferry test` 通過。新增的測試：
  - 分段算出的雜湊等於 `blake3::hash`，涵蓋各種長度、最後一段很短的情況；
  - 一段讀不完整時，整個檔案失敗；
  - 段的 CV 不符、或 root 不符時，檔案失敗，什麼都不留下；
  - Source 的擁有者以合成的事件序列做單元測試：Want、讀完、讀不完整、額度用完的各種先後。
  - Destination 的決策者也以合成的事件序列做單元測試：各種到達順序、失敗、別名；從 B1 移來（2026-10-23）。
- 驗收：
  - 4 GiB 單檔的本機複製，吞吐量隨 writer 數上升。要在 tmpfs 上量：這台的 ext4 RAID 寫入只有約 0.49 GB/s，現在就已經卡在磁碟。基準（2026-10-23，tmpfs → tmpfs）：3.1 秒、約 1.35 GB/s，Destination 一個 writer 約 95% 在忙；
  - 20 萬個 4 KiB 的檔案不變慢。基準：tmpfs → tmpfs 3.1 秒；tmpfs → ext4 8 到 9 秒，和 `cp -r` 加 `sync` 相同；
  - profile 中，沒有一條 thread 專門讀大檔，大檔不再擋住小檔；
  - 以合成的事件量 Source 的擁有者：每秒能處理的小檔數，比本機 4 KiB 小檔的實際速率高一個數量級。

**A2：session：端到端確認、重送與重連。**

- 做什麼：
  - session：transport 的最上層，在兩端的 server 裡，寫成不做 I/O 的狀態機；control 的序號、累計確認與重送緩衝；block 的編號、保留、確認、去重與重送；
  - peer 改成只往 session 的介面送 control 與 block；
  - transport 改成一組獨立的 stream，拿掉全域重排；
  - client 不再轉送：建好 stream 就把兩端交給 server（`SCM_RIGHTS`，同 `join`），拿掉 `relay.rs`；stream 的狀態改由 server 經 server control 回報；
  - 報告改走 server control；
  - 連線的健康集中（5.3 節）：
    - session 判斷每條 stream，包括 server control；
    - peer 只收 `Lost` 與 `Closed`；
    - client 的連線管理負責建立與重建；
    - 拿掉 heartbeat channel、path 的結束標記與 `LET_GO`；
  - server 的寬限時間，壽命跟著 session；client 重建 stream，新的 stream 以 session ID 加入。
- 結束時：
  - 兩個 session 以模擬的 stream 對接（隨機延遲、隨機斷、重複），兩端接上假的 peer，跑數百個種子：同一個種子結果相同，每則 control 恰好一次、依序，每個 block 恰好一次。這不必等 B1；
  - session 的健康判斷，以合成的事件與外面給的時間測：沉默、只有一個方向通、時斷時續、寬限到期、寬限到期後才回來的 stream。peer 只在寬限到期時收到 `Lost`，正常結束時收到 `Closed`；
  - 除了 session 與 client 的連線管理，程式裡沒有其他網路計時器；
  - 整個傳輸（兩個 peer 連同 session）的可重現模擬，要等 B1；在那之前，以真的 thread 跑同樣的情境，種子只決定何時斷線，失敗時不一定重現得出來；
  - 三種組合的 `ferry test` 都通過，而且 client 的 process 沒有經手任何資料；
  - `ferry test` 的 `--cut-a-path` 情境改為預期成功；
  - 傳輸中殺掉一個 ssh master，傳輸接著完成。

**A3：每條 SSH 連線 K 個 channel（好處看主機的 TCP buffer）。**

- 量測（2026-10-23，5.2 節）：在 fatcat（RTT 1.7 ms）與 nano4（6.5 ms）上都沒有好處。高 RTT 的模擬中，主機用預設的 TCP buffer 時最多快 ×1.35；buffer 調大時接近 ×K，等於多開連線而不用多登入。
- 建議：做，排在 A2 之後；K 依握手時量到的 RTT 決定，例如每 20 ms 一個、最多 8 個。短路徑上 K=1，不會吃虧。
- 做什麼：每條 SSH 連線開 K 個 channel，每個 channel 一條 stream。
- 結束時：K 個 channel 的吞吐量，對照第 0 步的量測；`ferry test` 對每一種組合都跑一次。

**A4：** 跨次執行的大檔續傳；sparse（`Hole` block）；粗粒度的 delta（Destination 送出已存在的大檔的段雜湊，Source 對相同的段回「沿用」）；hardlink。各自一個工作包。

### B 線：metadata 的並行

**B1：Destination 的決策者與執行者。**

- 進度：
  - 第一步，刪除平行，已做（2026-10-23；見[隨時可以做的小事](#隨時可以做的小事)）。
  - 第二步，目錄收尾平行，已做（2026-10-23）：最多 16 條 thread，一個目錄要等它底下要收尾的目錄都收完尾才收尾。首次複製 20,200 個空目錄，ext4 從 3.5 秒變 1.1 秒，NFS 從 13.2 秒變 6.8 秒，WekaFS 從 71 秒變 49 秒；NFS 與 WekaFS 上剩下的瓶頸，是 applier 一個一個建立目錄。
  - 第三步，建立交給執行者（決策者的前半），已做（2026-10-23）：applier 把一段要改的東西交給最多 16 個建立者，每個各負責一個目錄，自己接著處理下一個目錄；子目錄的各段等父目錄那一段把它建好；一段的 Want 在這一段做完才送，`Needs::Through` 不越過還沒送的 Want；目錄要等建立或修改它的各段做完才算完成。首次複製 20,200 個空目錄，NFS 從 6.5 秒變 2.0 秒，WekaFS 從 25 秒變 4.3 秒；本機的小檔複製沒有變慢。
  - 第四步，刪除另走一條線（決策者的後半），已做（2026-10-23）：applier 把一個目錄的多餘項目交給刪除線，自己接著往下做；只有那個目錄的各段等刪除做完，Want 也跟著晚送；`Needs::End` 等所有的段與刪除都做完。刪除線一次一個目錄，裡面照樣是最多 16 條 thread，所以同時最多約 32 個名稱操作。一次 sync 在一個目錄刪掉 2 萬個檔案、在另一個目錄複製 2 萬個新檔案，WekaFS 從 24.6 秒變 13.5 秒，NFS 從 10.1 秒變 7.9 秒，差不多就是刪除本身的時間。
  - 驗收（2026-10-23）：
    - WekaFS（nano4），B1 開始前（`7e7cb0a`）與四步之後（`5b81a9a`）輪流，各一輪：`sync` 刪掉 2,000 個目錄裡的 20 萬個多餘檔案，177 秒變 8.95 秒（×20），16 條刪除的 thread 從頭忙到尾，每個名稱約 0.7 ms；首次複製 50,500 個空目錄，56.8 秒變 9.7 秒（×5.8），剩下的是收尾：每個目錄開啟、stat、設權限與 mtime、fsync，約 2.8 ms，16 條約九成的時間在忙。
    - 本機，20 萬個 4 KiB 的檔案：ext4 沒有變慢（各 3 輪，中位數 8.44 / 8.51 秒）。tmpfs 起初像是新版有幾輪變慢，但那時主機上有別的 session 留下的 48 個空轉迴圈，已經跑了 33 小時；停掉之後各量 12 輪，兩版都有慢的幾輪（舊版 5 輪落在 3.9–5.2 秒，新版 6 輪落在 4.0–5.4 秒），中位數 3.21 / 3.58 秒。
    - 慢輪的原因是 `--cpus` 的 8 個核心跨了兩個 NUMA node。這台的 node 0 是核心 0–23 與 48–71，node 1 是其餘的；`--cpus` 從 PID 決定的位置取編號連續的 8 個，約三成會跨過去。整個量測釘在 node 0 上，兩版各 8 輪都在 2.6–3.4 秒，中位數 2.95 / 2.84 秒。同一個新版，釘在核心 0–7 上，中位數 2.79 秒；釘在跨兩個 node 的 20–27 上，4.26 秒，每一輪都比較慢。慢的那輪的 profile 裡，慢的是 writer 自己寫檔，寫入的時間是平常的 2.5 倍。所以 B1 沒有讓 tmpfs 變慢。之後 `--cpus` 改成只從一個 node 取核心，本機的 server 也和 client 取同一組（2026-10-23）：不釘核心，舊版與新版各 12 輪，中位數 3.07 / 3.06 秒；新版最慢 3.64 秒，舊版有一輪跨了 node，4.72 秒。
    - 小檔時建立者忙的時間，各輪差很多（0.06–2.1 秒；有那些迴圈時量的），還沒查。
    - 決策者的事件吞吐量，以 `--profile-file` 量 applier 忙的時間（tmpfs，各 3 輪，同樣有那些迴圈）：首次複製 10 萬個空目錄，每個目錄 4–6 µs，約三成的時間在忙；20 萬個小檔，每個檔案 1–2 µs（連同寫完的事件），九成以上的時間在等。一條 thread 還不是限制；最忙的是目錄多、檔案少的時候。
  - 和計畫不同的地方：
    - 量得比計畫小：刪除 20 萬個，不是 100 萬個；目錄 50,500 個，不是 10 萬個目錄加 10 萬個 symlink，symlink 沒有大量量過；各只一輪，因為舊版一輪就要幾分鐘。照每個名稱的時間推算，100 萬個多餘的檔案，舊版約 15 分鐘，新版約 45 秒，和第 0 步推算的 11 分鐘、40 秒相近。
    - 「大刪除時 Want 與資料照常流動」，用的是第四步的量測（一個目錄刪 2 萬個，另一個目錄複製 2 萬個），沒有在更大的量上再量。
    - 決策者的吞吐量是從 profile 算的，不是「以 A1 同樣的方法」：A1 還沒有。
    - Want 不是比較完就送：在 A1 之前，資料仍是 record 串流，寫入那一條線自己跑，所以目錄 D 的 Want 等 D 的段做完、多餘的刪完才送；`Needs::Through` 也不能越過還在等的 Want。草稿原本寫「Want 比較完就送，只有 link 等閘門」，要等 A1 讓 block 成為決策者的事件。
    - 錯誤的分類沒有收進決策者：建立者自己判斷一個錯誤是失敗、警告還是要停，警告也自己記下；決策者只負責告訴 Source 與使用者。刪除的結果則由決策者分類。決定（2026-10-23）：併到 A1，和寫入一起改。
    - 「結束時」有兩項沒做：決策者沒有以合成的事件序列做單元測試，它仍直接收比較的結果，測試都經過真的檔案系統（新加的兩個，拿掉保護就失敗）；也沒有以模擬的高延遲檔案系統比較，改在真的 NFS 與 WekaFS 上量。決定（2026-10-23）：合成事件的單元測試併到 A1，那時 block 也成了決策者的事件，不必寫兩次；模擬的檔案系統不做，以真的量測代替。
    - applier 還做的 I/O：root 一次、`-x` 時 root 的 fstat，以及 dry run 列出時的 stat。
- 做什麼：
  - applier 拆成決策者（不做 I/O 的狀態機）與執行者（pool）；
  - 刪除（`remove_tree` 依子目錄拆開平行）、mkdir、symlink、屬性、目錄收尾，都交給執行者；
  - Want 比較完就送，link 等每個目錄的閘門；
  - 錯誤的分類收進決策者，執行者只回報結果。
- 結束時：
  - 決策者以合成的事件序列做單元測試：各種到達順序、失敗、別名；
  - 以模擬的高延遲檔案系統（每個 metadata 呼叫加延遲）比較改動前後；
  - rsync parity 與 destination 的測試全過。
- 驗收：WekaFS 上，100 萬個分散在許多目錄的多餘檔案的 `sync`、10 萬個目錄的首次複製，時間明顯下降；大刪除進行時，Want 與資料照常流動；ext4 不變慢；決策者的事件吞吐量，以 A1 同樣的方法量。

**B2：以目錄為單位的 page（目錄小而多時值得）。**

- 量測（2026-10-23，第 9 節）：目錄小而多的樹，WekaFS 上 K=4 到 16 快 ×4–7，NFS 上 K=4 快 ×2.3。一般大小的目錄在 WekaFS 上卡在 stat（×1.5），超大目錄在 NFS 上也卡在 stat（×1.0）。所以 B2 要和 stat 的吞吐量一起做；列目錄的小改動先做。
- 做什麼：`Page` 與 `Unlistable`；改寫 manifest 檢查；K 個 lister；Destination 平行比較；完成改成由下往上數；`Known` 以 page 釋放。
- 結束時：fuzz 涵蓋任意順序的 page；K=1 時 dry run 的輸出與現在完全相同。

### 隨時可以做的小事

這三件不依賴上面任何一步，現在的架構就能做；三件都已經做了，第二件是 B1 的第一步：

- **manifest 壓縮。已做（2026-10-23）。** 資料壓縮時，Source 把 manifest 的訊息壓在同一條 zstd stream 裡，每則之後 flush，成為新的一種封包，Destination 依序解；不改任何順序。problem 會搶在各段之前送出，不能放進這條 stream，所以照舊原樣送。用真的編碼器量：`/usr` ×3.2、crate 的原始碼 ×3.4、建置產物 ×2.6、扁平目錄 ×14；manifest 的執行緒每個項目多花不到 1 µs。模擬的鏈路上（4.4 節的方法以外另造一棵 22 萬個項目的樹，大小與 mtime 近乎亂數，只壓 ×1.6），沒有變動的 `sync -n`：RTT 30 ms、100 Mbps 從 1.08 秒變 0.85 秒；120 ms、1 Gbps 從 2.90 秒變 2.29 秒；250 ms 從 3.51 秒變 3.28 秒，那裡啟動的十幾個 RTT 佔了大半。
- **刪除平行。已做，是 B1 的第一步（2026-10-23）。** 一個目錄多出的項目交給最多 16 條 thread，每條清空一個目錄；列出的結果說不是目錄的，直接 unlink，所以一個檔案只要一趟往返，不再先試著把它當目錄打開。applier 仍然等刪除做完，所以仍然擋住後面的 needs；完整的解法是 B1 接下來的決策者。量到的：500 個目錄裡的 5 萬個檔案，WekaFS 上從 44 秒變 2.3 秒，NFS 上從 8.5 秒變 1.7 秒；一個目錄裡的 2 萬個檔案，NFS 上從 5.8 秒變 3.5 秒；一個目錄裡的 1 萬個檔案，WekaFS 上從 8.7 秒變 6.7 秒。
- **列目錄：不重開目錄，加大緩衝區（Linux）。已做：`7e7cb0a`。** 之前用的是 rustix 的 `Dir::read_from`（`20b7586` 的 `src/fsys.rs:568-584`）。它會先以 `openat(fd, ".")` 把目錄再開一次，讀取的緩衝區從 768 位元組開始，最多只到 24 KiB。2026-10-23 量到：
  - **NFS：** 多開一次，就多一次 close-to-open 的確認。Ferry 列一個目錄比只開一次的小程式多約 0.17 ms（0.47 對 0.30 ms），正好是一次 GETATTR 的時間。
  - **WekaFS：** 每次 getdents 約 7–10 ms，所以呼叫次數決定時間。5,000 個項目的目錄，現在的做法 94 ms，32 KiB 的緩衝區 41 ms，128 KiB 16.5 ms；2 萬個項目分別是 341、245、99 ms。
  - **做法：** 改用 `RawDir` 直接讀已經開好的 fd，先 seek 回開頭，緩衝區 256 KiB；macOS 不變。2026-10-23 以 `7e7cb0a` 合併，測試全過；`docs/design/pipeline.md` 與 CHANGELOG 也寫了。用同樣三種樹，舊版與新版輪流各量 3 輪 cold 與 warm，列一個目錄的時間：

| | 5,000 個目錄 × 10 | 500 × 100 | 10 × 5,000 |
|---|---|---|---|
| NFS，cold | 0.40 → 0.28 ms（×1.4） | 0.69 → 0.56 ms（×1.2） | 9.9 → 9.4 ms（×1.0，卡在 stat） |
| NFS，warm | 0.27 → 0.12 ms（×2.3） | 0.25 → 0.15 ms（×1.7） | 在雜訊之內 |
| WekaFS，cold | 0.48 → 0.45 ms（×1.1） | 1.67 → 0.93 ms（×1.8） | 99 → 22 ms（×4.6） |
| WekaFS，warm | 0.34 → 0.29 ms（×1.2） | 1.52 → 0.78 ms（×2.0） | 95 → 23 ms（×4.1） |

NFS 上 5,000 個目錄的樹，GETATTR 少了約 3,800 次（每個目錄少一次確認，扣掉別人的流量）。整次 dry run 的時間：WekaFS 上 10 × 5,000 從 1.7 秒變 0.6 秒、500 × 100 從 0.9 秒變 0.6 秒；NFS 上 5,000 × 10 從 3.1 秒變 2.5 秒。本機的差別在幾 µs 之內，系統 CPU 時間少了約 15–20%。

每條 SSH 連線 K 個 channel 原本也列在這裡（`docs/design/architecture.md:280-292` 已經是 Proposed）；在 fatcat 與 nano4 上量到沒有好處，高 RTT 的好處又看主機的 TCP buffer（5.2 節），所以改列為 A3。

### 不想一次動這麼多時

只想先要重連：在現在每一段 striped 連線上加累計確認、重送緩衝與重新加入，再加 server 的寬限時間（見[其他做法](#其他做法)的第一項）。不動資料格式，但 HOL、大檔一條 thread、跨次續傳都還在，之後改成 block 時，這一層會變成多餘。

---

## 驗證

### 已驗證（2026-10-23）

在這台機器（96 核心的 container；ext4 是三顆 SATA SSD 組成的 RAID0）、fatcat 的 NFS（NFSv4.1，`nconnect=8`）與 nano4 的 login node（WekaFS）上量。量測的腳本在工作目錄的 `.scratch/verify/`，高 RTT 的模擬在 `.scratch/sim/`，都沒有進 git。

| 項目 | 怎麼驗 | 結果 | 對計畫的影響 |
|---|---|---|---|
| 分段的整檔 BLAKE3（4.3 節） | 5 種段大小、156 種長度；各段在不同的 thread 算 CV，依樹的形狀合併 | 全部等於 `blake3::hash`；改一個位元組，root 就不同 | 整檔雜湊的設計成立 |
| client 交出 stream 的兩端（第 2 節） | 真的 ssh：socketpair 當 ssh 的標準輸入輸出；兩個 ssh 以兩條 pipe 或一對 socketpair 背對背，經 ControlMaster 與不經過，也跨 fatcat 與 nano4 | 兩端的 SHA-256 都相同；殺掉一端的 ssh，另一端在 0.2 秒內跟著結束 | 機制成立 |
| socketpair 的成本（第 11 節） | 這台機器上傳 16 GiB | 一對 socketpair 每 GB 0.25 CPU 秒、約 8 GB/s；同一個 process 裡 0.03；現在的兩段 socket 加轉送 0.45 | 提案比現在省 |
| 擁有者的事件吞吐量（3.1 節） | 合成：16 個 reader、8 個壓縮，執行者不做 I/O | 一個一個回報約 27 萬檔/秒；每 16 個成批 430 萬，每 64 個 570 萬 | 一條 thread 夠用，但必須成批 |
| manifest 壓縮（4.4 節） | Ferry 自己的掃描與編碼，四棵樹；做了之後，以真的編碼器再量一次，另在模擬的鏈路上量沒有變動的 `sync -n` | 每個 page 各自壓 ×1.5–2.0；一條 stream ×2.6–3.6；規律的扁平目錄 ×18–19。做了之後：×2.6–3.4，扁平目錄 ×14；RTT 30 ms、100 Mbps 上 22 萬個項目 1.08 → 0.85 秒 | 改成一條 stream；已做 |
| 名稱操作的並行（R5、第 8 節） | 16 個目錄 × 250 個檔案，1、4、16 條 thread；另以 16 條擠在同一個目錄 | WekaFS 每次 0.53–0.84 ms，跨目錄 16 條 ×14–16；NFS 0.13–0.42 ms，×3–10；同一個目錄都不會變快 | B1 的前提成立；K 預設 16 |
| 一個 lister 的速度（R5、第 9 節） | 現在的 Ferry 做 dry run，加 `--profile-file`；三種合成的樹各 5 萬個空檔（5,000 個目錄 × 10、500 × 100、10 × 5,000），cold 與 warm 各 3 輪 | 列一個目錄約 0.5 ms，每個項目再加 10–25 µs：每秒 1.5–7 萬個項目，本機是幾十萬到兩百多萬。warm 時 NFS 快約 5 倍，WekaFS 幾乎一樣。lister 佔掃描時間的 60–98%；NFS 的超大目錄例外，每個 stat 都是一次 GETATTR | 掃描的瓶頸是 lister |
| K 個 lister（第 9 節） | 獨立的小程式：K=1、4、16 個 lister 共用目錄佇列，配 0、8、32 條 stat；同樣三種樹，每次 cold，3 輪；NFS 另讀 `/proc/self/mountstats` | 目錄小而多時，WekaFS ×4–7、NFS ×2.3；WekaFS 的一般目錄與 NFS 的超大目錄卡在 stat；NFS 的請求率約加倍 | B2 在目錄小而多時值得，要和 stat 一起看 |
| 列目錄的緩衝區（隨時可以做的小事） | WekaFS 上同一個目錄，以不同大小的 getdents 緩衝區各列 5 次 | 5,000 個項目：rustix 的做法 94 ms，32 KiB 41 ms，128 KiB 16.5 ms；2 萬個項目：341、245、99 ms | 列目錄先改 |
| 改用 `RawDir`（隨時可以做的小事） | `7e7cb0a`；舊版與新版輪流做 dry run，同樣三種樹，cold 與 warm 各 3 輪 | 列一個目錄：NFS 快 ×1.2–1.4（warm ×1.7–2.3），每個目錄少一次 GETATTR；WekaFS 一般目錄 ×1.8–2.0，大目錄 ×4.1–4.6，小目錄 ×1.1–1.2；本機差別在幾 µs 之內 | 和推算相符，已合併 |
| 刪除平行（B1 的第一步） | `sync` 刪掉多出的 `old/`；舊版（HEAD）與新版輪流，各 3 輪：500 個目錄 × 100 個檔案，以及 1 個目錄 × 2 萬個檔案；WekaFS 的單一目錄另以 1 萬個檔案量 5 輪，建好樹後等 30 秒（剛建好就刪，第一次會慢，可能是 `writecache` 還在寫回）；另以三種刪法對照每個檔案的時間 | 500 個目錄：WekaFS 44 → 2.3 秒（×19），NFS 8.5 → 1.7 秒（×5.0），本機 ×3.5–4；一個目錄：NFS 5.8 → 3.5 秒（×1.7），WekaFS 8.7 → 6.7 秒（×1.3），本機 ×1.3–1.4。WekaFS 上一個檔案只 unlink 要 0.6 ms，先試著打開再 unlink 要 0.83 ms | 已合併 |
| 目錄收尾平行（B1 的第二步） | 首次複製 20,200 個空目錄（200 × 100），來源在主機的 `/dev/shm`；舊版（刪除平行之後）與新版輪流，各 3 輪 | ext4 3.5 → 1.1 秒（×3.3）；NFS 13.2 → 6.8 秒（×1.9）；WekaFS 71 → 49 秒（×1.45）；tmpfs 差不多。NFS 與 WekaFS 上，applier 建立目錄成了剩下的瓶頸（WekaFS 38–55 秒） | 已合併；下一步是決策者 |
| 建立交給執行者（B1 的第三步） | 同上，首次複製 20,200 個空目錄，舊版（收尾平行之後）與新版輪流，各 3 輪；另以 20 萬個 4 KiB 的檔案檢查本機有沒有變慢；單元測試：子目錄的各段等父目錄、`Needs::Through` 不越過還沒要過的檔案（拿掉保護就失敗） | NFS 6.5 → 2.0 秒（×3.3），WekaFS 25 → 4.3 秒（×5.6），ext4 1.1 → 0.8 秒；小檔 tmpfs 4.26 / 4.21 秒、ext4 6.99 / 6.88 秒，沒有變慢。從 B1 開始前算起，NFS 13.2 → 2.0 秒（×6.6）；WekaFS 的絕對值各次差很多（叢集負載），以同一次交錯量的為準 | 已合併 |
| 刪除另走一條線（B1 的第四步） | `sync`：Destination 的 `a/old/` 有 2 萬個空檔（同一個目錄，刪除不能平行），Source 在 `b/` 有 2 萬個 4 KiB 的新檔案；舊版（建立交給執行者之後）與新版輪流，各 3 輪，建好樹後等 30 或 65 秒 | WekaFS 24.4–24.8 → 13.2–13.8 秒（第 0 輪 38.4 → 25.6 秒），NFS 9.9–10.5 → 7.9 秒；新版整次 sync 差不多等於刪除本身，刪除和複製同時進行時，刪除本身稍慢（NFS 2.8 → 3.5–4.9 秒）。結果都正確：`a/old/` 刪掉，`b/` 2 萬個檔案都在 | 已合併 |
| B1 的驗收 | nano4 的 WekaFS，B1 開始前（`7e7cb0a`）與四步之後（`5b81a9a`）輪流，各一輪：`sync` 刪掉 2,000 個目錄 × 100 個檔案，建好後等 30 秒；首次複製 500 × 100 個空目錄，來源在 `/dev/shm`。本機：20 萬個 4 KiB 的檔案，ext4 各 3 輪；tmpfs 在主機安靜時各 12 輪，釘在一個 NUMA node 上各 8 輪，同一個 build 釘在核心 0–7 與 20–27 上各 8 輪；applier 忙的時間以 `--profile-file` 量，tmpfs 上首次複製 10 萬個空目錄與 20 萬個小檔，各 3 輪 | 刪除 177 → 8.95 秒（×20）；目錄 56.8 → 9.7 秒（×5.8），剩下的是收尾，每個目錄 2.8 ms。ext4 8.44 / 8.51 秒；tmpfs 兩版一樣，各 12 輪都有約四成慢 1–2 秒，原因是 `--cpus` 的 8 個核心跨了兩個 NUMA node：釘在一個 node 上，中位數 2.95 / 2.84 秒，同一個 build 跨 node 時 ×1.5。applier：每個目錄 4–6 µs，約三成在忙；每個小檔 1–2 µs，九成以上在等 | B1 完成；`--cpus` 已改成只從一個 node 取核心 |
| SSH 的吞吐量（5.2 節） | `ssh -W`：1 條連線 1、4、8、12 個 channel，以及 4 條連線；交錯各量 3 次 | fatcat：1 條約 60 MB/s，多開 channel 到 65–67，4 條連線 104；nano4：1 個 channel 就到 116（1 GbE 的上限） | 短路徑上多開 channel 沒有好處 |
| 高 RTT 的 SSH（5.2 節，模擬） | 兩個 network namespace 以 TUN 相接，自寫的程式加上延遲、1 Gbps 與 50 ms 的佇列；真的 kernel TCP 與 OpenSSH；RTT 2、30、120、250 ms；TCP buffer 用預設與 64 MiB；另加 0.01% 的掉包；每項量 30 秒 | 一個 channel 每個 RTT 2 MiB；預設 buffer 時多開 channel 最多 ×1.35，多條連線 ×C；buffer 調大時 K 個 channel 約等於 K 條連線；有掉包時一條 TCP 掉到約 4 MB/s | A3 的好處看主機；自動多開連線列為待決定 |
| 現在的 Ferry，高 RTT（5.2 節，模擬） | 同上；256 MiB 的檔案、2 萬個 4 KiB 的檔案、一個 1 KiB 的檔案 | 1 條連線照 2 MiB/RTT：58、15、7.4 MB/s（30、120、250 ms）；4 條連線 ×3.0；經 ControlMaster ×1.15；小檔沒有逐檔的往返；啟動約 12 個 RTT | 資料額度要涵蓋頻寬 × RTT（第 6 節） |
| 登入方式 | 以 `BatchMode` 開新連線 | fatcat 用金鑰；nano4 每條新連線都要 OTP，只能共用已登入的 master | 重連的那個待決定問題會真的發生 |
| 本機的基準（A1） | 現在的 Ferry，20 萬個 4 KiB 的檔案與一個 4 GiB 的檔案 | 小檔：tmpfs 3.1 秒，ext4 8–9 秒（同 `cp -r` 加 `sync`）；大檔：tmpfs 3.1 秒（1.35 GB/s，一個 writer 約 95% 在忙），ext4 9.9 秒（磁碟本身 8.8 秒） | A1 的驗收要在 tmpfs 上量 |

量 lister 與 K 個 lister 時，fatcat 約有 180 人登入，nano4 的 login node load 150–185（216 核），所以同一項各輪可以差到 2 倍。表中是 3 輪的中位數或範圍。高 RTT 的模擬只有一台機器、每項一輪，結果和 window 的推算吻合到幾個百分比。

### 還沒驗證

- **真實的長距離鏈路：** 模擬只有固定的延遲、固定的速率與隨機的掉包。真實路徑上的突發掉包、重排、限速、中間設備與別人的流量都沒有。這台的 kernel 不能用 BBR，所以也沒有比較 BBR。
- **啟動的 12 個 RTT：** 是哪些往返還沒查；250 ms 時是 3.1 秒，小量的同步會被它主導。
- **真實大樹的增量同步：** 合成的樹已經量了（上表）。還差一棵真實的大樹（看它的形狀，以及伺服器沒有快取時的情況），以及 B2 做出來之後整個 Ferry 的掃描。之前在 NFS 上試過平行掃描，134,000 個項目快 1.5 到 2 倍，但共用伺服器最忙時的請求加倍（`docs/design/pipeline.md:178-187`）；這次的小程式也看到請求率約加倍。
- **block 大小 B、段大小 S、pack 的閒置時間、資料額度**：要等 A1 的程式。
- **真正的擁有者的吞吐量**：B1 的 applier 量了，從 profile 算忙的時間（上表）；A1 讓 block 成為事件之後，要再量一次。- 讀過核心模組：transport、mux、relay、peer、source、scan、destination 的各模組；dashboard、testkit、遠端安裝只看了結構。
