# 架構重新設計：端到端可靠、只保留必要的順序、每個 peer 一個擁有者

- 狀態：已採納（2026-10-23：兩個核心問題都決定做）；B1 已做完（2026-10-23）：四步與驗收；計畫中錯誤的分類與合成事件的單元測試併到 A1，模擬的高延遲檔案系統不做，見 [B1](#b-線metadata-的並行)；隨時可以做的三件小事也都做了（2026-10-23）；A1 的六步也做了（2026-10-23）：資料改以 block 傳送，B 與 S 量測後定為 2 MiB；Source 改成擁有者加執行者；資料額度；建立與寫入的錯誤由決策者分類，決策者的狀態以合成事件測試；驗收四項中兩項達到，4 GiB 單檔沒有變快（慢約 5%），大檔擋小檔只改善了一部分；2026-10-24 在硬碟上量到 A1 讀大檔慢約七成，原因是平行讀段，改成大檔照順序、一次一個之後，硬碟與本機都回到 A1 之前的速度，小檔也大多不再等大檔，但 NFS 上 A1 的平行讀段快約四成，使用者決定保留照順序讀，依檔案系統決定的做法列為[改進選項](#改進選項)，見 [A1](#a-線資料面與穩定性)；第五步沒有合併的兩件，2026-10-24 決定在 A2 之前單獨做，同一晚 B1 的補完五步也做了：決策者不做 I/O，收尾、分段檔案與改名的時機都移進決策者，writing 這條執行緒拿掉，Want 比較完就送；NFS 上同一個目錄刪除時先寫檔反而慢一成三，所以只在檔案寫成沒有名稱的地方和刪除重疊，見 [B1 的補完](#b-線metadata-的並行)；A2 的第一步也做了（2026-10-24）：session 的核心，不做 I/O，以模擬的 stream 對接數百個種子測過，block 的確認因此改成累計的，還沒接上，見 [A2](#a-線資料面與穩定性)；2026-10-25：使用者同意 A2 的第二到第六步，第二步做了：frame 的編碼、stream 的讀寫執行者與 session 的驅動者，以 socketpair 測過，還沒接上；第三步也做了：報告改走 server 的 control 連線；第四步也做了：client 建 stream、把兩端交給 server，不再經手資料，peer 改走 session，relay、全域重排與 heartbeat 拿掉；第五步也做了：stream 斷了由 client 重建，SSH 連線斷了先不問地重新登入，不行再在終端機上問；第六步驗收也做了：設計文件、圖與 CHANGELOG 照做好的改寫，stream 一再斷掉的複製以種子測過，4 GiB 單檔快了約一成；server control 沒有交給 session 判斷，所以第一條 SSH 連線斷了仍會失敗，要不要做由使用者決定，見 [A2](#a-線資料面與穩定性)；A1 驗收沒做到的兩項（單一大檔沒有變快、排在前面的大檔讓小檔等），列為[改進選項](#改進選項)；2026-10-25 重新決定：nano4 的同一個主機名稱後面有好幾台 login node，多條 SSH 連線與斷線後重新登入的連線都可能落在沒有 server 的機器上，使用者決定每個 host 只有一條 SSH 連線、斷線就失敗、不重連，`--ssh-connections` 拿掉，session 縮成薄的 stream 層、只有一條 stream，A3 改列為改進選項，見[重新決定](#2026-10-25-重新決定一條-ssh-連線斷線就失敗)；同一天 A2 的收回五步也做完了：一條 SSH 連線、薄的 stream 層、拿掉重連、client 看 control 的沉默、驗收與文件；驗收時找到兩個問題，推上去之後 CI 又找到四個（fuzz 的建置、macOS 上交給本機 server 的 stream 與等它收下的逾時、寫入端說不出 stream 為什麼斷），都已修好；nano4 上的驗收在推上去之後補做了：三種組合的 `ferry test` 各 39 個全過，1 GiB 兩個方向的複製與同步照常，見 [A2 的收回](#a-線資料面與穩定性)
- 開始：2026-10-22；最後修改：2026-10-25
- 基準：HEAD `20b7586`（0.16.0）。以下的程式位置都以它為準。
- 會改動（實作時；在那之前，這些文件以 Proposed 指向這份草稿）：
  - `docs/design/architecture.md`：「Client and servers」、「The client relays between the servers」、「Transport」（原則 2、4、7）、「Connections」（heartbeat、end marker、server 綁在第一條 SSH 連線）、「Packets on one connection」；
  - `docs/design/pipeline.md`：「Principles」（stage 與上限）、「1. Scan」（一個 lister、固定順序）、「2. Compare」（applier）、「3. Read and hash」到「6. Write」（record 串流、frame、parser）、「8. Finish」；
  - `docs/design/overview.md`：「Terms」（record、frame、packet）、「Scope: one-way only」中的續傳；
  - 以上三份的 `.zh-TW.md` 版本。
  - 2026-10-25 的重新決定另外會改：`docs/design/architecture.md` 的「SSH」「Transport」「Connections」、`docs/design/dashboard.md` 的 SSH 連線那一區、`docs/cli.md` 的 `--ssh-connections` 與重新登入，以及它們的 `.zh-TW.md`；CHANGELOG 裡 A2 的幾項；`TODO.md` 的 Reconnecting。
- 相關：
  - [目標架構](../review/2026-10-17/target-architecture.md)：異同見[「和 10-17 目標架構的異同」](#和-10-17-目標架構的異同)；
  - [ARC-04](../review/2026-10-17/architecture.md) 與 D16：分段的 record，決定先不做；
  - `TODO.md` 的「Set aside」：Reconnecting；
  - `docs/design/overview.md` 的兩個 open question：典型的工作負載、rsync 在哪裡痛。

這份草稿從高層重新審視 Ferry 的架構：client 在中間、兩邊各一個 server 的拓撲，資料怎麼流動，可靠性放在哪一層。它允許大幅度的重新設計。結論是：現在限制速度與穩定的，已經不是個別的程式碼，而是幾個互相綁住的結構決定。提案換掉其中兩個（全序、可靠性放在 transport），保留第三個（client 在中間、建立所有連線），並說明怎麼一步步走過去。這不是決定；需要使用者決定的事列在[「待決定的問題」](#待決定的問題)。

**目錄**

- [2026-10-25 重新決定：一條 SSH 連線，斷線就失敗](#2026-10-25-重新決定一條-ssh-連線斷線就失敗)
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
- [改進選項](#改進選項)
- [計畫](#計畫)
- [驗證](#驗證)

---

## 2026-10-25 重新決定：一條 SSH 連線，斷線就失敗

**決定（2026-10-25，使用者）：** 每個 host 只有一條 SSH 連線；它斷了，傳輸就失敗，不重連，和 rsync 一樣。`--ssh-connections` 拿掉。10-23 的兩個核心決定，第二個（端到端確認與重送：stream 斷了不再等於傳輸失敗）撤回；第一個（block）不變。

**為什麼：**

- nano4 的同一個主機名稱後面有好幾台 login node，每一條新的 SSH 連線都可能落在不同的一台（使用者確認）。server 等 stream 加入的 socket 在它那台機器的本機目錄裡（`$XDG_RUNTIME_DIR` 或 `/tmp`），所以第二條以後的連線，以及斷線後重新登入的連線，都可能落在沒有這個 server 的機器上，接不上。之前的設計，包括 A2 之前的 path，一直默認同一個主機名稱就是同一台機器，沒有考慮這件事；fatcat 只有一台，nano4 上又只用過一條連線，所以沒有發現。
- nano4 每一條新的連線都要 OTP，重新登入一定要使用者在場。
- 這些是 SSH 與主機的不確定性，傳輸中途沒辦法處理得可靠。同一條 SSH 連線上的 channel 都走同一條 TCP 連線，一定落在同一台機器，斷了也一起斷，所以只用一條，就排除了它們。

**拿掉的：**

- 多條 SSH 連線：`--ssh-connections`、第一條以外的 master（`%C-1`、`%C-2`……）、stream 依連線編號分配。
- 斷線後的重連：stream 的重建；SSH 連線的重新登入，包括在終端機上問、dashboard 交出終端機；server 做完之後留下、等晚到的 stream；`Report::Broke`。
- session 的端到端可靠性：control 的序號、確認與重送；block 的編號、保留、確認與去重；stream 的重新加入；寬限時間；在多條 stream 之間分配。
- server 裡 peer 與 session 之間的轉接：outward 與 inward 兩條執行緒、兩條 pipe、把 data 封包組回 block 再切成 fragment、`BlocksEnd`。
- 測試用的 `FERRY_TEST_BREAK`；`--cut-a-stream` 的情境改成預期傳輸乾淨地失敗。

**留下的：**

- block（pack 與 segment）、Source 的擁有者、Destination 的決策者、資料額度（A1、B1）。
- client 建立 stream，把兩端交給兩個 server，自己不碰資料；兩個 host 之間，兩個 ssh 背對背（A2 第四步）。複製 1 GiB 時，client 的執行緒少用約半秒 CPU。
- 報告走 server control（A2 第三步）。
- server 的 socket，直連與 `--remote-join` 兩種加入方式，stream 開頭的標頭與開頭標記。
- dashboard 上每個 host 的速率與「heard」，每個 host 一列。

**session 縮成一層薄的 stream 層（2026-10-25，使用者同意）：**

- peer 的封包直接寫上那一條 stream，不再經過轉接與 pipe，也不再切成 fragment、組回 block。
- 閒置 5 秒送 keepalive；stream 結束、出錯，或 20 秒沒有聽到任何東西，就算斷了，傳輸失敗。
- 結束時兩邊都說完了才關，所以正常結束不會被當成斷線。
- 計時器只在這一層與 client 的連線管理；peer 仍然沒有網路計時器。
- 只有一條 stream（2026-10-25，使用者同意）。同一條 SSH 連線上多開 channel（原本的 A3）列在[改進選項](#改進選項)：這些 channel 一定落在同一台、一起斷，不會帶回這次的問題，好處是繞過每條 channel 2 MiB 的視窗，要在長距離的路徑上量過再說。真的要做，這一層也能長出多條 stream：任何一條斷了就整個失敗，所以不需要確認與重送。

**斷線要快，而且乾淨地失敗：**

- server：stream 結束、出錯，或 20 秒沒有聽到東西，peer 就失敗，走現在的停止路徑，Destination 不留半成品。
- client：control 連線結束（`ssh` 以 255 結束），或 20 秒沒有收到任何東西，傳輸就失敗；server 每秒都送 metrics 與 stream 的回報，control 不會正常地安靜那麼久。現在的 client 不看 control 的沉默，只靠 ssh 結束：Ferry 自己的 master 有 `ServerAliveInterval`，約一分鐘後結束；用使用者自己的 ControlMaster、又沒設 ServerAlive 時，斷線後可能要等 TCP 自己放棄，很久才發現。這要補上。（已補上：A2 的收回第四步。）
- server 做完或失敗之後，不無限期地等 control 關閉。

**會失去的：**

- 斷線就要重跑：完成的檔案會跳過，但要重新掃描與比較，傳到一半的大檔從頭來。所以 A4 的跨次續傳（第 10 節）更有價值，那相當於 rsync 的 `--partial`。
- 掉包的路徑上，少了多條 TCP 連線的加速：模擬中 120 ms、0.01% 掉包時，一條 TCP 約 4 MB/s，4 條連線 14–16 MB/s（5.2 節）；一台真的主機上量過，一條約 31 MB/s，四條約 60 MB/s（`docs/design/architecture.md` 的「Connections」）。
- 一條 channel 每個 RTT 最多 2 MiB：模擬中 120 ms 約 17 MB/s，250 ms 約 8 MB/s（5.2 節）。rsync 也受同樣的限制；A3 可以繞過，但主機的 TCP buffer 要夠大才有用。

**撤回或改寫的地方，各處都加了註：**

- 摘要「得到的」第一項；第 1 節的原則 1 與 7；第 2 節的 N 條連線與 K 個 channel；第 3 節的 session 那一層；4.1 的可靠性；第 5 節的 session、5.3 的「server control 也由 session 管」、5.4；第 10 節的「server 的存活」與「同一次執行內的重連」；第 12 節的對應幾列。
- 待決定的問題：「端到端確認與重送」「server 的寬限時間」「重連需要登入時要不要問」三項撤回；「高 RTT 時要不要自動多開 SSH 連線」不再適用；「session 獨立成一層」與「連線的健康檢測集中」改成薄的 stream 層。
- 改進選項：「peer 直接用 session 的介面」「fragment 直接讀進 block 的緩衝區」不再需要；A3 移入。
- 計畫：A2 的重連撤回，新增「A2 的收回」的步驟（[A 線](#a-線資料面與穩定性)）；A3 改列為改進選項。A2 六步的結果與量測照樣留著，當作紀錄。

**文件：** 先改這份草稿；`docs/design/` 照 [README](README.md) 的流程，跟著程式一起改。現在的設計文件、CHANGELOG 與 `TODO.md` 寫的是 A2 做好的樣子（多條連線、重建 stream、重新登入），到 A2 的收回時才改。（已改：A2 的收回第五步。）

---

## 摘要

**三個結構決定，前兩個互相綁住，第三個被它們拖累：**

1. **全序。** manifest、record 串流、每一段 transport 的片段、applier，都要求一個全域的順序。真正需要的只有「同一目錄之內」與「同一檔案之內」的順序。全序帶來一串單執行緒點，以及 head-of-line blocking。
2. **可靠性放在 transport，而且斷了就整體失敗。** transport 保證「完整、有序，否則失敗」，資料又不是可以單獨重送的單位，所以任何一條連線斷掉，整個傳輸就失敗；重連與續傳都很難事後加上去。
3. **client 在中間。** 所有資料都經過 client、都走 SSH，client 的存活就是傳輸的存活。client 建立所有連線是刻意的選擇：兩台主機不必互通，主機上不放憑證。提案保留它；它在中間的負擔（重排、讀每則訊息、server 隨它結束）大多來自前兩個決定。

另外有一個獨立的問題：**在高延遲的檔案系統上，metadata 操作有好幾個單執行緒的 stage**（刪除、mkdir、屬性、收尾、列目錄）。

**提案，一段話：** 把可靠性移到兩端，以可重送、可驗證的 block 為單位，端到端確認；負責的 session 是 transport 的最上層，在兩端的 server 裡，peer 仍然只往介面送。只保留真正需要的順序。session 下面的 stream 會斷、只保證各自的順序；所有連線仍由 client 建立，建好就把兩端交給 server，自己不碰資料；三種組合都是 client 加兩個 server，本機到本機只差選項。流量由 Destination 發的額度決定。每個 peer 改成一個不做 I/O 的擁有者，加上依操作種類並行的執行者；整個傳輸只有 Destination 的擁有者做決策。

**2026-10-25：** 可靠性改回 rsync 的做法：一條 SSH 連線，斷了就失敗；block、額度、擁有者與決策者、client 不碰資料都留著（[重新決定](#2026-10-25-重新決定一條-ssh-連線斷線就失敗)）。

**得到的：**

- 斷線不再等於失敗：沒被確認的 block 改走別條，stream 在背景重建，server 等 client 重新接上。**2026-10-25 撤回：** 斷線就失敗。
- 連線的健康只在一處判斷：server 端是 session（2026-10-25 起是薄的 stream 層），client 端是連線管理。peer 只收「放棄了」與「結束了」兩個事件，沒有網路計時器；heartbeat channel 與它的一整套特例都拿掉。
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
- 每個檔案原子就位、BLAKE3 驗證、`O_TMPFILE`。這裡原本還有批次 fsync 與 macOS 的 barrier，2026-10-24 使用者決定整個拿掉，見 [fsync.md](fsync.md)。
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

本機 ext4 上複製小檔時，瓶頸是 fsync（2026-10-24 已拿掉），不是這些單執行緒點；大檔是例外（下面的 a）。在高延遲的檔案系統或快速的鏈路上，它們會輪流變成瓶頸，而且會互相阻塞：

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

檔案內容是平行的：16 個 writer、64 個 fsync thread（2026-10-24 已拿掉）、4 個 renamer。但下面這些都只有一條 thread：

- **applier：** mkdir、symlink、特殊檔、只改屬性的項目、`sync` 的刪除（遞迴的 `remove_tree`）（`src/destination/apply.rs`）。
- **finisher：** 目錄的 owner、mode、mtime、fsync（fsync 已拿掉；`src/destination/finish.rs:23`）。
- **列目錄：** Source 的 lister 與 Destination 的 compare，各一條（`docs/design/pipeline.md:168-187`）。2026-10-23 量到，一條 lister 在 fatcat 的 NFS 與 nano4 的 WekaFS 上，列一個目錄約 0.5 ms，每個項目再加 10–25 µs，每秒只處理 1.5 萬到 7 萬個項目；本機是幾十萬到兩百多萬（[驗證](#驗證)）。

串行時，吞吐量就是「1 ÷ 一次往返的延遲」。2026-10-23 在 nano4 的 login node 上量到，WekaFS 上一次 unlink 約 0.65 ms、mkdir 約 0.69 ms、改屬性約 0.53 ms；fatcat 的 NFS 上是 0.13、0.14、0.23 ms（[驗證](#驗證)）。所以在 WekaFS 上，一條 thread 刪 100 萬個多餘的檔案約 11 分鐘，其間 applier 不送任何 needs（[R1 的 b](#r1全序造成單執行緒點與-head-of-line-blocking)）；首次複製 10 萬個目錄，光 mkdir 就要一分多鐘；`chmod -R` 之後的同步，每個項目都要改屬性，全都排在一條 thread 上。列目錄也一樣：1,000 萬個項目、10 萬個目錄的樹，光是 Source 列目錄，WekaFS 上就要 2.5 到 3 分鐘，NFS 上沒有快取時要 2.5 到 5 分鐘。

### 三個決定互相綁住

![左：全序，使每一段都要重排成一條，一條慢路徑擋住全部，frame 跨檔案只能一個 parser，擋住多路徑互不阻塞。中：可靠性放在 transport，保證有序、不遺失，否則失敗；資料沒有確認、沒有重送，缺一片就只能整體失敗，server 也隨 client 結束，擋住重連與續傳。右：client 在中間，是刻意的選擇：連線一律由 client 建立，兩台主機不必互通，主機上不放憑證。它的負擔來自前兩個決定：每一段都要有序、可靠，client 才得重排、讀每則訊息。前兩個換掉之後，client 只建連線，不碰資料；資料經過 client 這台機器，是保留它的代價](assets/redesign-interlock.svg)

全序與「可靠性在 transport」互相綁住，只換一個，另一個還是擋住：

- **只在 transport 加確認與重送：** 片段還是要全域重排，HOL 還在；client 還是得重排、讀訊息；大檔還是一條 thread。
- **只做分段的大檔（ARC-04）：** 解決了大檔的平行，但斷線還是失敗。

所以重連與續傳一直排不進去。提案把這兩個一起換掉。**2026-10-25：** 第二個改回原樣：只有一條 SSH 連線，斷了就失敗；全序照樣換掉，block 也照樣做（[重新決定](#2026-10-25-重新決定一條-ssh-連線斷線就失敗)）。

第三個，client 在中間，是刻意的選擇，提案保留它。它的負擔大多是被前兩個拖累的：每一段都要有序、可靠，client 才得重排、讀每則訊息；斷了就是終局，server 才隨 client 結束。前兩個換掉之後，client 建好 stream 就把兩端交給 server，連位元組都不必經手，server 也能在寬限時間內等它回來。剩下的代價，是 host 到 host 的資料仍經過 client 這台機器（[R3](#r3資料面綁在以-client-為中心的拓撲上)）。

---

## 提案

### 1. 原則

新增或改變的原則：

1. **可靠性端到端。** Source 與 Destination 之間，以可重送、可驗證、冪等的單位溝通，並互相確認。transport 與中繼不保證任何東西，只盡力送。**2026-10-25 撤回：** 只有一條 stream，它保證自己的順序，斷了就失敗；block 照樣各自可以驗證（[重新決定](#2026-10-25-重新決定一條-ssh-連線斷線就失敗)）。
2. **只保留真正的相依。** 同一目錄之內、同一檔案之內，以及父子目錄之間；其餘都可以平行。
3. **連線一律由 client 建立；peer 只往介面送。** server 不往外連、不開 port、不放憑證，兩台主機不必互通。client 建好每條 stream，就把兩端交給 server，自己不碰資料；每一種組合都是 client 加兩個 server。
4. **記憶體與在途的量由消費者決定。** Destination 發額度，一處決定。
5. **每一種檔案系統操作的並行度，都是政策，不是 thread 的個數。**
6. **一份狀態，一個擁有者。** 每個 peer 的狀態只由一條不做 I/O 的 thread 改，I/O 都交給執行者。整個傳輸只有 Destination 做政策的決定，Source 只決定怎麼做（[3.1 節](#31-peer-的形狀一個擁有者平行的執行者)）。
7. **連線的健康只在一處判斷。**
   - server 端只由 session 判斷 stream 健不健康；client 端只由連線管理負責建立與重建連線。
   - peer 只收兩個事件：`Lost`（session 放棄了）與 `Closed`（正常結束）。事件放進擁有者的收件匣，不是 callback。
   - 其他元件都不持有網路計時器（[5.3 節](#53-連線的健康只在-session-與-client-的連線管理)）。
   - 2026-10-25 起，server 端判斷的是薄的 stream 層，不再是 session；client 端只建立，不再重建；peer 收到的是 stream 斷了、或正常結束（[重新決定](#2026-10-25-重新決定一條-ssh-連線斷線就失敗)）。

不變的原則：可靠性先於速度；斜線不改變行為；結果與 rsync 相同；一切需要使用者的事在傳輸開始前問完（重連時的登入原本是[待決定的例外](#待決定的問題)；2026-10-25 拿掉重連，這個例外也不存在了）。

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

**2026-10-25：** 每個 host 只有一條 SSH 連線、一條 stream；上表的 N 對 socketpair、N 條連線與 K 個 channel 都變成一條，stream 斷了也不重建（[重新決定](#2026-10-25-重新決定一條-ssh-連線斷線就失敗)）。

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

**2026-10-25：** session 這一層換成薄的 stream 層：peer 的封包直接寫上一條 stream，閒置時送 keepalive，stream 結束、出錯或沉默 20 秒就失敗，結束時兩邊說完才關；不再有序號、確認、重送、重新加入與寬限（[重新決定](#2026-10-25-重新決定一條-ssh-連線斷線就失敗)）。上面兩張表的 session 一列，以及四點中的第 1 與第 3 點，因此不再成立。

#### 3.1 peer 的形狀：一個擁有者，平行的執行者

兩個 peer 用同一個形狀，session 也是：

- **擁有者：** 一條 thread。這個 peer 的狀態都在它手上，也只有它能改。它不做 I/O，只收事件、發命令。
- **執行者：** 依操作種類分開的 pool，做所有的 I/O 與重的計算：列目錄、stat、讀、寫、雜湊、壓縮、名稱操作；session 的執行者是 stream 的收送。做完以事件回報，不自己決定接下來做什麼。

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
| D → S | `Done` | — | 一切都已寫好、取得名稱；絕不在 `Error` 之後 |

**可靠性由 session 提供，peer 看不到：**

- 每個方向各自為控制訊息編號（`seq`）；接收端在它送出的每個 frame 上順便帶累計確認（`ack`）。
- 傳送端把還沒被確認的訊息放在重送緩衝區，以位元組為上限（例如 4 MiB）；滿了，session 就請擁有者先別送，擁有者把 control 留在自己這邊，不等（3.1 節的規則 1）。
- 平常固定走一條 stream，不必重排；那條斷了，就換一條還活著的 stream，從最後確認之後全部重送；接收端丟掉已交付過的序號。

（2026-10-25 撤回：只有一條 stream，順序由它保證，斷了就失敗，沒有序號、確認與重送。）

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

- **pack** 把多個完整的檔案依讀完的順序串在一起，滿 B（2 MiB，量測決定，見下）或閒置一下（例如 2 ms）就封口，就像現在「串流閒置時，frame 不等滿就送」。可以跨目錄。不大於段大小 S 的檔案都整個進 pack；一個 pack 可以只有一個檔案。
- **segment** 是大於 S 的檔案的一段，S 是 2 的冪（2 MiB，同 B），從 S 的倍數開始；最後一段可以比較短。所以每個分段的檔案至少有兩段。
- **`len`、`size`、`mtime` 是開檔時的值**，和現在 record header 的語意相同（`src/destination/write.rs:435`）：讀到開檔時的大小，Destination 以這個 mtime 設定檔案。
- **讀不完整，不必補零。** 現在的 record 補零是為了讓串流對齊；block 自己描述自己，讀不到的檔案在 pack 裡寫成 `Unread`、`len` 為 0。一段讀不完整，整個檔案失敗。
- **編號由 session 給，冪等。** block 本身不帶編號；Source 的 session 收下時給它一個遞增的編號，帶在每個 fragment 上（5.1 節）。Destination 的 session 依編號去重，peer 只看到每個 block 一次。（2026-10-25 撤回：沒有 session，block 不編號；只有一條 stream，也不會重複。）
- **壓縮**以 block 為單位，level 1；壓不下去就原樣送；本機到本機時不壓，同現在（ARC-05）。

**做了之後（A1 的第一步，2026-10-23）：** 格式在 `src/block.rs`，還沒接上傳輸。和上面不同的地方：

- block 前面多 8 個位元組，是 header 與 payload 的長度。A1 的資料仍走一條有序的串流，要靠它切出一個個 block；A2 之後由 session 的 fragment 帶。（2026-10-25：拿掉 session 之後，仍靠這 8 個位元組。）
- pack 與 segment 共用一個 `Check`：`Hash([u8; 32]) | Unread(String)`。在 segment 裡，`Hash` 是那一段的 CV。
- 解碼時，拒絕 `encode` 不會做出的 block：
  - header 與 payload 加起來超過 B + 64 KiB（給讀不到的原因留空間）；
  - pack 沒有檔案、有檔案大於 S、讀不到的檔案帶了位元組，或各檔的長度加起來不等於 payload；
  - segment 不在 S 的倍數、長度不是 S 也不是檔案剩下的，或檔案不大於 S；
  - 壓縮過的 payload 沒有比原樣小，或解出來的長度不對。
- 解碼後，每個檔案或段成為一個 `Part`：雜湊或 CV 相符才給位元組，否則給原因，訊息同現在。段另外帶它的 CV，給 `root` 合併。

**B 與 S 的量測（2026-10-23）：** 同一批檔案，依上面的規則組成 pack 與 segment，B = S，每個 block 各自壓縮，算送出的位元組，header 也算在內。「現在」一欄由 Ferry 自己的 record 與 frame 程式做出。單位 MB：

| 資料 | zstd | 現在（2 MiB 的 frame） | 2 MiB | 8 MiB | 16 MiB |
|---|---|---|---|---|---|
| crate 的原始碼（546 MB，3.6 萬個檔案） | 1 | 88.9 | 88.0 | 87.0 | 86.9 |
| | 3 | 73.0 | 73.0 | 67.6 | 66.8 |
| 建置產物（697 MB，3,213 個檔案） | 1 | 236.0 | 236.1 | 235.1 | 235.0 |
| | 3 | 197.5 | 202.8 | 183.9 | 182.7 |
| `/usr`（8.5 GB，3.7 萬個檔案） | 1 | 4,614 | 4,613 | 4,604 | 4,603 |
| | 3 | 4,373 | 4,372 | 4,307 | 4,296 |
| 原始碼的 tar（584 MB，一個檔案） | 1 | 92.8 | 92.8 | 92.0 | 91.9 |
| | 3 | 79.5 | 79.6 | 76.4 | 75.9 |
| | 9 | 68.6 | 68.6 | 63.8 | 62.9 |

- 8 MiB 比 2 MiB：zstd 1 少 0.2–1.1%，zstd 3 少 1.5–9.3%，zstd 9 少 7%。16 MiB 再少不到 1.4%。
- 另有四個各取 1 GiB 的大檔：模型權重兩個、數值資料、zip。它們壓得很少，各種大小差不到 0.2%。
- 2 MiB 的 block 和現在的 frame 差不多。pack 在下一個檔案放不下時就提早封口，所以建置產物在 zstd 3 時多 2.7%。
- 代價：
  - 記憶體：每條讀或壓縮 block 的 thread 各占一個 block。Source 最壞約 16 × 8 + 8 × 2 × 8 = 256 MiB。
  - 解壓：level 1 時，8 MiB 的 block 多數比 2 MiB 慢 4–33%，每核仍有約 600 MB/s 以上。
  - 64 MiB 的資料額度只放得下 8 個 block。額度又以壓縮後的大小計，Destination 解開後是它的幾倍（原始碼約 ×6）。額度怎麼算，第 4 步要一起看。
- 第一步因此定為 B = S = 8 MiB，同上面原本的例子。量測的程式在工作目錄的 `.scratch/verify/blocksize/`。
- **第二步改為 2 MiB（2026-10-23）。** 接上傳輸之後量本機的速度（tmpfs → tmpfs，不壓縮），舊版與新版輪流：
  - 一個 4 GiB 的檔案：舊版約 3.0 秒；8 MiB 的 block 3.2–3.5 秒；2 MiB 2.8–2.9 秒。8 MiB 時，單執行緒的步驟一次處理 8 MiB，超出 L2 cache，也讓管線比較顛簸。
  - 20 萬個 4 KiB 的檔案：各 6 輪，中位數舊版 3.04 秒，8 MiB 3.01 秒但最慢兩輪 3.4、3.7 秒，2 MiB 2.86 秒。
  - 壓縮率的差別只在 zstd 3 以上明顯，預設的 zstd 1 只差 0.2–1.1%，本機預設又不壓縮；記憶體與每份額度能放的 block 數，2 MiB 也都好四倍。
  - 所以 B = S = 2 MiB：壓縮率等於之前的 frame，本機不比之前慢。要更高的壓縮率，日後可以讓壓縮時的 block 比較大；S 要兩邊一致，所以得放進 hello。

#### 4.3 整檔雜湊不必放棄

![一個 27 MiB 的檔案切成四段：0–8、8–16、16–24、24–27 MiB。每段由一條 thread 以 set_input_offset 設好位置，算出非 root 的 chaining value：CV0 到 CV3。CV0 與 CV1 合併成 P01，CV2 與 CV3 合併成 P23，都用 merge_subtrees_non_root；最後 merge_subtrees_root 合併 P01 與 P23，得到的正是 blake3::hash(整個檔案)](assets/redesign-hash.svg)

BLAKE3 是樹狀的雜湊。對齊、大小為 2 的冪的段，可以各自算出子樹的 chaining value（CV），再依 BLAKE3 樹的形狀兩兩合併，得到和一次讀完完全相同的 root hash。`blake3` 1.8.7 的 `hazmat` 模組正好提供這些：`HasherExt::set_input_offset`、`HasherExt::finalize_non_root`、`merge_subtrees_non_root`、`merge_subtrees_root`，以及決定樹形狀的 `left_subtree_len`。2026-10-23 寫程式驗證過：5 種段大小、156 種長度，包括最後一段只有 17 位元組的情形，各段在不同的 thread 算 CV，合併出的 root 都等於 `blake3::hash`；改一個位元組，root 就不同。

- Source：每段由讀它的 reader 算 CV，放進 segment；擁有者收齊各段的 CV 後合併出 root，以 `FileHash` 送出。合併很便宜：n 段只要合併 n−1 次，每次只處理 64 個位元組，擁有者自己做就好。
- Destination：每段到了，先對收到的位元組以同一個 offset 重算 CV，與 segment 帶的比較，不符就讓整個檔案失敗，不寫入；全部到齊，再把 CV 合併出 root，與 `FileHash` 比較，相符才繼續設屬性、link。
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

**2026-10-25：** 這一節的 session 撤回：只有一條 stream，沒有序號、確認、重送、重新加入與寬限，也不再切成 fragment。留下的是 frame 裡的 `Keepalive` 與「正常結束不當成斷線」、5.2 的量測，以及 5.3 健康判斷的做法，但 server 端改由薄的 stream 層判斷。見[重新決定](#2026-10-25-重新決定一條-ssh-連線斷線就失敗)。

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

**server control 也由 session 管。** client 與 server 之間的控制連線，用同樣的 keepalive，斷了也能重新加入。（2026-10-25 撤回：server 照舊隨 control 連線結束；client 看 control 的沉默，20 秒沒收到東西就失敗。）所以：
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

（2026-10-25 撤回：斷線就失敗，見[重新決定](#2026-10-25-重新決定一條-ssh-連線斷線就失敗)。）

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
- **大檔：** reader 開檔、`fstat` 後發現大於 S，回報大小與 mtime，檔案以 `Arc<File>` 共用；擁有者切成 ⌈大小 ÷ S⌉ 段，任何 reader 都可以用 `pread` 讀其中一段、算它的 CV；每段讀完就去壓縮、送出，不進 pack；各段的 CV 都到了，擁有者合併出 root，送 `FileHash`（4.3 節）。（A1 做了之後：同一個檔案一次只讀一段、照順序，大檔一個接一個，因為硬碟上平行讀段只有六成的速度；見 A1 的「第六步之後」。NFS 上反而是平行讀快，見[改進選項](#改進選項)。）

**決定什麼、不決定什麼：** 擁有者只決定怎麼做：pack 何時封口、大檔的段與小檔怎麼交錯讀；block 走哪條 stream，由 session 決定。傳哪些檔案、什麼順序、傳多少，來自 Destination 的 Want 與額度；K、`-x`、avoid 由 client 在 prepare 時決定。讀不到的檔案以 `Problem` 回報，讀不完整的在 block 裡標成 `Unread`，後果都由 Destination 決定。

**完成與停止：**

- 完成是狀態上的一個條件：`ManifestEnd` 已交給 session、每個 Want 的檔案都交出或回報了；之後等 Destination 的 `Done`，它表示一切都已寫好、取得名稱，自然也都收到了。不再靠幾條 thread 依序關掉 channel。
- 停止照舊：第一個錯誤，全體停止（`Stop`）。擁有者收到 Destination 的 `Error`，或認定自己的錯誤要停止傳輸時，由它觸發；session 在寬限時間後仍接不回來，也以失敗交給擁有者。

**掃描可以晚一步。** K=1 時，掃描可以先維持現在的樣子：照順序給 ID、組成 page（同時以筆數與位元組為上限，同現在的 `PART`、`PART_BYTES`），交給擁有者；擁有者沒有 manifest 額度時就先不收，掃描自然停下。B2 要 K>1 時，給 ID 與待列的目錄才移進擁有者，lister 與 stat 也才變成純粹的執行者。

### 8. Destination：一個決策者，平行的執行者

![提案中的 Destination：中間是決策者，一條 thread，不做任何 I/O，只保存目錄與檔案的狀態並做決定。它收到的事件：manifest page 到了、比較完成、block 驗證好了、操作完成、FileHash 與 Unlistable。它發出的命令：交給比較的 pool 列目錄與 stat，交給執行者做名稱操作、寫內容與屬性、目錄收尾，並經 session 送出 Want、Credit、Results。下方：現在 applier 一條 thread 刪 100 萬個檔案，期間沒有任何 Want；提案中不同目錄的刪除由 K 條平行做，Want 照常送出](assets/redesign-destination.svg)

**保留 ARC-03 的單一擁有者，拿掉單一執行者。** 形狀同 [3.1 節](#31-peer-的形狀一個擁有者平行的執行者)；不同的是，這個擁有者也是整個傳輸唯一的決策者。

- **決策者**（coordinator）是一條 thread，所有目錄與檔案的狀態都只在它手上，不用鎖。它不做任何 I/O：收事件、發命令。
- **執行者**是依操作種類分開的 pool：

| pool | 做什麼 | 並行度 |
|---|---|---|
| 比較 | 列 Destination 的目錄、merge、stat | 8，同現在 |
| 解碼 | 解壓、驗證 pack 內每個檔案的雜湊或 segment 的 CV | 8 |
| 名稱操作 | 刪除（`remove_tree` 依子目錄拆開）、mkdir、symlink、特殊檔、link、rename | K；同一個目錄一次一個 |
| 內容 | 寫入沒有名稱的暫存檔（大檔以 `pwrite` 寫各段）、owner、mode、mtime | 16 |
| 收尾 | 目錄的 owner、mode、mtime | K |

- **名稱操作的平行來自不同的目錄。** 同一個目錄裡的名稱操作，kernel 以那個目錄的鎖一個一個做。2026-10-23 量到（[驗證](#驗證)）：16 條 thread 各自在不同的目錄，WekaFS 上各種名稱操作快 ×14 到 ×16，NFS 上 ×3 到 ×10；16 條 thread 擠在同一個目錄，WekaFS 上只快 ×1.2 到 ×1.3，NFS 上沒有變快。所以一個目錄裡就有 100 萬個多餘的檔案時，刪除還是那麼慢；改變的是它不再擋住別的目錄與資料面（下面的閘門）。
- **錯誤的分類收進決策者。** 現在一個檔案寫入失敗時，write stage 自己判斷要不要停止傳輸（`ends_the_transfer`），自己送 `Results::Failed`、加共用的計數（`src/destination/write.rs:396-411`）；applier 只收到 `Settled`。提案中，執行者只回報結果，由決策者決定這是單一項目失敗、警告，還是停止。
- **決策者寫成不做 I/O 的狀態機**（sans-IO）。目錄的生命週期、刪除與建立的順序、失敗的分類，都能用合成的事件序列與模擬的檔案系統測試，比整合測試更能涵蓋到達順序與競態。Source 的擁有者也這樣寫，兩者就能在一個 process 裡對接，以固定的種子模擬（A2）。
- 日後 Linux 可以把某些 pool 換成 io_uring 的後端，決策者不用改。

**一個目錄與它的檔案：**

![上排是目錄的狀態：已宣告、已比較（送出這個目錄的 Want）、刪多餘（平行）、閘門「多餘已刪」、建立與修屬性（平行）、收尾、完成並通知父目錄；收尾的條件是子目錄都完成、檔案都定案、最後一段 page 已到。下排是一個檔案：block 到了，寫入沒有名稱的暫存檔、驗證、設屬性，這些都可以在刪除完成前做；只有 link 要等閘門，link 之後定案，計入目錄的收尾](assets/redesign-directory.svg)

- **比較完就送 Want，不等這個目錄的刪除。** 檔案寫進沒有名稱的暫存檔，不碰目錄的名稱空間；只有最後的 link 要等「多餘已刪」這道閘門。大量的刪除因此只擋這個目錄的 link，不擋資料面，也不擋別的目錄。（2026-10-24 量到：NFS 沒有 O_TMPFILE，檔案以暫存名稱寫在同一個目錄，和刪除搶目錄的鎖，提早寫反而慢一成三；所以只在檔案寫成沒有名稱的地方這樣做，見 [B1 的補完](#b-線metadata-的並行)。）
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

**2026-10-25：** 「server 的存活」與「同一次執行內的重連」撤回，圖中提案的那一半也是：server 照舊隨 control 連線結束，斷線就失敗。「跨次執行的續傳」保留，而且因為斷線就要重跑，更有價值（A4）。見[重新決定](#2026-10-25-重新決定一條-ssh-連線斷線就失敗)。

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
| 預設持久（2026-10-24 拿掉，overview 已不列；見 [fsync.md](fsync.md)） | 不 fsync；`--fsync` 給了就拒絕 | 同 |
| 讀到開檔時的大小 | record header 的 size 與 mtime | block 的 `len`、`size`、`mtime`；重送的是保留的同一份位元組，不重讀 |
| 不跟隨 symlink、不出根目錄 | `openat`、`O_NOFOLLOW`、`one_name`、manifest 檢查 | 同；fsx 的 API 不變；manifest 檢查改寫，照樣 fuzz |
| `sync` 不刪讀不到的 | `Unreadable`；列不出的目錄不刪其中的東西 | 同；`Unlistable` 讓列不出變成明確的訊息 |
| 先刪多餘，再建立 | applier 在套用前刪 | 每個目錄的閘門；只有 link 要等 |
| 目錄最後收尾、由深到淺 | stack 加 finisher | 由下往上數 |
| Source 的根不進、不刪 | `Guard` 的 avoid 與 protect | 同 |
| 一切需要使用者的事在開始前問完 | prepare 狀態機 | 同；重連需要登入時，是否詢問待決定 |
| 第一個錯誤，全體停止 | `Stop`、`Stages` | 同；錯誤由擁有者分類；stream 斷了不再是錯誤，而是重送的事件；session 放棄時的 `Lost` 也走同一條停止路徑 |

2026-10-25：表中提到重送、重連，以及「stream 斷了不再是錯誤」的部分撤回：stream 斷了就是錯誤，走同一條停止路徑；沒有重送，Destination 收到的就是 Source 第一次送出的那一份。

---

## 其他做法

- **多條 SSH 連線，加上確認落在同一台機器。** server 在握手時說出它在哪台機器（例如每次開機隨機產生的 ID），每條新的連線先確認落在同一台；不是同一台，就重新登入，或從那一台經叢集內部轉回 server 那台。nano4 每條新的連線都要 OTP，重新登入直到落回同一台不實際；經叢集內部轉回去，要 login node 之間免密碼互連，又多一層。2026-10-25 決定不做：每個 host 只有一條 SSH 連線（[重新決定](#2026-10-25-重新決定一條-ssh-連線斷線就失敗)）。
- **只在每一段 transport 加確認與重送（hop-by-hop）。** 在現在 striped 連線的片段上加累計確認、重送緩衝與重新加入，再加 server 的寬限時間。這樣可以重連，而且不必動資料格式。但 HOL、單一 parser、大檔一條 thread、跨次續傳都還在；之後改成 block 時，這一層會變成多餘。只想先要重連時，這是過渡的選項，見[計畫](#不想一次動這麼多時)。（2026-10-25 起不重連，不適用。）
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
| 控制面與資料面分開；block（pack、segment）；額度；重連 | 有 | 同意。但把可靠性明確放在兩端的 session，中間的 stream 什麼都不保證，於是 client 只建連線，不碰資料。2026-10-25 起不重連：只有一條 stream，斷了就失敗；client 照樣不碰資料 |
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

- [x] **端到端確認與重送；stream 斷了不再等於傳輸失敗。** 牴觸 `docs/design/architecture.md:227-236`。2026-10-23：使用者決定做。**2026-10-25 撤回：** 一條 SSH 連線，斷線就失敗（[重新決定](#2026-10-25-重新決定一條-ssh-連線斷線就失敗)）。
- [x] **資料改成 block：pack 與 segment（重議 D16）。** 2026-10-23：使用者決定做。D16 當時只看單一檔案的吞吐量；這次的理由是可以單獨重送、不互相阻塞、續傳不必再改格式。
- [x] **session 嵌在各自的擁有者裡，還是獨立成一層。** 2026-10-23：獨立成一層，是 transport 的最上層，在兩端的 server 裡，有自己的擁有者；peer 只往它的介面送（第 3 節）。原本建議嵌入，理由是 Ack 與額度要和 `Known` 一起改；逐一對過之後，沒有任何事件需要同時改兩邊。2026-10-25：session 縮成薄的 stream 層，仍然獨立於 peer。
- [x] **連線的健康檢測集中。** 2026-10-23：server 端只由 session 判斷，client 端只由連線管理建立與重建；peer 只收 `Lost` 與 `Closed`；其他元件不持有網路計時器（第 1 節、5.3 節）。2026-10-25：server 端改由薄的 stream 層判斷；client 端只建立，不重建，另外看 control 的沉默。
- [x] **server 的寬限時間多長；要不要可以設定。** 2026-10-23 已決定：server 的壽命改成跟著 session，所有 stream（包括 server control）都斷了才開始計寬限，不再是控制連線斷了就結束。這牴觸「The server lives as long as its control connection」。建議 5 分鐘。2026-10-24 使用者決定：1 分鐘。**2026-10-25 撤回：** server 照舊隨 control 連線結束，沒有寬限。
- [x] **本機的 peer 在 client 的 process 內。** 2026-10-23：不做；client 把 stream 的兩端交給 server，本機到本機只差選項（第 2、11 節）。理由見[其他做法](#其他做法)。
- [x] **直連。** 2026-10-23：不做；連線一律由 client 建立（第 2 節）。理由見[其他做法](#其他做法)。
- [ ] **manifest 以目錄為 page；K 的預設值；K>1 時 dry run 的順序。** 2026-10-23 以合成的樹量了（第 9 節）：目錄小而多時值得。NFS 上 K=4 就拿到大部分的好處，請求率約加倍；WekaFS 上 K=16 還在變快。由使用者決定。
- [x] **重連需要登入時，有終端機要不要問。** 這是「傳輸開始後不再問」的例外；沒有終端機時一律不問，以剩下的 stream 繼續，全部斷光就在寬限後失敗。這會真的發生：nano4 每一條新的 SSH 連線都要 OTP（2026-10-23 確認），master 還在時開新的 channel 不必登入，master 斷了重連就一定要使用者。2026-10-24 使用者決定：要問。**2026-10-25 撤回：** 不重連。
- [x] **高 RTT 時，要不要自動多開 SSH 連線。** 2026-10-25：不再適用，每個 host 只有一條 SSH 連線；同一條連線上多開 channel 列在[改進選項](#改進選項)。原本的內容：
  - 背景：主機用預設的 TCP buffer 時，或路上有掉包時，只有多條 TCP 連線才會變快（模擬，5.2 節）。但每條連線都要登入，現在要使用者自己下 `--ssh-connections`，預設是 1。
  - 建議：client 依握手時量到的 RTT 決定要幾條，只自動開「不用輸入就登得進去」的那幾條（以 `BatchMode=yes` 試）。登不進去的，例如要 OTP 的 nano4，就留在原本的 master 上多開 channel，並告訴使用者多開連線能快多少。
  - 使用者決定。
- [ ] **大檔續傳：有名稱的暫存檔、門檻、續傳時要不要從磁碟重算已驗證段的 CV。** 使用者決定，在 A4 之前。2026-10-25：斷線就要重跑之後，這一項更有價值。
- [ ] **同一個檔案系統時讓 kernel 複製，與驗證的方式。** 只有本機到本機能用（第 11 節）。建議預設不開。使用者決定。
- [ ] **block 大小 B、段大小 S、pack 的閒置時間；資料額度的初始值、上限、要不要自動調整。** 量測決定。2026-10-23：B 與 S 先依壓縮率定為 8 MiB，接上傳輸後量了本機的速度，改為 2 MiB（4.2 節）；pack 的閒置時間與資料額度還要量。
- [x] **名稱操作的並行度 K。** 2026-10-23 量測：WekaFS 上 16 條幾乎線性（×14–16），NFS 上 ×3–10，同一個目錄都不會變快。建議預設 16，同一個目錄一次一個；可以設定。

---

## 改進選項

量到在某些情況下會更快、但現在不做的改動。每一項寫下怎麼做、量到什麼、還沒量什麼，以及決定到哪裡。

- **依檔案系統決定，同一個大檔同時讀幾段。**
  - 現在（A1 的[第六步之後](#a-線資料面與穩定性)）：每個大檔一次只讀一段、照順序；2026-10-24 起不論大小同時讀 16 個檔案，大檔不再一個接一個（見那裡）。從硬碟同時讀一個檔案的好幾段，kernel 的預讀就失效：fatcat 的硬碟上，16 條 thread 同時讀只有一條的六成，A1 讀 4 GiB 的單檔因此慢了約七成。
  - 但 NFS 正好相反：每次讀都要等 server，同時讀好幾段才填得滿網路。fatcat 的 NFS（NFSv4.1，`nconnect=8`）上，同一個檔案的 2 MiB 段由 1、2、4、8、16 條 thread 同時讀，各 2 輪：691–793、646–838、881–1,104、998–1,013、1,065–1,077 MB/s。複製 4 GiB 的單檔到 RAM，A1 平行讀段 3.74–3.99 秒，照順序讀 6.30–6.62 秒，慢約四成，和 A1 之前（6.13–6.17 秒）差不多。本機的 SSD（RAID0）上，幾條同時讀都約 855 MB/s。
  - WekaFS 又不一樣（nano4 的 login node，負載 200–500）：同一個檔案的段由 1、2、4 條 thread 同時讀，3,873–3,981、5,322–6,414、7,324–9,562 MB/s；8 條 2,417–9,162 MB/s，忽快忽慢；16 條只有 822–1,006 MB/s，比一條還慢，各自開 descriptor 1,378–1,435，每段先 WILLNEED 1,793–1,896 MB/s。16 個不同的檔案同時讀卻有 11.5–13.6 GB/s，所以慢在同一個檔案，不在 thread 數。A1 讀 4 GiB 的單檔到 RAM 也不比照順序讀快（6.13–6.35 對 5.85–6.16 秒）：ferry 在那裡約 0.7 GB/s，限制在別處，一段一段讀的 4 GB/s 遠遠夠用。
  - 做法：傳輸開始時，Source 以 `statfs` 查一次根所在的檔案系統，Linux 看 `f_type` 的 magic number，macOS 看 `f_fstypename`。網路檔案系統（NFS、SMB/CIFS、Lustre、GPFS、CephFS、BeeGFS、WekaFS 等）同一個大檔同時讀幾段，各有上限：依上面的數字，NFS 8 段，WekaFS 4 段；本機的，或認不出來的，一次一段，和現在一樣。WekaFS 的 magic 是 `0x18031977`，`stat` 認不出名稱，清單要自己加。
  - 代價與限制：要維護一份檔案系統的清單與各自的上限；清單上沒有的照現在的方式讀，最差就是現在的速度。根底下掛載的其他檔案系統，會被當成和根一樣。上限定錯了反而變慢，如 WekaFS 的 16 段。現在做，WekaFS 上不會變快，只有 NFS 會。
  - 另一個做法：不看檔案系統，一律同時讀兩個不同的大檔，各自照順序。依 A1「第六步之後」的小程式，NFS 快約四成（815 對 1,118 MB/s），WekaFS 約兩倍，SSD 不變，硬碟慢約 5%（安靜的那輪 231 對 220 MB/s）。不必維護清單，但只有一個大檔時沒有幫助。2026-10-24 以「不論大小同時讀 16 個」做了，見 A1「第六步之後」；剩下的只有同一個大檔同時讀幾段。
  - 還沒量：同時讀好幾段時，旁邊的小檔分到的額度少多少、晚多少；對共用的 server 同時多發的請求；其他網路檔案系統。
  - 2026-10-24 使用者決定：先保留一次一段，這一項列為改進選項。
- **單一大檔更快（2026-10-25：本機已做到，WekaFS 上沒有追）。**
  - 現在（A2 的收回之後）：tmpfs → tmpfs 的 4 GiB 單檔 1.6–2.0 秒，分成約 1.6 秒與約 2.0 秒兩群，每一版最快都在 1.57–1.61 秒；和 A2 之前的 `d589cf6` 輪流，中位數 3.00 → 1.92 秒（各 8 輪），比 A1 之前（2.93–3.08 秒）快約三分之一。見 [A2 的收回](#a-線資料面與穩定性)的驗收。
  - 原本：tmpfs → tmpfs 的 4 GiB 單檔 2.85–2.98 秒，和 A1 之前一樣；A1 驗收的第一項（吞吐量隨 writer 數上升）沒有達到，見 [A1](#a-線資料面與穩定性)。
  - 量到的限制在 transport 的轉送：約 6.76 萬個 64 KiB 的封包，經過 Source、client、Destination 約六條執行緒，每個封包約 44–49 µs；Source 的 pump 與 Destination 讀連線的執行緒，大部分時間都在等連線。writer 不是限制：A1 之前一個 writer 寫整個檔案，忙 2.37 秒（全程 2.99 秒）；A1 把各段分給 16 個 writer，合計 1.46 秒。2026-10-24 起一個檔案同一時間只由一個 writer 寫（WekaFS 上 16 個比一個慢四成），所以單檔不會再隨 writer 數變快，能變快的是轉送。
  - 做法：A2 的第四步拿掉 client 的轉送，資料不再經過 client，在 A2 的驗收再量。若還是不快，再看每個封包的成本：封包的大小（64 KiB）、Source 與 Destination 各自經過的執行緒、A1 多出的約 1.3 秒 sys 時間（沒有追）。
  - 2026-10-25 A2 驗收時量：中位數 2.91 → 2.64 秒，快了約一成；限制改在 Destination 的 session 與 server 裡的轉接。這兩者在 A2 的收回時拿掉（[重新決定](#2026-10-25-重新決定一條-ssh-連線斷線就失敗)），收回之後量到的見第一點。
  - WekaFS 上也慢：一條 thread 冷讀就有 3.9–4.0 GB/s，複製 4 GiB 的單檔到 RAM 卻要 6 秒左右；是不是同一個原因，沒有追，A2 的收回之後也沒有再量。
  - 2026-10-25 使用者決定：列為改進選項。同一天 A2 的收回之後，本機已經做到；這一項剩下的只有 WekaFS 上的那一點。
- **排在前面的大檔，不讓小檔等。**
  - 現在（2026-10-24 使用者決定）：不分大小、一視同仁，所有需要的檔案照接收的順序排成一列，16 個 reader 同時讀 16 個檔案，額度先到先得；大檔同一時間只有一段在讀，見 A1 的[第六步之後](#a-線資料面與穩定性)。大檔改成照順序讀之後，A1 驗收的第三項（大檔不再擋住小檔）在 tmpfs 與硬碟上大致做到。
  - 但排在前面的大檔仍先佔住 reader：fatcat 的 NFS 上，16 個 128 MiB 的檔案排在 2 萬個 4 KiB 的檔案之前，和大檔一次一個時相比，整棵樹由 3.45 秒變 2.37 秒，小檔一半寫好卻由 0.5 秒變 2.2 秒。WekaFS 上，大檔一次一個時，小檔也大多排在大檔之後，一半要到全程的四分之三；同時讀 16 個之後沒有量。
  - 做法：有小檔在等時，大檔最多佔一半的 reader，就像 A1 第三步的「段最多佔一半」（2026-10-24 照順序讀時拿掉）。按位元組平分額度是另一個做法，2026-10-24 使用者決定不做。
  - 代價：多一條分大小的規則，大檔晚一點；硬碟上大小檔交錯得更多，磁頭來回也更多。只有大檔、或只有小檔時沒有差別。
  - 還沒量：WekaFS 上同時讀 16 個時的情形；留給小檔的 reader 讓大檔慢多少。
  - 2026-10-25 使用者決定：列為改進選項。
- **peer 直接用 session 的介面（2026-10-25：不再需要）。** 同一天決定拿掉 session 與轉接（[重新決定](#2026-10-25-重新決定一條-ssh-連線斷線就失敗)），peer 的封包直接寫上 stream，就是 A2 的收回的一步。原本的內容留作紀錄：
  - 現在（A2 第四步）：peer 沒有改，server 裡兩條轉接執行緒在 peer 與 session 之間，經兩條 1 MiB 的 pipe：送出時把 data 封包組回 block，收到時把 block 切回 data 封包。每個 block 因此多複製兩次，多經過兩條執行緒。
  - 量到的：4 GiB 單檔時，Destination 把 block 寫給 peer 的轉接執行緒有 2.39 秒在等 pipe（全程 2.72 秒），Source 組 block 的轉接執行緒忙 0.61 秒。
  - 做法：草稿原本的樣子，peer 的 pump 直接把 control 與 block 交給 session，Destination 的接收直接拿 session 交來的 block；pipe、組回 block 與結束標記都拿掉。Source 與 Destination 的收送都要改。
  - 還沒量：拿掉之後快多少。
  - 2026-10-25 A2 驗收時列入，還沒決定。
- **fragment 直接讀進 block 的緩衝區（2026-10-25：不再需要）。** session 拿掉之後，沒有 fragment 要組回。原本的內容留作紀錄：
  - 現在：stream 的讀取者把每個 fragment 讀進自己的緩衝區，交給 session，session 再複製到 block 裡。block 從空的開始長大，2 MiB 的 block 要重新配置好幾次，複製的量因此約是兩倍。
  - 量到的：4 GiB 單檔時，Destination 的 session 執行緒忙 1.47 秒（全程 2.72 秒），應該大多在這裡，沒有再細分。
  - 做法：fragment 的標頭先讀，讀取者依 block 的編號與位置，把內容直接讀進那個 block 的緩衝區；session 只記帳。要讓讀取者拿得到 block 的緩衝區，session 不再是唯一碰它的。
  - 先做得到的小改：第一個 fragment 來時，就照 block 的上限配置，省掉長大時的重新配置；frame 裡沒有 block 的大小，只能照上限。
  - 還沒量。
  - 2026-10-25 A2 驗收時列入，還沒決定。
- **同一條 SSH 連線上多開 channel（原本的 A3）。**
  - 現在（2026-10-25 起）：每個 host 一條 SSH 連線、一條 channel，每個 RTT 最多送 2 MiB（OpenSSH 的 channel window）：模擬中 120 ms 約 17 MB/s，250 ms 約 8 MB/s（5.2 節）。rsync 也一樣。
  - 量到的：fatcat 與 nano4 這種短路徑上沒有好處；高 RTT 時，主機用預設的 TCP buffer 最多快 ×1.35，buffer 調大時接近 ×K（5.2 節）。掉包的路徑上沒有幫助，那要多條 TCP 連線，2026-10-25 決定不做。
  - 為什麼不會帶回 nano4 的問題：這些 channel 都走同一個 master、同一條 TCP 連線，一定落在同一台機器，斷了也一起斷。
  - 做法：薄的 stream 層長出 K 條 stream：block 交給佇列最短的一條，control 固定走一條；任何一條斷了就整個失敗，所以不需要確認與重送。K 依握手時量到的 RTT 決定，例如每 20 ms 一條、最多 8 條；短路徑上 K=1。
  - 還沒量：真實的長距離鏈路。
  - 2026-10-25 使用者決定：列為改進選項，先只用一條。
  - 2026-10-26 使用者決定：和無聲斷線的處理一起做，計畫在 [silent-drops.md](silent-drops.md)；那裡另外記下 block 的順序：Destination 現在假設 block 照送出的順序到，K 條時要先處理。
- **小檔成批交給寫入者。**
  - 現在：Destination 的決策者把每個要寫的小檔當成一件工作交給寫入者，每個寫入者手上最多兩件（`most: 2 * writers`），每交一件就可能叫醒一個寫入者；寫入者寫完一個檔案回報一次。第 3 節說事件成批「不是最佳化，是規則」，交工作這裡沒有照做：B1 的補完把 writing 執行緒的做法原樣搬進了決策者。
  - 量到的：A2 第二步時（還有 session），20 萬個 4 KiB 的檔案，tmpfs → tmpfs，決策者忙 0.94–1.33 秒，其中交工作（20 萬次 send）0.50–0.82 秒，每次 2.5–4.1 µs（含被擠掉的時間），寫入者約八成五的時間在等。A2 的收回之後（`77a3af8`，同樣的樹，釘在 NUMA node 0，3 輪 1.50–1.70 秒，`--profile-file`）：決策者忙 0.82 秒、等工作 0.65 秒；寫入者每個檔案 22.7 µs，16 條合計約兩成時間在忙。所以現在它不是瓶頸，加快它換不到時間；限制在下一項。
  - 做法：一件工作可以裝多個小檔，上限例如 16 個或 1 MiB；只拿已經到的檔案，不為了湊滿而等；等著的檔案不多、又有寫入者空著時，照舊一件一個，不犧牲平行。寫入者寫完一批，一次回報各檔案的結果，每個檔案的成敗照舊分開算。大檔的段不變。
  - 代價：決策者多一些邏輯與測試（湊批、以批計算的上限、一批做到一半失敗）；一個檔案的結果要等同一批的其他檔案寫完才回報。
  - 還沒量：成批之後端到端快多少；前面的環節變快之後，決策者會不會變成限制。
  - 2026-10-25 使用者決定：列為改進選項，等到要做時再量。
- **小檔的 pack 裝滿再封口。**
  - 現在：Source 的擁有者只要事件佇列一時空了，就把手上的 pack 封口送出（`src/source/mod.rs` 的 `drive`）。設計文件寫的是「眼下沒有東西可放就封口」，讓一輪最後的檔案與進度不必等；草稿原本寫的是「閒置一下，例如 2 ms」。reader 讀一個 4 KiB 的檔案約 5 µs，佇列常常一下就空，所以 pack 很小。
  - 量到的（`77a3af8`）：
    - 20 萬個 4 KiB 的檔案，tmpfs → tmpfs：打成 47,735 個 block，平均一個 pack 4.2 個檔案、約 17 KiB，之後每個環節都按 block 付一次固定成本。Destination 的 `receive` 這一條執行緒從頭到尾沒有閒著：讀連線 0.79 秒（83,526 次，每次約 9.6 KiB），把 block 交給解壓縮的池子 0.54 秒（47,735 次，每次 11.4 µs），解析 0.13 秒；Source 的 pump 寫連線 1.07 秒（58,155 次）。
    - 每個 block 單獨壓縮，所以小的 pack 也損失壓縮。`~/.cargo/registry/src`（36,106 個檔案，521 MiB）在本機以 `--compress-level 1`（SSH 的預設）複製，打成 7,000–8,000 個 pack（平均約 70 KiB），省下 82%。以同版本的 zstd 估算，依檔名排序、按大小分組各自壓縮：72 KiB 一包省 81.6%，和實測對得上；2 MiB 一包省 84.2%，送出的位元組由 100.8 MB 降到 86.2 MB，少約 14.5%。估算的小程式在 `.scratch/packsize/`，沒有進 git。
  - 做法：pack 沒滿時，只在下游閒著時封口，也就是沒有 block 在等壓縮，也沒有 block 在等額度；下游在忙就繼續裝，到 2 MiB 為止。下游閒著時照舊馬上封口，所以一輪最後的檔案與進度不必等。block 數、按 block 計的成本與壓縮的損失一起下降。
  - 代價：封口多一條看下游的規則；下游在忙時，裝著的 pack 要等下游空出來才送，這時它本來也送不出去。
  - 還沒量：做了之後，本機 20 萬個小檔的時間與 block 數；registry 原始碼在 zstd 1 時實際送出的位元組；經 SSH 到 fatcat 的時間；大小混合的樹裡，小檔一半寫好的時間。
  - 2026-10-25 使用者決定：列為改進選項，等到要做時再量。
- **Destination 的大檔，同一時間只 fsync 一個。** 2026-10-24 量到後列入，還沒決定，同一天 fsync 就整個拿掉了；原文與量測搬到 [fsync.md](fsync.md)，留給以後加回 fsync 時參考。
- **限制 SSH 連線裡在途的資料量，讓控制訊息不必久等。**
  - 現在：Source 寫上 stream 的資料，只受 data 額度（64 MiB，Destination 寫完 block 才還）與每條 channel 2 MiB 的 window 限制。寫進去的資料排在 SSH 連線那條 TCP 的送出佇列裡，同一條 TCP 先進先出，之後才寫的心跳、「停」、host 的回報、外部觀點的回覆，都排在它們後面。誤判斷線已經由「從對方來的任何東西都算」解決（[silent-drops.md](silent-drops.md) 的第 7 步之後），剩下的是這些控制訊息晚到。
  - 量到的（2026-10-26，TUN 模擬，K=1，`FERRY_TEST_BEAT_LOG`）：乾淨的連線上，心跳單程延遲的中位數就是 RTT/2，最慢約 1.7 個 RTT。RTT 250 ms、掉包 1% 時，速率約 0.1 MB/s，心跳延遲中位數 6.2 秒、最慢 9.2 秒；連線途中掉過包、擁塞視窗被壓小時，120 ms 上傳最慢 3.8 秒（TCP 送出佇列一直滿著約 2 MiB）。延遲約等於「排在前面的資料 ÷ 當下速率」，不受 RTT 限制；K 條 channel 時前面可排的量是 K 倍。所以慢而掉包的上傳中，Ctrl-C 的「停」要好幾秒才到 host（使用者看得到在等它，可以再按一次強制）。
  - 做法：在 peer 之間加一則「收到回條」，和額度同一層，transport 不變。Destination 讀 stream 的執行緒每讀到約 256 KiB，就送一則「已收到第 N 個位元組」，排在資料前面；Source 的 pump 記下每個位元組何時寫出，由回條得到送達速率與最小來回時間。寫 data 或 manifest 的封包之前，若「已寫出 − 已收到」超過窗口 W，就等回條；訊息類不受限。W ＝ 送達速率 ×（最小 RTT ＋ 1 秒），下限 256 KiB（四個 64 KiB 的 data 封包）。窗口限住速度時，速率約為 W/RTT，W 每個來回放大（1 ＋ 1 秒/RTT）倍，很快長到鏈路的上限；鏈路是瓶頸時，佇列裡只排約 1 秒的量，控制訊息最多晚約「1 秒 ＋ RTT」，鏈路慢到 256 KiB 要超過 1 秒時，最多晚「256 KiB ÷ 速率」。窗口的計算做成不做 I/O 的狀態機，以合成的回條與時間測試。K 條 channel 時同一個窗口管全部。不改 data 額度：額度量的是 Destination 那邊的記憶體，調小會連慢磁碟的緩衝一起限住。
  - 代價：pump 多一個等待與一份狀態；速率估計晃動時可能拖慢傳輸，目標 1 秒是取捨（設小控制更即時、較可能拖慢；設大則相反）；多一種每秒數百則的小訊息。
  - 還沒量：做了之後，乾淨路徑（2、30、120、250 ms）、本機、fatcat、nano4 的速度不能掉；掉包的長路徑上心跳與「按 Ctrl-C 到 host 收到停」的時間。
  - 2026-10-26 使用者決定：先不做，接受控制訊息在慢路徑上的延遲；之後改成列為改進選項。

---

## 計畫

![路線：第 0 步先量測，決定哪些值得做。A 線是資料面與穩定性：A1 block 資料面、額度與 Source 的擁有者，A2 session：端到端確認、重送與重連，client 把 stream 的兩端交給 server，A3 每條 SSH 連線 K 個 channel，好處看主機的 TCP buffer，A4 續傳、sparse、粗粒度 delta。B 線是 metadata 的並行：B1 決策者與執行者（已做），B2 以目錄為單位的 page 與平行掃描，目錄小而多時值得。另有三件隨時可以做的小事：manifest 以一條 stream 壓縮（已做）、刪除交給 pool（已做，B1 的第一步）、列目錄不重開並加大緩衝區（已做）](assets/redesign-roadmap.svg)

每一步結束時，都要能執行、測試過；rsync parity 測試與 `ferry test` 的情境，就是行為不變的安全網。A 線與 B 線大致獨立，可以平行，但都會改 `src/destination/`，要錯開。整個傳輸的可重現模擬，得等 B1 讓 Destination 也有擁有者；session 自己的模擬不必等。

**2026-10-25：** A2 的重連撤回，改為下面的「A2 的收回」；A3 改列為[改進選項](#改進選項)。圖還是原本的路線（[重新決定](#2026-10-25-重新決定一條-ssh-連線斷線就失敗)）。

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

- 步驟（2026-10-23 與使用者確認）：
  1. block 格式自成一個模組，連同分段的雜湊；先不接上傳輸，只有單元測試；量 B 與 S。
  2. 兩端一起換成 block：Source 送 block，Destination 解碼後直接交給 writer，大檔的各段以 `pwrite` 寫；拿掉 parser 與 record、frame 的串流。
  3. Source 改成擁有者加執行者：大檔的讀取移到 reader，`Known` 與 `Backlog` 的鎖改成擁有者的狀態。
  4. 資料額度取代 `DATA_QUEUE` 與 `DECODED`。
  5. 從 B1 移來的兩項：錯誤的分類收進決策者，以及決策者的合成事件單元測試。
  6. 驗收。
- 進度：
  - 第一步已做（2026-10-23）：`src/block.rs`，見 [4.2 節](#42-block)的「做了之後」與 B、S 的量測，當時定為 B = S = 8 MiB，第二步改為 2 MiB。
    - 單元測試 10 個：
      - 段大小 1、4、64 KiB，各 24 種長度，加上 8 MiB 的段三種長度，合併出的雜湊都等於 `blake3::hash`，包括最後一段只有一個位元組的情形；改一個位元組，雜湊就不同；
      - 任意的 pack 與 segment，壓縮或不壓縮，來回都不變；
      - 位元組不對的檔案或段、放錯 offset 的段，都被找出來；
      - 不合格式的 block 與 segment 被拒絕；隨意改動的位元組只會被拒絕，不會 panic。
    - 各拿掉一項檢查，就有測試失敗；試了 7 項。
    - 另加一個 fuzz target `block`，也排進每晚的 fuzz。本機跑了 3 分鐘、2,800 萬次，沒有問題。
  - 和計畫不同的地方：
    - 格式的細節，見 4.2 節的「做了之後」。
    - 「結束時」的第一項測試（分段的雜湊）在這一步就做了。
  - 第二步已做（2026-10-23）：資料改以 block 傳送，拿掉 `record.rs`、`frame.rs` 與 parser。
    - Source：reader 讀 1 MiB 以下的小檔，邊讀邊算 BLAKE3；file sender 把它們裝進 pack，滿 2 MiB，或眼下沒有東西可放時封口。更大的檔案仍由 file sender 依序讀：放得進 pack 的整個讀，其餘一次讀一段、邊讀邊算 CV，讀完後合併出 root，以新的 `FileHash` 封包送出。壓縮的 pool 把 pack 與 segment 編成 block，block sender 依序交給 mux，最後是結束標記。
    - Destination：資料封包由 block 的 Assembler 切出 block，連同 `FileHash`，依到達的順序交給解碼的 pool；pool 解壓、檢查每個檔案或每段，原樣的 block 不再複製。feeder 依序交給 writing 這條執行緒，它把每個完整的檔案、每一段交給 16 個 writer：以 segment 傳送的檔案，第一段到了才建立，各段以 `pwrite` 寫到位置，段全部寫完、`FileHash` 也到了，合併出的 root 相符，才交給 writer 設屬性，之後同樣分批改名。段或 root 不符、有一段讀不完整、或結束時段沒到齊，檔案都失敗，建立的暫存檔也拿掉。
    - 新增的測試：Source 送大檔分段、最後送 `FileHash`；Destination 以亂序收段、`FileHash` 在段之前或之後都寫對；root 不符、某段內容與 CV 不符、某段讀不完整（先到或後到）、段沒到齊，各自失敗而什麼都不留下；block 被切斷時不留下暫存檔。拿掉 root 的比對、忽略讀不完整的段、結束時放過沒到齊的檔案、建檔後不寫出先到的段，各有測試失敗。全部 481 個測試與本機的 `ferry test`（38 個情境通過、2 個略過；另以 `--scale 3G --kind mixed` copy、修改、sync 都相符）通過。
    - 本機的速度，與第一步之前的版本輪流：4 GiB 的檔案 2.79–2.93 秒，舊版 2.90–3.05 秒；20 萬個 4 KiB 的檔案各 10 輪，中位數都是 3.00 秒。主機上同時有別的工作，各輪差到 ±15%。
    - 和計畫不同的地方：
      - B = S 由 8 MiB 改為 2 MiB，見 4.2 節。
      - A1 的資料仍走一條有序的串流，所以 block 之後有結束標記（8 個 0 位元組），Destination 以它判斷哪些需要的檔案沒有送來，同現在 record 串流的結束。草稿裡「完成是狀態上的條件」要等 A2 的 session。
      - `FileHash` 是自己的一種封包，草稿把它列在 peer control 裡；A2 之前還沒有 peer control 的通道。
      - 大於 1 MiB 的檔案仍由 file sender 依序讀，reader 也只讀 1 MiB 以下的檔案，記憶體才和現在相同：reader 讀整個檔案，最多 64 個在途。改由 reader 讀各段，是第三步。
      - 以 segment 傳送的檔案的狀態（建立了沒有、哪些段寫了、root），由 writing 這條執行緒持有；草稿把它放在決策者，第五步錯誤分類收進決策者時再合併。
      - 額度還沒做：解碼後等待的 block 最多 16 MiB，writer 持有的最多 64 MiB，取代原本 `DECODED` 的 16 MiB；改成資料額度是第四步。
      - profile 的名稱跟著改：`parser` 改為 `writing`，`frame sender` 改為 `block sender`；拿掉不再用到的幾個等待狀態。
  - 第三步已做（2026-10-23）：Source 改成擁有者加執行者。`src/source/` 拆成 `owner.rs`（不做 I/O 的狀態機）、`read.rs`（reader）與 `mod.rs`（接線）。
    - 擁有者持有 `Known`、待讀的檔案、reader 手上的工作數、分段讀取中的檔案與各段的 CV、正在裝的 pack，以及掃描的兩個計數。pack 只記 reader 的緩衝區，由壓縮的 pool 接成 payload，所以擁有者不碰任何位元組。拿掉 `Known` 的 `Mutex` 與 `Backlog` 的 `Mutex` 加 `Condvar`。
    - 所有事件走同一個佇列，依發生的順序：掃描的一段（擁有者先記下它的項目，才讓掃描送出，Destination 需要的項目因此一定在）、Destination 的 needs、reader 做完的工作、閒置。執行者照擁有者的命令做事。
    - reader 讀放得進 pack 的檔案（2 MiB 以下），以及大檔的各段（`pread`，邊讀邊算 CV）。擁有者最多派 32 件工作；還有檔案等著開時，段最多占一半。
    - 單元測試 11 個，以合成的事件：依 Want 的順序讀並裝進 pack、pack 的上限、backlog 與 in-flight 的等待、未知的 ID、大檔各段亂序讀完後送 `FileHash`、某段讀不完整後不再讀也不送雜湊、小檔與大檔交錯、打不開的檔案、dry run。拿掉六項規則，各有測試失敗。
    - 擁有者的速度：只算它自己的時間，合成的事件每秒 1,130 萬個小檔。實際傳輸時，20 萬個 4 KiB 的檔案，擁有者忙 0.42 秒，約每秒 48 萬個，其餘時間在等壓縮。
    - 本機的速度，與第一步之前的版本輪流：4 GiB 的檔案 2.95–3.0 秒，舊版 3.02–3.08 秒；20 萬個 4 KiB 的檔案各 6 輪，中位數 3.08 秒，舊版 3.38 秒（含一輪 4.04 秒）。大檔沒有因為平行讀而變快：profile 中 Source 的 pump 大部分時間都在寫連線，瓶頸在 transport 的轉送，不在讀檔。
    - 和計畫不同的地方：
      - 掃描的每一段都要等擁有者回覆，是一次往返；草稿寫的是擁有者「先不收」，效果相同。20 萬個小檔時，掃描等擁有者共 0.5–0.75 秒；擁有者在等壓縮時，掃描也跟著等。
      - manifest 的額度仍是 mux 的 paced 佇列（8 MiB），由掃描在送出時等，擁有者還沒有接手。
      - 壓縮的 pool 滿時，擁有者送 block 會等。草稿的擁有者不等，以資料額度決定要不要交出 block，那是第四步。
      - reader 手上的工作上限改為 32 件、每件最多 2 MiB，原本是 64 個、每個最多 1 MiB，記憶體的上限相同。
      - 「額度用完的各種先後」的單元測試，要等第四步的額度。
      - profile 的名稱：`file sender` 換成 `owner`，新增「wait for the owner」，拿掉不再用到的「wait for the readers」。
  - 第四步已做（2026-10-23）：資料額度。
    - Destination 一開始以新的 `Credit` 封包給 Source 64 MiB 的額度；隱藏的 `--data-credit` 可以改，最少兩個最大的 block。每個 block 的記憶體放掉時（它的檔案寫完或失敗），記下它的份額，累積到額度的八分之一才送回一次。
    - 額度以 block 解開後的 payload 計，最少 4 KiB。草稿寫的是壓縮後的大小；改的理由：Destination 持有的是解開後的資料，以壓縮後計，原始碼約 ×6 時，64 MiB 就是 384 MiB；而 Source 的擁有者在交給壓縮之前就知道解開後的大小，不必等壓縮完。代價是可壓縮的資料在線上的量比較少，長鏈路需要時再調大。
    - Source 的擁有者只在額度之內交出 block；額度不夠時，block 先留著，也先不派新的讀檔。另外最多交 16 個還沒送出的 block 給壓縮，由 block sender 每送出一個就回報。
    - Destination 拿掉 `DECODED`、`HOLDING` 與解碼 pool 的容量限制，改由額度管：讀連線的執行緒不再等解碼的空間，manifest 與訊息也就不會卡在資料後面。
    - 新增的測試：擁有者以合成的事件 5 個（額度不夠時 block 等、讀檔也停；block 依序在額度夠時送出；最後一個 block 等到額度才結束；segment 也佔額度；壓縮那邊滿了先留著），Destination 1 個（給出與歸還的總量，留住的不到八分之一）。拿掉額度的檢查、或不歸還額度，各有測試失敗。以最小的額度（約 4 MiB）複製 crate 的原始碼與 4 GiB 的檔案，結果都相符。
    - 量到的問題與改法，都在 tmpfs、預設的 8 個核心上：
      - 每個 block 歸還一次額度時，4 GiB 要 2,048 則 `Credit`，每則都要經過兩端與 client 的好幾條執行緒，4 GiB 從約 3.0 秒變成 3.3–3.8 秒。給 24 個核心時兩版只差 2%，所以是三個程序擠在 8 個核心上搶 CPU。改成每八分之一歸還一次，是 512 則。
      - 額度全放在壓縮那邊時，Source 同時活著的緩衝區多了，page fault 約多一倍，CPU 時間多一成。限制在 16 個 block 之後，回到第三步的水準。
      - 額度調大調小（8 MiB 到 256 MiB），以及 mux 的資料佇列設不設上限，都沒有改善。
    - 本機的速度：4 GiB 3.06–3.17 秒，第三步 3.00–3.08 秒；20 萬個 4 KiB 的檔案各 8 輪，中位數 3.27 秒，第三步 3.13 秒、第一步之前 3.36 秒，各版的範圍大幅重疊。
    - 和計畫不同的地方：
      - 額度以解開後的大小計，見上。
      - 初始額度由 Destination 以 `Credit` 給，不是兩邊各自的常數；草稿只寫了由 Destination 決定。
      - mux 的資料佇列仍有 16 MiB 的上限，和額度並存：記憶體的上限是額度，量起來設不設上限一樣，留著是比較保守的做法。
      - 擁有者另外以 16 個 block 限制交給壓縮的量，草稿沒有寫。
      - 第 6 節的另外兩種額度，manifest 額度與 want 視窗 W，A1 的計畫沒有列，沒有做；manifest 仍由 mux 的 paced 佇列與掃描的兩個等待管。
  - 第五步已做（2026-10-23）：錯誤的分類收進決策者，以及決策者的合成事件單元測試。
    - 建立者只回報：一段做完，交回它的目錄打不開，或依項目的順序交回遇到的每個錯誤，以及設不了的屬性。由 applier 判斷：要為它刪掉的是要保護的（Source 的根或含它的目錄），但不是在建立目錄時，或磁碟滿、超過配額、唯讀，傳輸停止，以第一個為準；其他的只讓那個項目失敗；目錄打不開，它要改的都失敗。判斷的結果和之前建立者自己判斷的相同。
    - 寫入的錯誤也交給 applier：檔案寫不進、改不了名，或和 Source 讀到的不符，writing 這條執行緒連同錯誤交給 applier，由它判斷是單一檔案失敗，還是停止傳輸（root 沒有地方放、擋路的是要保護的、磁碟滿或唯讀），並告訴 Source 與使用者。writing 這條執行緒不再送任何東西給 Source。刪除線的結果在 B1 就由 applier 判斷。
    - 設不了的屬性不再由遇到的執行緒自己記成警告：建立者、writer、收尾的執行緒都連同結果交回，由它們回報的那條執行緒記下；傳輸停止時也照樣記下。
    - 新增的測試：
      - 決策者的狀態以合成的事件做單元測試 5 個，不碰檔案系統：一個目錄等的四件事（它的段做完、它的檔案寫完、它底下沒看到的目錄的段做完、manifest 離開它）的 24 種先後，每一種都在最後一件發生時才收尾，底下的先，根最後；結束時還沒收尾的，由深到淺收尾；一段要等它的目錄建好、多餘的也刪完，才交給建立者，只交一次；建立者遇到的各種錯誤、寫入遇到的各種錯誤，各自怎麼判斷。原本就有 2 個同類的測試（依深度收尾；Source 打不開的檔案，連同 settle 兩次）。
      - 端到端 1 個：新的注入點 `FERRY_TEST_FAIL=write` 讓 writer 寫整個檔案時失敗，檔案單獨失敗，什麼都不留下，結束狀態 23。這條路之前沒有測試。
      - 拿掉 11 項規則試：10 項各有測試失敗（完成的條件少了段、少了子目錄；建立目錄時的保護也停止；不等刪除就交出；收尾不依深度；目錄打不開時連相同的也算失敗；寫入失敗、改名失敗、Source 讀不完整，當成寫完；root 沒有地方放卻不停止）。1 項沒有：失敗的檔案不讓它的目錄少等一個，目錄就到結束時才收尾，結果相同，只是晚，沒有測試看得出來；改動之前也一樣。
    - 全部 503 個測試與本機的 `ferry test`（38 個情境通過、2 個略過）通過。
    - 和計畫不同的地方：
      - 收尾的錯誤仍由收尾自己的那條執行緒判斷，不是決策者；哪個目錄還在等底下哪些目錄收尾，也在它手上。要收進決策者，applier 得等到最後一個目錄收完尾才結束；現在它在資料結束時交出剩下的目錄，就先結束了。收尾的執行者只回報，這點和計畫相同。2026-10-24 使用者決定：和下一項一起，在 A2 之前單獨做，見 [B1 的補完](#b-線metadata-的並行)；同一晚做了（第二步）。
      - 以 segment 傳送的檔案的狀態（建立了沒有、哪些段寫了、root），以及檔案和 Source 讀到的符不符，仍由 writing 這條執行緒持有、檢查。第二步說要在這一步合併進決策者，沒有合併：它交給 applier 的是結果與錯誤，判斷在 applier。合併要讓每個 block 與 `FileHash` 成為決策者的事件，而決策者仍直接處理比較的結果，也還做少量 I/O（root、`-x` 時 root 的 fstat、dry run 列出時的 stat，見 B1），應該先改成不做 I/O 的狀態機。2026-10-24 使用者決定：在 A2 之前單獨做一個工作包，見 [B1 的補完](#b-線metadata-的並行)；同一晚做了（第一步與第三步）。
      - 合成事件的測試涵蓋的是決策者的狀態（目錄、段的等待、錯誤的判斷），不是整個事件迴圈：迴圈仍直接收比較的結果，經 mux 送訊息。別名（不分大小寫的檔案系統把兩個名稱當成同一個）要 stat 才看得出來，不能以合成的事件測，仍由經過檔案系統的測試涵蓋（`src/destination/tests.rs`，以及 CI 上 ext4-casefold 跑的 `tests/sync.rs`）。
      - 寫入的錯誤要經過 applier 才判斷，該停止傳輸時（例如磁碟滿）會晚一點停：applier 處理到那個事件之前，writing 這條執行緒還會交出幾件工作。停止之後失敗不再回報，所以不會多出訊息。
  - 第六步，驗收（2026-10-23）：A1 之前（`74b5e44`）與做完之後（`db1133a`）輪流，tmpfs → tmpfs，預設的 8 個核心，除非另外註明。兩項達到，一項沒有，一項只做到一部分；後兩項由使用者決定怎麼處理。
    - 4 GiB 單檔的吞吐量隨 writer 數上升：**沒有達到。**
      - 各 6 輪，A1 之前 2.91–3.13 秒（中位數約 3.0），現在 3.13–3.31 秒（約 3.2），慢約 5%。給 24 個核心時，2.55–2.68 秒對 2.62–2.91 秒，同樣慢約 5%。
      - writer 已經不是限制：A1 之前一個 writer 寫整個檔案，忙 2.37 秒（全程 2.99 秒）；現在各段分給 16 個 writer，合計 1.46 秒。Source 的 pump 與 Destination 讀連線的執行緒，大部分時間都在等連線。
      - 限制在 transport 的轉送：兩版都是約 6.76 萬個 64 KiB 的封包，經過 Source、client、Destination 約六條執行緒，換算成每個封包，A1 之前約 44 µs，現在約 49 µs。
      - 逐步量（各 4 輪，中位數）：A1 之前約 3.1 秒，第二步約 2.93 秒，第三步約 3.07 秒，第四步約 3.15 秒，做完約 3.18 秒。第二步比 A1 之前快，第三步（大檔的各段平行讀）與第四步（額度）各多約 0.1 秒。
      - CPU 時間（A1 之前 14 輪，現在 15 輪）：user 平均少約 0.9 秒（4.6–5.6 對 5.3–6.8 秒），sys 平均多約 1.3 秒（12.1–14.0 對 11.4–12.5 秒）。sys 多在哪裡，沒有再追。
      - 要讓 writer 的平行顯現出來，得先拿掉 client 的轉送，那是 A2；A2 之後要再量這一項。
      - 2026-10-24：慢的 5% 來自平行讀段；改成照順序讀之後，和 A1 之前一樣快，見下面的「第六步之後」。隨 writer 數上升這一項，仍要等 A2。
      - 2026-10-25 使用者決定：列為[改進選項](#改進選項)。
    - 20 萬個 4 KiB 的檔案不變慢：**看不出差別，可能慢 2–5%。**
      - tmpfs 各 10 輪，中位數 A1 之前 3.16 秒，現在 3.29 秒；全部釘在 NUMA node 0 上再各 10 輪，2.99 / 3.15 秒。20 對裡現在慢的有 14 對，差的中位數 0.1 秒；各輪本身就差到 ±15%。
      - CPU 時間差不多（各 6 輪：user 約 12 秒，sys 約 7.8 秒），page fault 少四成多。
      - tmpfs → ext4 各 6 輪，中位數 7.15 / 7.29 秒，6 對裡現在慢的有 4 對。
    - profile 中沒有一條執行緒專門讀大檔：**達到。** Source 沒有 file sender 了；4 GiB 的各段由 16 個 reader 讀，合計 1.97 秒；擁有者不做 I/O。
    - 大檔不再擋住小檔：**只做到一部分。**
      - 一個 4 GiB 的檔案排在 2 萬個 4 KiB 的檔案之前，各 3 輪，看目的地上的名稱何時出現。
      - A1 之前，小檔全在大檔寫完之後才出現：第一個約 3.15 秒，大檔約 3.15 秒。現在第一個約 0.5 秒就出現，但一半要到 3.2–3.3 秒，大檔約 3.05–3.19 秒。全程兩版一樣，3.3–3.7 秒。
      - 原因在 Source 的擁有者：額度不夠時，它不派任何讀檔；而一件段的工作是 2 MiB，一件小檔只有 4 KiB。額度因此幾乎都給了段，小檔的 pack 只在一開始出得去。
      - 要讓小檔先走，得改送的順序。那是政策：草稿裡由 Destination 的 Want 順序與 want 視窗 W 決定，A1 沒有做 W。要不要改、何時改，由使用者決定。
      - 2026-10-24 更正：上一句不對。W 只限制已 want、還沒定案的檔案數，預設 10 萬個，這個測試只有 2 萬個，碰不到；第 7 節把大檔的段與小檔怎麼交錯讀交給 Source 的擁有者。改成大檔照順序讀之後，這一項大致做到，見下面的「第六步之後」。
      - 2026-10-25 使用者決定：排在前面的大檔仍先佔住 reader 這件，列為[改進選項](#改進選項)。
    - 以合成的事件量 Source 的擁有者：**達到。** 每秒 869 萬個小檔；本機 4 KiB 小檔的實際速率約每秒 6.3 萬個（20 萬個約 3.15 秒），差約 140 倍。實際傳輸 20 萬個小檔時，擁有者忙 0.12 秒。
    - 「結束時」：所有測試（503 個）與本機的 `ferry test`（38 個情境通過、2 個略過）通過。列出的新測試都有，只有決策者的「別名」沒有以合成的事件測，見第五步。
    - 量測的程式與輸出在工作目錄的 `.scratch/verify/a1/`（`accept-*`、`mixed.py`、`profsum.py`）。量 CPU 時間時，迴圈放在管線裡，最後一輪的數字會包含之前每一輪的（zsh 以最後一個指令取代子 shell，子程序累計的資源用量跟著帶過去），那幾輪不算。
  - 第六步之後（2026-10-24）：在硬碟上量到 A1 的退步，改成大檔照順序讀。
    - 起因：使用者問大小檔交替讀寫會不會影響速度，並提供 fatcat 的硬碟（`/data/hdd0/tanch`，10 TB、xfs）來量。那裡沒有 root，每次以 fadvise 丟掉檔案的資料；目錄與 inode 仍在快取裡，別人也在用這顆硬碟。
    - 交替本身的代價小：本機 tmpfs 沒有差別；本機的 ext4（SSD），按位元組平分的試作版（只在 `.scratch`，沒有提交）9 對的中位數慢 2%；從硬碟讀，A1 的混合比它自己的大檔加小檔多約 6%；寫到硬碟，三版的混合都比分開慢約四成，A1 之前也是，各版的差別在雜訊內。
    - 但 A1 從硬碟讀大檔慢了約七成：4 GiB 單檔，A1 之前 27.1 秒，A1 44–48 秒。另以小程式在同一顆硬碟上量：
      - 一個檔案的段由 1、2、4、16 條 thread 同時讀：154、111–114、125–135、92–99 MB/s（各 2 輪）；各自開 descriptor，或每段先 WILLNEED，都只有 99–117 MB/s。
      - 不同的大檔同時各自循序讀：1 個 157 MB/s，2 個 143–146（一個接一個 145–150），4 個 130，8 個 127，16 個 113。
      - 所以主要是同一個檔案的段交錯讀，讓 kernel 的預讀失效；同時讀幾個檔案也會慢，2 個時只差 2–3%。
    - 驗收只在 tmpfs 上量（ext4 卡在磁碟），沒有發現。
    - 另一個問題：A1 把要的大檔全部開著。開檔數的上限為 256 時，600 個 2 MiB 多的檔案，A1 有 14 到 47 個開不了（`Too many open files`，結束狀態 23），A1 之前的版本沒有問題。
    - 決定（2026-10-24，使用者同意建議）：同一個檔案的段一次只給一個 reader，照順序讀。做了：
      - 掃描時就依大小分開，大於一段的檔案另排一列。大檔一個接一個讀，各段照順序，前一段讀完才派下一段，由空著的 reader 讀；最多開著 2 個大檔，正在讀的與先開好的下一個。
      - 有大檔要讀時，交給 reader 的工作不超過 reader 的數目，所以總有一個空著接下一段；只有小檔時，照舊最多 32 件。
      - 拿掉「有小檔在等時，段最多佔一半」的規則。
    - 和建議不同的地方：建議說「不同的檔案照樣平行讀」。小檔照樣平行，大檔改為一次一個，因為同時讀 4 個大檔就只剩 83%。大檔一次一個，也就是 A1 之前讀大檔的方式。
    - 新增與改寫的測試：Source 的擁有者以合成事件改寫 3 個（一段一段照順序、讀不完整的段、段也佔額度），新增 2 個（只有小檔時讀滿 32 件；大檔與小檔混著要，reader 照工作的先後完成：大檔一次一個、照要的順序開、各段照順序、最多開 2 個、有大檔時工作不超過 reader 數、全部送出）。拿掉其中 5 條規則，各有測試失敗。全部 504 個測試與本機的 `ferry test`（38 個情境通過、2 個略過）通過。上限 256 時 600 個大檔，複製完全相同；這沒有寫成測試，因為要寫 1.2 GB，開著的大檔數由合成事件的測試檢查。
    - 結果（輪流量）：
      - fatcat 硬碟 → RAM，4 GiB：A1 之前 27.1–27.2 秒，A1 44.2–48.3 秒，改後 27.05–27.06 秒。
      - fatcat 硬碟 → RAM，混合：A1 之前 37.0–37.5 秒，A1 56.9–59.7 秒，改後 38.8–40.1 秒。比 A1 之前慢約 7%：小檔與大檔同時讀，磁頭要來回。小檔一半在 16.5–20.6 秒寫好，A1 之前 32.7–33.3 秒。
      - fatcat RAM → 硬碟，混合，各 3 輪：A1 之前 75.0–120.6 秒，A1 73.7–106.5 秒，改後 54.0–114.8 秒。別人同時也在讀這顆硬碟（同一段時間裡多讀了 0–3.7 GB），各輪差到兩倍，看不出版本之間的差別；只有一輪別人沒在讀，那一輪改後 54.0 秒，A1 73.7 秒，但只有一輪。小檔一半寫好的時間比較穩：改後 13.3–24.6 秒，A1 之前 55.0–92.2 秒，A1 57.2–82.6 秒。
      - 本機 tmpfs，4 GiB：改後 2.86–3.06 秒，A1 3.20–3.62 秒；另與 A1 之前輪流，改後 2.85–2.98 秒，A1 之前 2.93–3.08 秒。驗收第一項的「慢約 5%」因此沒有了。
      - 本機 tmpfs，混合：改後 3.15–3.51 秒，A1 3.44–3.75 秒，A1 之前 3.44–3.62 秒；小檔一半在 0.57–0.78 秒寫好，A1 與 A1 之前都要 3.3–3.6 秒。
      - 本機 ext4，混合：改後 8.82–8.94 秒，A1 9.15–9.43 秒。
      - 20 萬個 4 KiB：14 對看不出差別（改後中位數 3.20 與 3.03 秒，A1 3.04 與 3.07 秒；後一組釘在 NUMA node 0）。
      - fatcat NFS → RAM，4 GiB（`/home`，NFSv4.1，`nconnect=8`，各 3 輪）：A1 之前 6.13–6.17 秒（約 700 MB/s；第一輪 10.55 秒不算，可能 NAS 還沒有快取），A1 3.74–3.99 秒（1.08–1.15 GB/s），改後 6.30–6.62 秒（650–680 MB/s）。NFS 上 A1 的平行讀段快約四成；改後回到 A1 之前的速度，略慢 2–7%，只有 2–3 輪，分不出是不是雜訊。這和建議時說的「快的儲存應該不會吃虧」不符，原因見[改進選項](#改進選項)。
      - fatcat NFS → RAM，混合（各 3 輪）：A1 之前 7.56–8.67 秒，A1 4.20–4.66 秒，改後 6.74–9.49 秒。小檔一半寫好：A1 之前 7.3–8.2 秒（在大檔之後），A1 4.0–4.4 秒，改後 0.54–1.3 秒。NFS 上整棵樹也是 A1 最快，改後的小檔最早。
      - RAM → fatcat NFS（兩版的 Destination 相同）：4 GiB，前兩輪 A1 之前 8.00–8.66 秒，A1 8.06–8.97 秒，改後 7.09–7.57 秒，第三輪三版都慢到 17.6–26.9 秒；混合，A1 之前 13.1–94.4 秒，A1 14.9–28.8 秒，改後 16.5–17.7 秒。NAS 的雜訊比版本間的差別大。
      - nano4 的 WekaFS（`/work`，在 login node 上量，負載 160–500，各 3 輪）：
        - WekaFS → RAM，4 GiB：A1 之前 6.42–6.97 秒，A1 6.13–6.35 秒，改後 5.85–6.16 秒。
        - WekaFS → RAM，混合：A1 之前 11.33–11.57 秒，A1 10.80–10.98 秒，改後 9.91–10.20 秒。第一個小檔：A1 之前在大檔之後（6.7–7.0 秒），A1 1.2–1.3 秒，改後 0.30–0.36 秒；一半：9.1–9.4、8.6–8.7、7.6–7.9 秒；大檔都在 6.3–7.4 秒寫好。
        - RAM → WekaFS，4 GiB：A1 之前 6.58–7.25 秒，A1 6.43–6.63 秒，改後 6.30–6.62 秒。
        - RAM → WekaFS，混合：A1 之前 19.77–26.22 秒，A1 18.64–18.75 秒，改後 17.87–18.33 秒。
        - 四項都是改後最快或打平。限制不在 WekaFS：它一條 thread 冷讀就有 3.9–4.0 GB/s，4 GiB 卻要 6 秒左右；是哪裡沒有追。
        - 但 WekaFS 上小檔仍大多排在大檔之後，一半要到全程的四分之三。可能的原因：WekaFS 上開一個小檔比讀一段慢得多，額度不夠時，誰先讀好誰先拿額度，段因此仍拿走大部分；沒有驗證。若是這樣，按位元組平分會有幫助，沒有試。
    - 決定（2026-10-24，使用者）：保留照順序讀，接受 NFS 上的這個代價；依檔案系統決定同時讀幾段，列為[改進選項](#改進選項)。
    - 驗收的第三項（大檔不再擋住小檔）因此在 tmpfs 與硬碟上大致做到：大檔一次只有一段在讀，額度不再幾乎都給段。WekaFS 上沒有做到，見上。按位元組平分還要不要做，由使用者決定。
    - 決定（2026-10-24，使用者）：不做按位元組平分，不刻意分配大檔與小檔，一視同仁，照接收順序讀，額度先到先得。也不偵測儲存媒介。改成不論大小同時讀 16 個檔案（使用者先問 rclone 怎麼做：`--transfers` 預設 4 個、不分大小，≥ 256 MiB 的檔案再以 4 個 stream、每個連續 64 MiB 平行傳，本機到本機不切，不偵測儲存媒介；再比 4、8、16 個，選 16）。
      - 補量小檔同時 1、4、8、16 個（`nfiles.py`，個／秒）：本機 SSD 14.1k、41.9k、62.4k、60.3k；fatcat 硬碟 671、952、1,246、1,554；fatcat NFS 4.5k、16.3k、29.6k、47.9k。小檔要 16 個才拿到大部分；大檔 4、8、16 個差不多。
      - 做法：所有需要的檔案排成一列，照接收的順序；16 個 reader 同時讀 16 個檔案，最多派出 32 個，讓每個 reader 都有下一個在等；每個檔案同一時間只有一件工作在 reader 手上，大檔照順序一段一段讀，開著的大檔的下一段排在還沒開的檔案前面。拿掉大檔另排的一列、「最多開 2 個大檔」與「有大檔時工作不超過 reader 數」。
      - 先試了「最多派出 16 個」，NFS 上 2 萬個小檔慢了（中位數 0.63 對 0.56 秒，最慢 1.16 秒）：每讀完一個都要等擁有者派下一個。改成 32 個就回來了。
      - 量到的（從來源讀到 fatcat 的 RAM，中位數）：NFS 上 16 個 128 MiB，舊的一次一個 3.22 秒，現在 1.96 秒；2 萬個 4 KiB，0.56 與 0.54 秒；兩者混合，3.45 與 2.37 秒，但小檔一半寫好由 0.5 秒變 2.2 秒，排在前面的大檔先佔了 reader。fatcat 硬碟上 16 個 128 MiB，16.2 與 15.4 秒（先前 5 輪 17.6 與 15.5 秒），不像小程式預測的慢兩成，ferry 一次一個時一段讀完才派下一段，中間的空檔被同時讀的檔案填上，這是推測；以先試的「最多派出 16 個」量，2 萬個 4 KiB 一樣（19.5 與 19.7 秒），4 GiB 加 2 萬個小檔也一樣（69.2 與 70.2 秒）；以最後的 32 個再量各 3 輪，2 萬個 4 KiB 19.5 與 20.4 秒（範圍重疊），4 GiB 加 2 萬個小檔 61.9 與 69.1 秒（三對裡兩對慢一成，一對快 5%；別人也在用硬碟，各輪由 40 到 74 秒），但小檔一半寫好由 18.6–27.1 秒提早到 7.7–11.6 秒。我的推測：大檔的下一段排在已派出的小檔後面，小檔先走，兩者交錯得更多，磁頭來回也更多。本機 RAM → ext4：20 萬個 4 KiB 1.96 與 1.89 秒，16 個 256 MiB 與混合的樹都一樣（卡在本機的傳輸）。開檔上限 256 時 600 個 2 MiB 多的檔案，複製完全相同。WekaFS 沒有量（nano4 要使用者登入）。
      - 程式與輸出：`.scratch/r16/`（`treecopy.py`、`*.out`）、`.scratch/verify/a1/n48-*.out`。
    - 另外看到：Destination 改名一批要滿 256 個才交出，最後不滿一批的小檔要等整個傳輸結束才有名稱。2026-10-24 改了：不滿的一批，第一個檔案進來 1 秒後就交出（使用者決定 1 秒），混合的樹裡最後一個小檔在 1.8–2.0 秒取得名稱，原本 3.6 秒，見 [fsync.md](fsync.md#改名的批次)。批次原是為 fsync 加的；fsync 拿掉後試過拿掉批次，寫好一個就交給目錄的改名執行緒，本機 ext4 與 NFS 都變慢（NFS 上 2 萬個小檔 6.24 對 6.68 秒），批次留著，見 [fsync.md](fsync.md#改名的批次)。這對第 8 節的名稱操作 pool 也有意義：改名緊跟在建檔後面，會在同一個目錄裡搶鎖。
    - 同時讀寫多個檔的影響（2026-10-24，使用者問；以小程式量，不是 ferry）。大檔是 16 個 128 MiB 的檔案，共 2 GiB；讀之前以 fadvise 丟掉資料，目錄與 inode 仍在快取裡；寫時每個檔案寫完就 fsync，N 個 fsync 同時等，如當時 Destination 的做法（fsync 在同一天拿掉）。小檔讀 2 萬個、寫 5 千個 4 KiB 的檔案，寫的各自 fsync。表中是中位數：硬碟 7 輪，其中讀只有 1 輪別人沒在寫；其他 3 輪。

      | N：1 → 16（小檔 1、16、64） | fatcat 硬碟（xfs） | 本機 SSD（ext4，3 顆 RAID0） | fatcat NFS | nano4 WekaFS |
      |---|---|---|---|---|
      | 同時讀 N 個大檔 | 231 → 177 MB/s（安靜的那輪）；別人在寫時都約 145 | 839 → 855 | 815 → 1,151（2 個就有 1,118） | 3.3 → 13.1 GB/s（8 個 14.4） |
      | 一個大檔的段由 N 條同時讀 | 154 → 92–99 | 853 → 857 | 691–793 → 1,065–1,077 | 3.9–4.0 → 0.8–1.0 GB/s（4 條 7.3–9.6） |
      | 同時寫 N 個大檔，各自 fsync | 177 → 80 | 768 → 778 | 1,004 → 1,119 | 2.4 → 4.1 GB/s |
      | 一個大檔由 N 條同時寫，最後 fsync | 228 → 229 | 584 → 513 | 710 → 698 | 1.64 → 0.98 GB/s |
      | 小檔，N 條讀（個／秒） | 475、1,247、1,624 | 13.4k、60.2k、74.8k | 4.7k、48.3k、52.3k | 1.2k、15.5k、17.6k |
      | 小檔，N 條寫並 fsync | 28、197、411（更正，見下） | 4.6k、18.1k、21.2k | 2.9k、3.3k、5.8k | 846、1,237、2,404 |

      - 讀大檔：硬碟上同時讀越多越慢，一次一個最快；SSD 不受影響；NFS 與 WekaFS 一個檔案填不滿，同時讀好幾個才快。同一個檔案的段同時讀，硬碟與 WekaFS（16 條）都比一次一段慢，只有 NFS 變快。
      - 寫：資料先進 page cache，到磁碟的順序由 fsync 決定。同時 fsync 16 個大檔，硬碟剩一半不到，8 個少約四分之一，2 到 4 個和一個差不多。一個大檔由 16 條寫、最後一次 fsync，硬碟與 NFS 不受影響，因為 kernel 照位置寫回；SSD 慢約一成，WekaFS 慢四成。
      - 小檔：各處都是越多條越快，讀時 16 條已拿到大部分。寫時同時等的 fsync 越多，一次 journal 寫入涵蓋的檔案越多，硬碟上 64 條是 1 條的 15–22 倍；當時 Destination 的 64 個 fsync 執行緒因此是對的。
      - 對 ferry：大檔一個接一個讀，讀的一側在硬碟上是最好的情形。寫的一側當時不是：好幾個大檔同時等 fsync，16 個 256 MiB 的檔案從 RAM 複製到 fatcat 的硬碟，比 `cp` 再 `sync` 慢一倍以上；量測搬到 [fsync.md](fsync.md)，fsync 也已拿掉。另外，Destination 寫一個大檔最多用 16 個 writer，WekaFS 上這比 1 個慢四成，但 ferry 在那裡只有約 0.65 GB/s，還碰不到。2026-10-24 照使用者的決定改成一個檔案同一時間只有一個 writer，段照到達的順序一段一段寫（debug build 檢查同一個檔案不會有兩段同時在寫）。量到的（中位數）：RAM → 本機 tmpfs、ext4 的 4 GiB 單檔 3.01 與 2.99 秒、3.12 與 3.11 秒，再 sync 8.57 與 8.54 秒；16 個 256 MiB 一樣；RAM → fatcat NFS 的 2 GiB 單檔 3.58 與 3.64 秒，16 個 128 MiB 3.24 與 3.27 秒（各 5 輪；先前 3 輪那組 NAS 忽快忽慢，7 秒到 39 秒）；硬碟加 sync 太亂，中位數一個 writer 不比較慢。WekaFS 沒有量。輸出在 `.scratch/w1/`。
    - 有無 fsync 的比較（2026-10-24，使用者問；同一個小程式）：每檔 fsync、最後 sync 一次、不 sync 三種寫法，大檔與小檔、N 條，四種儲存。表、解讀，與硬碟小檔數字的更正，搬到 [fsync.md](fsync.md#量到的代價)；fsync 在同一天拿掉。
    - 量測的程式與輸出：`.scratch/verify/a1/` 的 `hddmixed.py`、`segread.py`、`tworead.py`、`nread.py`、`onebig.py`、`nfiles.py`、`nsum.py`、`copybench.py`、`many.py`、`nano4-run.sh`、`nano4-sync-run.sh`、`fatcat-*.sh`，與 `hdd-*.out`、`seq-*.out`、`segread-*.out`、`nfiles-*.out`、`sync-*.out`、`nano4.out`、`fatcat-nfs-and-extra.out`、`many-hdd.out`；試作版在 `.scratch/wt-fair`，差異在 `fair2.diff`。
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

**2026-10-25：** 重連與 session 的端到端可靠性撤回，見下面的「A2 的收回」。以下保留原本的計畫與六步的紀錄。

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
  - 整個傳輸（兩個 peer 連同 session）的可重現模擬，要等決策者不做 I/O（B1 的補完，排在 A2 之前）；在那之前，以真的 thread 跑同樣的情境，種子只決定何時斷線，失敗時不一定重現得出來；
  - 三種組合的 `ferry test` 都通過，而且 client 的 process 沒有經手任何資料；
  - `ferry test` 的 `--cut-a-path` 情境改為預期成功；
  - 傳輸中殺掉一個 ssh master，傳輸接著完成。
- 步驟（2026-10-24 夜裡自行訂的；2026-10-25 使用者同意第二到第六步）：
  1. session 的核心（做了）：不做 I/O 的狀態機，時間由外面給（`src/transport/session.rs`）；control 的序號、累計確認與重送；block 的編號、保留、確認、去重與重送；選 stream；stream 的重新加入；每條 stream 的健康（閒置 5 秒送 keepalive，沉默 20 秒算斷）、寬限 1 分鐘、`End` 與 `Closed`、`Lost`。還沒有接上任何東西，行為不變。
  2. frame 的編碼、stream 的讀寫執行者與 session 的驅動者，以 socketpair 測（做了）：`src/transport/frame.rs`、`src/transport/driver.rs`。還沒有接上任何東西，行為不變。
  3. 報告改走 server control（做了）。
  4. client 把 stream 的兩端交給 server（`SCM_RIGHTS`），peer 改用 session；拿掉 `relay.rs`、全域的重排、heartbeat 與 `LET_GO`（做了）。
  5. client 的連線管理：重建 stream；寬限 1 分鐘；要登入時在終端機上問（做了）。
  6. 驗收（做了）。
- 第一步的結果（2026-10-24）：
  - 兩個 session 以模擬的 stream 對接：
    - 延遲隨機；stream 隨機斷，多半兩個方向都斷，也有只斷一個方向、只能靠沉默發現的；斷了之後還在路上的 frame 都丟掉；client 聽到斷了，過一會兒建一條新的。
    - 兩端接上假的 peer：送 control 與最大 200 KiB 的 block，送完、收完就要 session 關閉。
    - 300 個種子，每個斷 2–12 次；100 個種子只有一條 stream，斷了很快發現、也很快接回，每個斷 62–88 次；50 個種子 client 不再重建，兩邊都在寬限之後 `Lost`。
    - 檢查：每則 control 恰好一次、依序，每個 block 恰好一次，兩邊都 `Closed`；同一個種子的命令序列相同；沒有一條 stream 違反 session 的規則；session 要被叫醒的時間永遠在現在之後；關掉的 stream 不再寫。
    - 另外以 release 跑了 24,000 個種子（不在測試裡），共斷 257,580 次，都通過。
  - 健康判斷以合成的事件測：沉默、只有一個方向通、時斷時續、寬限到期、寬限到期後才回來的 stream；也測了其他 session 的 stream、編號不對的 stream。
  - 驗收裡的「重複」：stream 自己不會重複，重複來自斷線後的重送，模擬中每個種子都有。
- 和計畫不同的地方（第一步）：
  - `Ack` 多了一個累計的欄位：`Ack { ack, below, blocks }`，`below` 以下的 block 都收到了。只照草稿列出收到的 block 的話，帶著確認的 stream 一斷，那些確認就丟了，送的一方永遠留著那些 block，不會結束；模擬的第 71 個種子找到的。現在帶確認的 stream 斷了，下一條會把 `below` 以上收到的全部再說一次。
  - `End` 的意思：這一邊的 peer 做完了，送出的都已被確認。之後仍可能有確認，因為對方可能還在送，所以送過 `End` 也照樣送 keepalive。收到 `End` 的 stream 之後結束，要這一邊也送過 `End` 才不算斷線；在那之前，這一邊還需要 stream。`End` 之後才加入的 stream 也會收到 `End`；已經 `Closed` 的 session，對之後加入的 stream 先說 `End` 再關掉，以防對方因為 stream 斷了而沒收到。
  - session 不叫 peer 等：control 由協定的額度限住（3 節：session 照單全收），沒有另外的上限。
  - stream 違反 session 的規則時（不是以 `Hello` 開頭、其他 session 的、編號不對、control 跳號、fragment 的位置不對），當成斷了，並告訴 client 原因（`Warn`）；草稿沒有說要不要告訴。模擬中從來沒有發生。
  - 交給一條 stream 的 writer、還沒寫出去的，最多約 4 個 fragment（256 KiB），所以 control 最多等這麼多。
  - 模組叫 `transport::session`；原本叫 `session` 的握手模組，2026-10-25 使用者同意後改名為 `handshake`（`4669787`）。
- 第二步的結果（2026-10-25）：
  - frame 的編碼：照 5.1 節，`[kind: u8][length: u32][payload]`，數字都是 little-endian，各種 frame 的 payload 列在模組開頭的表裡。讀的一方在配置記憶體之前，先依種類檢查長度：fragment 最多 64 KiB 加上它的欄位，control 與 `Ack` 最多和一個封包一樣（16 MiB）。stream 在 frame 中間結束，算斷線；讀到的不是 frame，才算違反規則，並告訴 client。
  - 驅動者：一條執行緒擁有 session；每條 stream 各有一條讀、一條寫的執行緒。讀的一方每讀到一個 frame 就交給驅動者，所以 stream 永遠讀乾。寫的一方 flush 出去之後，才回報寫了多少。stream 斷了，立刻以 `shutdown` 關掉兩個方向，它的兩端與兩條執行緒都因此結束；正常結束的，等寫的一方寫完才關。session 交給 peer 的，以及放棄與結束，送到 peer 的收件匣；stream 斷了，以及違反規則的原因，送給 client。所有的佇列都不設上限，驅動者從不等待：量由協定的額度限住（第 6 節）。
  - 測試以 socketpair 與真的執行緒：兩個 session 以 1 到 3 條 stream 對接，各送 100 到 150 則 control 與 200 到 300 個最大約 320 KiB 的 block。情境有三種：stream 都不斷；傳輸中每 2 ms 斷一條，共 20 條，每三條有一條只關一個方向，每斷一條就接一條新的；全部同時斷，200 ms 後再接兩條。每種都檢查 control 恰好一次、依序，block 恰好一次。
  - 另外的測試：已經 `Closed` 的 session，對之後加入的 stream 回 `Hello` 與 `End`；讀到的不是 frame，那條 stream 被關掉，client 收到原因，session 照常用其他 stream；同一個編號還在用時又加入，拒絕；handle 都放掉之後，驅動者關掉 stream 並結束。frame 的編碼有 4 個測試：各種 frame 讀回相同；在 frame 中間斷掉；各種不是 frame 的；太大的不寫。
  - 驅動者的測試連跑 40 次，以及同時跑 4 份、共 10 輪，都通過。
- 和計畫不同的地方（第二步）：
  - 驅動者輪流處理 stream 上來的與 peer 交來的。第一版讓 stream 上來的先；分析一次偶發的測試失敗（30 次中 1 次）時發現，資料一直進來時，peer 的 control 會一直排在後面，在 Destination 上就是額度晚還。那次失敗本身是測試的競態，也一併改了。
  - stream 的編號由交出它的一方（client）決定，兩個 server 用同一個；同一個編號還在用時又加入，拒絕並告訴 client。草稿沒有說編號由誰給。
  - 寫的一方 flush 之後才回報寫了：session 判斷沒有東西要寫、宣告 `Closed` 時，`End` 已經交給 kernel，之後關掉 stream 不會丟掉它。代價是每條 stream 在途的最多約 256 KiB，吞吐量要在第四步量。
  - pipe 無法 `shutdown`：以 pipe 交來的 stream 斷了時，讀它的執行緒要等另一端結束才結束。
- 第三步的結果（2026-10-25）：
  - server 的 peer 把報告交給一個 mux，它的 pump 寫到 control 連線的 channel，每個封包在鎖下整個寫出；server 自己在那裡說的（metrics、快照、內部錯誤）因此只會插在兩個封包之間。dry run 列出的路徑照舊在 8 MiB 時等待；其他報告進不設上限的佇列，peer 的執行緒從不因 client 顯示得慢而等待。peer 的 `Options` 改成直接帶 `Reporter`。peer 完成時的 profile 也改走 control。
  - client 的 relay 不再讀報告：每個封包原樣轉送，只解碼錯誤。每個 server 的 control 連線各由一條執行緒讀到結尾，報告交給顯示，profile 照舊保存。relay 先等兩條都讀完，才關閉 server；失敗時最多等 5 秒，和原本給 server 結束的時間共用。
  - 測試：Destination 的單元測試改從 channel 收報告，並檢查它送回的封包裡沒有報告；`tests/server.rs` 的 control 連線上現在也有 peer 的報告。全部 550 個測試、本機的 `ferry test`（同一處，以及跨 tmpfs 與工作目錄的磁碟，各 36 個通過、2 個略過）通過；3,000 個檔案的複製，摘要的計數和前一版相同，dry run 列出的也相同，兩個 server 的 profile 都在。
- 和計畫不同的地方（第三步）：
  - 計畫只有一句。關閉 server 之前先把 control 讀到結尾，是因為上面現在有最後的計數：原本那裡只有 metrics 與快照，丟了也無妨，client 關閉 server 時會一併放掉 control 的 channel，連同還在路上的。
  - relay 解碼錯誤、以知道傳輸失敗的部分，留到第四步拿掉 relay 時，一起改走 server control。
  - 另外修了一個偶爾失敗的測試：`scan` 的「深的樹只開有限個目錄」數的是整個 process 開著的描述子，同時跑的測試開的也算進去；第二步的驅動者測試會同時開上百個 socket，完整測試時就失敗了一次。改成只數指向它自己暫存目錄的描述子（深 600 層的樹，最多 131 個，上限 256）。
- 第四步的結果（2026-10-25；`259b8a2`、`c37f548`、`d1d2476`、`e9702e2`）：
  - client 為每條 stream 建一對 socket，把兩端交給兩個 server：本機的 server 經它的 socket 拿到一端，和 joining process 交出輸入輸出的方式相同；host 上的，一端是 SSH channel（直連 socket，或執行 joining process）的輸入輸出。兩邊都在 host 上時，兩個 ssh 背對背，位元組由 kernel 在兩個 ssh 之間搬。stream 數是兩邊 SSH 連線數較多的一邊，本機算一條。
  - 每個 server 先讀到的位元組，client 在交出之前就寫進另一端：直連 channel 的那個位元組、帶 stream 編號的標頭，以及在另一邊是本機的 server 時（它在 session 之前什麼都不寫）的開頭標記。host 上的 server 回直連的 channel 時寫開頭標記，joining process 則在 shell 印的東西之後寫。所以每條 stream 每個方向正好一個開頭標記，server 讀的一方先跳到它。
  - server 跑一個 session，stream 隨時加入，在 control 連線的 channel 之前或之後都可以。peer 本身沒有改：server 裡一條執行緒把 peer 寫出的封包交給 session，每個封包是一則 control，但 data 封包組回完整的 block，交成 session 的 block，結束改成一則新的 `BlocksEnd`，說共有幾個；另一條把 session 交來的變回封包給 peer，block 各成一個 data 封包，到齊了才給結束，對方的 peer 做完了才結束 peer 的輸入。peer 與 session 都做完，server 才結束。
  - client 不再轉送任何東西，只跟著兩個 server 的 control 連線（報告、metrics、失敗），直到兩個都結束；一個結束了，或回報失敗，另一個只再等 5 秒，之後關掉它的 control 連線，讓它自己停下。
  - dashboard 的 SSH 連線改由 host 上的 server 回報：session 每秒說每條 stream 送收了多少、多久前聽到對方，client 依 stream 編號放到它走的 SSH 連線上。佇列表以每條 stream 寫入者手上、還沒寫出的位元組取代 path 的佇列（「send」與「back」）；client 自己的兩列與 Destination 的「receive」拿掉，佇列表由 14 列變 11 列。
  - 測試：全部 541 個；舊 transport 的 20 個測試拿掉，新增 control 連線的 channel（直連、一開就關、經 shell 加入、process 先結束）、stream 的各種接法（兩邊本機、本機對直連的 host、兩個 host 背對背、經 shell 加入、切斷一條），以及 SSH 連線的彙整與 stream 的回報。本機的 `ferry test`，同一處與跨兩個檔案系統，各 36 個通過、2 個略過；以 fatcat，從本機直連過去、從 fatcat 經 shell 加入回來，各 39 個通過、沒有略過。另外在 fatcat 上：來回複製、fatcat 到 fatcat 背對背（直連與加入）、3 條 SSH 連線、切斷一條 stream，結果都和來源相同。
  - 量測（釘在一個 NUMA node 上，中位數，第四步之前的 `259b8a2` 對做完之後）：2 GiB 單檔 1.50 → 1.35 秒（6 輪）；20 萬個 4 KiB 的檔案 1.61 → 1.53 秒（8 輪）；1 GiB 加 2 萬個小檔 1.03 → 0.92 秒（6 輪）。剛切換時 2 GiB 要 2.12 秒，見下面 pipe 的部分。fatcat 上 300 MiB、兩條 SSH 連線約 108 MiB/s。
- 和計畫不同的地方（第四步）：
  - peer 沒有改成直接用 session 的介面，而是在 server 裡加轉接：草稿說「peer 改成只往 session 的介面送 control 與 block」。這樣第四步不必同時改 Source 與 Destination 的收送，風險小；代價是多幾次複製。要不要改成直接用，看 A2 驗收時的量測。
  - 剛切換時 2 GiB 單檔從 1.50 秒變 2.12 秒：轉接的執行緒把 2 MiB 的 block 一次寫進 64 KiB 的 pipe，兩端一來一往，額度跟著晚還。pipe 加大到 1 MiB（Linux 不需權限的上限）之後變 1.35 秒。
  - session 多了一個命令 `Ended`：對方的 peer 做完了，它送的都已交到。peer 靠輸入結束得知對方做完，而 session 的 `Closed` 要兩邊都做完，不加會互等。對方只在它送的都被確認之後才說 `End`，所以收到時一定都交到了。
  - stream 的標頭是 `[kind][number]`，編號由 client 給；client 交出前先寫好每個 server 要先讀的位元組，開頭標記每個方向一個。草稿只說「以 `Hello` 帶著 session ID 加入」。session ID 在握手的 hello 裡，由 client 為每次傳輸隨機產生。
  - peer 的失敗由 server 在 control 連線上以新的報告 `Failed` 告訴 client，client 以第一個為傳輸的原因；一邊結束或失敗後，另一邊最多再給 5 秒，否則它的 session 會為了等對方回來，等滿寬限的一分鐘。
  - 停滯時列出執行緒（60 秒沒有動靜）改以 server 回報的計數變動為「動」，訊息由「nothing has passed between the servers」改為「nothing has moved」。
  - `--cut-a-path` 改名 `--cut-a-stream`：多開一條、開好就切斷。傳輸照常用其他 stream 完成，所以 `ferry test` 的情境與 `tests/sync.rs` 的測試改為預期成功：這原是第六步驗收的一項，提早到這裡。
  - 第三步讓報告走的 mux，它的佇列與 peer 的同名，dashboard 把它們算了兩次（「data ×2」）；改了名字。
  - 還沒做：stream 斷了不會重建（第五步），所以只有一條 stream 時一斷，就等滿寬限的一分鐘再失敗，以前是立刻失敗；server 在 session 結束後就離開，已 `Closed` 的 session 回應之後加入的 stream，要等第五步讓 server 多留一會兒才用得上。設計文件與 CHANGELOG 留到驗收時一起改。
- 第五步的結果（2026-10-25；`962fc25`）：
  - server 在 control 連線上告訴 client：哪條 stream 斷了，以及它的 peer 與 session 都做完了。做完之後 server 不離開，它的 session 照樣回應之後加入的 stream（以防對方沒收到 `End`），直到 client 關掉 control 連線；兩個都做完，client 才關。
  - 任一個 server 說某條 stream 斷了，client 就重建一條，只建一次：用下一個沒用過、而且兩邊都走同一條 SSH 連線的編號（兩邊連線數的最小公倍數為間隔），只重建兩個 server 都拿到過的 stream。建不成的 2 秒後再試，只說一次；有一個 server 已經不在，就不再重建。
  - 在 host 上重建之前，先確認那條 SSH 連線的 master 還在；不在就重新登入：先不問；被拒絕、又有終端機，才在終端機上問，dashboard 或進度列這時交出終端機，也不讀使用者打的字；連不上的 host 則之後再不問地試。
  - 一個 server 沒說做完就不在了，另一個立刻請它停下，不再等 5 秒。注入的 panic 由約 5 秒變成約 0.1 秒結束。
  - `--cut-a-stream` 改成切斷最後一條、不多開，所以一定要重建：本機只有一條 stream，切斷後照樣完成。
  - 測試：全部 544 個，含借用終端機的交接、stream 只重建一次而且走同一條連線；`ferry test` 本機兩種各 36 個通過、2 個略過，fatcat 兩個方向各 39 個通過；fatcat 上切斷一條 stream（直連、加入、兩條 SSH 連線、從 fatcat 回來）都重建成功；複製 600 MiB 時讓兩條 SSH 連線中的一條的 master 結束，不問地登入了新的 master、重建 stream，複製完成，檔案相同。
- 和計畫不同的地方（第五步）：
  - server 做完之後留下、等 client 關掉 control 連線才結束，草稿沒有寫；不這樣，已 `Closed` 的 session 回應之後加入的 stream 就用不上。
  - 一個 server 不在了，另一個立刻停下：它的 session 只會等滿寬限。草稿的「client 確定一邊已經不在時，可以直接叫另一邊放棄」，這裡做了。
  - 要在終端機上問的那條路，這裡沒有需要輸入 OTP 的 host 可以實測，只有交接的單元測試。
- 第六步的結果（2026-10-25；`56b4b27`，以及之後的文件）：
  - 驗收的各項：
    - 兩個 session 以模擬的 stream 對接、數百個種子；session 的健康判斷以合成的事件測：第一步做了，見上。
    - 網路計時器：程式裡只有 session 的三個（閒置 5 秒送 keepalive、沉默 20 秒算斷、寬限 1 分鐘），以及 client 的連線管理：安靜登入時等 master 最多 30 秒；一邊失敗或不在之後，給另一邊結束的 5 秒；一邊做完之後，給另一邊做完的寬限加 15 秒；重建不成時 2 秒後再試；結束時給 channel 的 process 自己結束的 5 秒。另外是 SSH master 的 `ServerAliveInterval`，5.3 節保留它。其他的計時（dashboard 與進度列的更新、每秒的 metrics 與 stream 的回報、寫入的批次、60 秒沒有動靜時列出執行緒）都不判斷連線。
    - 整個傳輸的可重現模擬沒有做，照計畫以真的 thread 代替：`FERRY_TEST_BREAK=<種子>`（只在 debug build）讓 client 每 5 到 50 ms 切斷一條 stream，最多 30 條，切哪條、何時切由種子決定。測試裡 3 個種子，複製 16 MiB 的檔案加 800 個小檔，每次都和來源相同、沒有留下暫存檔；另外在 fatcat 上跑了 20 個種子，共斷 122 次，都成功。
    - 三種組合的 `ferry test`：本機（同一處，以及跨兩個檔案系統）各 36 個通過、2 個略過；以 fatcat，從本機直連過去、從 fatcat 經 shell 加入回來、fatcat 到 fatcat，各 39 個通過、沒有略過。
    - client 沒有經手資料：client 只把 stream 的兩端交出去，自己不讀不寫；只有 `FERRY_TEST_BREAK` 時多留一份 Source 那端，好切斷它。本機複製 1 GiB，client 各執行緒的 CPU 時間合計，第四步之前 0.52–0.57 秒，現在不到 10 ms。
    - `--cut-a-stream` 的情境預期成功：第四步做了，第五步改成一定要重建。
    - 傳輸中殺掉一個 ssh master：fatcat 上兩條 SSH 連線，複製 600 MiB 時以 `ssh -O exit` 或 `kill -9` 結束第二條的 master，不問地登入了新的，重建 stream，複製完成，sha256 相同。第一條的 master 結束，傳輸失敗，見下面。
  - 量測：tmpfs → tmpfs 的 4 GiB 單檔，第四步之前（`259b8a2`）對現在，中位數 2.91 → 2.64 秒。現在的 profile：Destination 的 session 執行緒忙 1.47 秒（全程 2.72 秒）：它把收到的 fragment 複製進 block，4 GiB 就複製 4 GiB，忙的時間應該大多在這裡，沒有再細分；把 block 寫給 peer 的轉接執行緒，有 2.39 秒在等 pipe；Source 那邊，把 data 封包組回 block 的轉接執行緒忙 0.61 秒，stream 的寫入者有 1.39 秒在等 socket。所以現在的限制是 Destination 的 session 與轉接，列為兩個[改進選項](#改進選項)。
  - 文件：`architecture.md`（client 建 stream、transport、connections、server 的報告與結束）、`pipeline.md`、`dashboard.md`、`profiling.md`、`overview.md`、`cli.md` 以及各自的 `.zh-TW.md`，七張圖（`architecture-peers`、`architecture-transport`、`architecture-connections`、`pipeline`、`pipeline-workers`、`pipeline-scan`、`dashboard-colors`），CHANGELOG 的 Unreleased，`TODO.md` 的 Reconnecting。
- 和計畫不同的地方（第六步）：
  - **server control 沒有交給 session 判斷。** 計畫的「連線的健康集中」說 session 判斷每條 stream，包括 server control；做的是 control 連線仍是一條普通的 channel，走第一條 SSH 連線，server 隨它結束。所以第一條 SSH 連線的 master 一結束，兩個 server 的 control 都斷，傳輸失敗。現在的訊息說是 server 所走的 SSH 連線斷了（`ssh` 以 255 結束），以先離開的那一邊為原因；之前會怪到另一邊。要做的話：control 連線也以 session 的方式承載（編號、確認、重送），server 在 control 斷了之後等寬限，client 重新登入後重新接上；要不要做，由使用者決定。2026-10-25 決定不做：討論時發現 nano4 的 login node 不只一台，使用者決定只用一條 SSH 連線、斷線就失敗（[重新決定](#2026-10-25-重新決定一條-ssh-連線斷線就失敗)）。
  - 整個傳輸的可重現模擬沒有做（見上），所以以種子切斷的失敗不一定重現得出來；同一個種子只保證切斷的時機相同。
  - 在終端機上問登入的那條路，仍只有交接的單元測試，沒有在需要 OTP 的 host 上實測。
  - `tests/remote.rs` 裡一個 heartbeat 的測試，改名為「一邊加入得慢，另一邊會等」，拿掉已經沒有作用的 `FERRY_TEST_HEARTBEAT`。它檢查的是加入晚 5 秒；晚超過 20 秒時，另一邊的 session 會把還沒聽到東西的 stream 當成斷了，client 重建，而重建的又一樣慢，最後慢的那邊等滿寬限而失敗。只在 `--remote-join`、而且 host 的 shell 啟動超過 20 秒時才會遇到，沒有改。

**A2 的收回：一條 SSH 連線，斷線就失敗（2026-10-25 決定）。**

- 為什麼、拿掉什麼、留下什麼：見[重新決定](#2026-10-25-重新決定一條-ssh-連線斷線就失敗)。A2 的 17 個 commit（`4669787` 到 `5db2b03`）都還沒 push；留下的部分照用，其餘一步步拿掉，不整個 revert。
- 步驟（2026-10-25 提議，開始之前和使用者確認）：
  1. **一條 SSH 連線。** 拿掉 `--ssh-connections`（還帶著它的指令列以 2 結束）、第一條以外的 master、stream 依連線分配；stream 固定一條。dashboard 每個 host 的 SSH 連線那一區變成一列。
  2. **拿掉重連。** stream 的重建、重新登入與借用終端機、server 做完之後的等待、`Report::Broke`、`FERRY_TEST_BREAK` 與它的測試。stream 斷了，傳輸就失敗：`--cut-a-stream` 的情境與 `tests/sync.rs` 的測試改成預期失敗，原因說得清楚，Destination 不留半成品。
  3. **薄的 stream 層取代 session 與轉接。** peer 的封包直接上 stream；keepalive、沉默、收尾；拿掉 `transport::session`、驅動者裡 session 的部分、outward 與 inward、pipe、`BlocksEnd`。量 4 GiB 的單檔與 20 萬個 4 KiB 的檔案，對照收回之前（2.64 秒、1.53 秒）。
  4. **client 看 control 的沉默。** 20 秒沒收到任何東西就失敗；server 做完或失敗之後，等 control 關閉有上限。
  5. **驗收與文件。** `docs/design/`、`docs/cli.md`、CHANGELOG 的 Unreleased（A2 的幾項改寫）、`TODO.md`（Reconnecting 改成不做）、圖。三種組合的 `ferry test`；傳輸中結束 ssh 的 master，傳輸很快失敗、訊息清楚、Destination 不留半成品；讓 ssh 暫停（`SIGSTOP`），約 20 秒後失敗；nano4 上經使用者的 ControlMaster 照常複製。
- 進度（2026-10-25，同一天依序做完；每步測試全過才提交，五步做完一起推上去）：
  1. 一條 SSH 連線（`04e5e3a`）：`--ssh-connections` 拿掉，還帶著它的指令列以 2 結束；master 只剩一個，以 `%C` 命名，或使用者自己的；stream 一條；dashboard 每個 host 一列。
  2. 薄的 stream 層（`7751c30`；計畫的第三步，先做）：peer 的封包原樣寫上 stream；寫入端閒置 5 秒送 keepalive，只在兩個封包之間；peer 做完時送 stream 的結束；讀取端把兩者拿掉，20 秒沒收到東西、或沒有結束就到了盡頭，就算斷了，讀它的 peer 失敗。兩端都設成不阻塞，等空間的寫入端在 stream 斷了時會停下，pipe 上也一樣。session、驅動者、frame、server 裡轉接的兩條執行緒與 pipe、`BlocksEnd`、Hello 裡的 session ID 都拿掉。`--cut-a-stream` 的情境改成預期失敗、Destination 不變。
  3. 拿掉重連（`cc1d498`；計畫的第二步）：client 不再重建 stream、不再重新登入、不再借出終端機；`Report::Broke`、`FERRY_TEST_BREAK` 與它的測試拿掉。
  4. client 看 control 的沉默（`46dbc45`）：server 從它的 control channel 到達起，每秒說一次 stream 送收了多少；client 20 秒沒收到任何東西，就把那個 server 當成不在了，說「nothing came on its control connection for 20 s」，傳輸失敗。server 做完之後，最多等 20 秒讓 client 關閉 control。
  5. 驗收與文件：見下。
- 驗收時找到並修好的（2026-10-25）：
  - `6b28873`：經 `--remote-join` 加入的 stream，加入的行程還在時，sshd 不會關它的 channel，讀取端讀到 stream 的結束之後還在等 EOF，等到 20 秒的沉默才失敗。fatcat 上 `--remote-join` 的 `ferry test` 因此 39 個裡 31 個失敗。改成讀到 stream 的結束就結束。同時，沒能開始的 stream 會告訴 client 原因（`start the stream: …`），不再只說 server 以 1 結束。
  - `c4f7f63`：讓 Ferry 自己的 master 暫停（`SIGSTOP`）時，master 還握著 control 連線 pipe 的另一端，client 讀 control 的執行緒不會結束，傳輸卡住，405 秒後手動停掉。改成 client 放手之後，讀 control 的執行緒每 250 ms 檢查一次就停；沉默的那一側不再多等；傳輸失敗時，攜帶 stream 的 `ssh` 立刻結束。
  - 寫文件時發現：stream 的 keeper 在兩次查看之間睡著時，profile 把它算成在工作（`run`），client 找「什麼拖住傳輸」時，以及 dry run 的 dashboard，可能因此指向它。改成算等待（`wait for work`），加了測試，拿掉修正時會失敗。
- 驗收（2026-10-25）：
  - 全部 528 個測試、`cargo fmt --check`、clippy 通過。
  - `ferry test`，用最終的 build：本機，兩邊在同一處與跨兩個檔案系統，各 36 個通過、2 個略過；fatcat 三種組合（到 fatcat，直連；從 fatcat，`--remote-join`；fatcat 到 fatcat）各 39 個全過。
  - 傳輸中讓 SSH 出事（fatcat，Ferry 自己的 master，複製一個 600 MiB 的檔案，各一次，用最終的 build；Destination 都只留下空的根目錄，沒有暫存檔，也沒有開著的檔案）：`kill -9` master，0.1 秒後失敗，說「the SSH connection its server ran over dropped」；`ssh -O exit`，0.1 秒，同樣的訊息；`SIGSTOP` master，20.2 秒後失敗，說「the Destination on fatcat-own: nothing came on its control connection for 20 s」。
  - nano4 上經使用者的 ControlMaster 照常複製：推上去時沒有做，使用者的 ControlMaster 沒有開著，新的連線要 OTP。使用者開好之後，同一天補做：在 nano4 上建的 `bcb5a48`，以 `--remote-ferry` 指定；login node 是 25a-lgn04，負載 430–470。
    - 三種組合的 `ferry test`（到 nano4，直連；從 nano4，`--remote-join`；nano4 到 nano4）各 39 個全過。每個情境要 5–25 秒，fatcat 上是一兩秒：這台 login node 上經 master 跑一次 `ssh nano4 true` 就要 10–16 秒，其中 shell 的啟動檔佔 1.6–3.7 秒。
    - 1 GiB 的混合樹（`--scale 1G --kind mixed`）：到 nano4，8 項全過，83.6 MiB/s。從 nano4，複製完兩邊的雜湊相同，「the destination matches the source」卻失敗：使用者的 shell 啟動檔在前一個背景程序還拿著鎖時，會在標準輸出印一行警告；傳輸會跳過它，`ferry test` 讀 host 上 `ferry fixture` 的輸出時沒有跳過，把它算進了雜湊那一行。這是 `ferry test` 原本就有的問題，之後修了（見下）；之後的改動、同步、核對與 dry run 照 `--scale` 的步驟手動做，都照常。
    - 那次從 nano4 的複製只有 20.3 MiB/s（含啟動），ssh 本身兩個方向都有 99–117 MB/s，所以和 `d589cf6` 輪流，從 nano4 複製另一棵 1 GiB 的混合樹各 3 輪（含啟動）：最終版 15.8、26.2、13.3 秒，`d589cf6` 24.3、14.0、15.3 秒，沒有差別，雜湊都相同。20.3 MiB/s 那次是樹剛產生完就複製，之後沒有再出現，原因沒有查。
    - Ferry 沒有自己開連線（它的 runtime 目錄裡沒有 control socket），使用者的 master 從頭到尾是同一個。傳輸中讓 SSH 出事的那幾項沒有在 nano4 上做：會動到使用者的 master。
    - 順帶看到兩個之前就有的問題，使用者要修，同一天修了：
      - `0471a76`：`ferry test` 讀 host 上 `ferry fixture` 的輸出時，不像傳輸那樣跳過 shell 印的東西，上面那次從 nano4 的核對因此失敗。現在 `ferry fixture` 的輸出放在開始與結束的記號之間，和 probe 一樣，shell 印的東西跳過，並說出來。加了一個測試，假的 ssh 在每個指令前先印東西，修正之前會失敗。
      - `c9fd1e2`：使用者的 master 已經在時，Ferry 保住連線的 `ssh -N` 會向 master 要一個 session，在 host 上開一個登入 shell，跑一遍啟動檔才結束（nano4 上量到一次 15.6 秒），每執行一次 Ferry 就多一個；`hold` 看到 master 在就返回、沒有回收它，留下一個殭屍行程，到傳輸結束才回收；`hold` 若是先看到 `ssh -N` 結束、才看到 master，已回收的 pid 仍留在收到訊號時要結束的清單裡，那個 pid 若已給了別的行程，Ferry 因訊號結束時會對它送 SIGTERM。現在先問 master 在不在，在就不跑 `ssh -N`；跑了的，master 答應時若已結束（有 `ControlPersist` 時，它把 master 留在背景就結束），立刻放掉，不再登記。加了三個單元測試與一個整合測試，各自在拿掉對應的修正時失敗。`ferry_ended_by_a_signal_ends_the_ssh_holding_its_connection` 的假 ssh 原本不論 `ssh -N` 起來沒有都說 master 在，改成 `ssh -N` 起來之後才說在，照它要測的情況：`ssh -N` 本身就是 master。
      - 修好之後，用在 nano4 上建的 `c9fd1e2`，經一個記下參數再呼叫 ssh 的包裝：從 nano4 的 1 GiB 8 項全過；從 nano4、`--remote-join` 的 39 個情境全過，約 9 分鐘（之前約 13 分鐘，各只有一次，負載也不同，不能說是修正的關係）。兩次共 43 個 Ferry 行程各問一次 master 在不在，沒有一個跑 `ssh -N`。這兩次 shell 都沒有印那行警告，之前兩次 1 GiB 都有印；可能是少了那些多開的登入 shell，背景的同步少了，只有這兩次，不確定。所以第一個修正在 nano4 上沒有實際碰到，靠測試。
  - 量測（本機，tmpfs 到 tmpfs，都釘在 NUMA node 0，輪流，取中位數）：第三步對照第一步（`04e5e3a`，session 還在）：4 GiB 單檔 2.67 → 1.64 秒（各 6 輪），20 萬個 4 KiB 的檔案 1.62 → 1.48 秒（各 8 輪）。最終版對照已推上去的 `d589cf6`（A2 之前，client 轉接）：4 GiB 單檔 3.00 → 1.92 秒（各 8 輪；另一次各 6 輪，2.98 → 1.91），20 萬個小檔 1.69 → 1.49 秒（各 8 輪）。最終版的 4 GiB 比第三步那次量到的慢，所以把第三步之後的每個提交與最終版輪流再各量 6 輪：每一版最快都在 1.57–1.61 秒，中位數則在 1.63 到 2.02 秒之間，和提交的先後無關；時間分成約 1.6 秒與約 2.0 秒兩群，所以不是退步，CHANGELOG 寫成「1.6 到 2.0 秒」。量的時候主機的負載在 2 到 10 之間，來自主機上其他的工作。
- 和計畫不同的地方：
  - 第二步與第三步對調：session 拿掉之後，就沒有東西會重建 stream，重連的程式自然閒置，之後再拿掉。
  - dashboard 的佇列表也拿掉了「send」那一列（各條 stream 寫入端手上的位元組）與「back」那一行：薄層沒有自己的佇列，等著 stream 的就是資料的佇列。計畫只說 SSH 那一區變成一列。
  - 多了測試用的 `FERRY_TEST_SILENCE`，讓沉默的測試不必等 20 秒。
  - 多條 stream 剩下的通用性一起拿掉：`Report::Streams` 的一串改成一條 stream 的 `Report::Carried`；join 的標頭不再帶 stream 的編號，5 個位元組變 1 個；profile 裡已經沒有執行緒會進入的三個狀態（read from the peer、write to the peer、wait for the receiver，原本給 client 的轉接與 server 裡的轉接用）拿掉。
  - 驗收時才找到的兩個修正，以及 keeper 在 profile 裡的修正，見上。
  - fuzz 的建置，推上去之後 CI 才發現壞了：`striped` 這個 target 用的 `transport::striped`，A2 第四步（`c37f548`）就拿掉了，A2 沒有跟著改，本機也沒建過 fuzz；`destination_stream` 的 `report` 在 A2 第三步改成了 `Reporter`。`striped` 拿掉（每晚的 fuzz 也跟著拿掉），`destination_stream` 改用不回報的 `Reporter`；四個 target 都建得起來，各跑 20 秒沒有問題。stream 這一層原本沒有 fuzz target：每建一條 stream 就起一條 keeper，stream 結束之後最多還留 1.25 秒，照 fuzz 的速度會堆出上千條執行緒，讀到的切法又由 kernel 決定。2026-10-25 使用者同意之後補上：讀取端拆封包的部分抽成不做 I/O 的 `Deframer`，寫入端追蹤封包邊界的部分抽成 `Boundaries`，keeper 也改成 stream 一結束就結束，不再等到下一次查看（`55bbf53`）；新的 `stream` target 以任意位元組、任意切法（每次讀到多少、peer 每次要多少），和一個照規則算的模型對照交出的位元組與斷掉的原因，也把任意切法寫出、在兩個封包之間插進 keepalive 的封包讀回來比對，加進每晚的 fuzz。本機跑了 10 分鐘，約 135 萬次，沒有問題；重構前後輪流量 20 萬個 4 KiB 的檔案與 4 GiB 的單檔，各 6 輪，沒有變慢（中位數 1.70 → 1.54 秒、1.79 → 1.59 秒，負載升到 6.4，4 GiB 本來就分兩群，所以不算變快）。
  - macOS 上的測試，推上去之後 CI 才發現全壞了：client 把 socket pair 的兩端經由 SCM_RIGHTS 交給本機的兩個 server 之後，就關掉自己的副本（`join::hand`，A2 第四步加的）；macOS 的 unix socket 垃圾回收，會把傳送途中、要給一條還沒被 accept 的連線、又沒有行程握著的 socket 清空，server 收到的那一端就讀到 EOF。Linux 不會這樣，本機的測試都過；A2 起的提交一直沒推，沒在 macOS 上跑過。改成 server 收到之後回一個位元組，`hand` 等到它（最多 20 秒）才返回，呼叫端這時才關掉副本；`--join` 的行程本來就握到 server 做完，改成讀到 EOF 為止、忽略這個位元組。加了兩個測試（拿掉等待時都會失敗）。本機沒有 macOS，靠 CI 驗證：修正之後（`b8cf4e2`），macOS 15 arm64、26 arm64 與 26 x86_64 上 475 個測試都只剩新加的「server 不在了」那個失敗，因為 macOS 上那條連線是結束、Linux 上是被重設，測試只認得後者；改了測試之後（`48ffbe3`），三台 macOS 都全過。再下一次，macOS 26 arm64 上「panic 是內部錯誤」的測試以 1 結束，不是 101：Destination 回了位元組，接著讀到 Source 已經送來的資料、照測試的要求 panic 結束，這些都發生在 client 設定等待逾時之前，連線已經斷了，macOS 對這樣的 socket 設定逾時會回 EINVAL（Linux 不會）。改成連上之後、送出之前就設好逾時，加了一個 server 收下就關掉連線的測試。
  - 同一次 CI，xfs 上 Destination 停住的那個測試失敗：Source 的讀取端因沉默把 stream 判為斷了，但先回報的是等著寫的 pump，只說「write a packet: the stream broke」，沒說為什麼，測試找不到「nothing came」。誰先回報是競賽；改成 stream 記住第一次被判為斷掉的原因，之後寫入端與讀取端回報時都帶上它（例如「the stream broke: nothing came on the stream for 20 s」）。寫入端的測試加了這一項。
  - nano4 的驗收在推上去之後才做，見上。

**A3：每條 SSH 連線 K 個 channel（好處看主機的 TCP buffer）。**

- 2026-10-25：改列為[改進選項](#改進選項)（同一條 SSH 連線上多開 channel）。以下是原本的內容。
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

**B1 的補完：決策者不做 I/O（2026-10-24 決定，排在 A2 之前）。**

- 為什麼：A1 第五步沒有合併的兩件（收尾的錯誤與哪個目錄在等哪些目錄；以 segment 傳送的檔案的狀態），以及 B1 沒做到的「Want 比較完就送」，都要決策者先不做 I/O。A2 也動同一塊：資料不再走一條有序的串流，Destination 的完成要改成狀態上的條件，「結束時段沒到齊」的判斷跟著改；整個傳輸的可重現模擬也要它。先做這個，A2 就不必把這些改兩次。
- 做什麼：
  - 決策者改成不做 I/O 的狀態機：比較的結果成為事件，送給 Source 的訊息成為命令；root、`-x` 時 root 的 fstat、dry run 列出時的 stat 交給執行者；
  - 檔案的狀態（以 segment 傳送的檔案建立了沒有、哪些段寫了、root、和 Source 讀到的符不符）與改名的時機，從 writing 這條執行緒移進決策者；
  - 收尾的錯誤由決策者分類，哪個目錄在等底下哪些目錄也在它手上；決策者到最後一個目錄收完尾才結束；
  - Want 比較完就送，link 等每個目錄的閘門。
- 步驟（2026-10-24 與使用者確認，同一晚依序做完；每步測試全過才提交，並推上去跑 CI）：
  1. 決策者改成不做 I/O 的狀態機（`4b0bf7f`）。`src/destination/decider.rs` 保存 applier 原本保存的一切：每個目錄的進度、建立者手上或在等的段、等最後一段的各段、告訴 Source 比較到哪裡。`Decider::on(event)` 回傳命令：交給建立者、刪除線、收尾與寫入；告訴 Source 需要什麼；計數、列出、說明、某一項失敗、警告；最後，若要停止傳輸，為什麼。applier 的執行緒成了驅動者（`apply.rs`）：依原本的順序把事件交給決策者，執行每個命令，執行前先確認 Destination 沒有停止，警告除外。它原本自己做的 I/O 交給執行者：root 的檢查、比較與建立，由讀封包的執行緒在任何一段送去比較之前做；比較時開啟的 root 由 compare 執行緒保存，`-x` 時也由它取得 root 的檔案系統；只有 dry run 才需要的 stat（會取代目錄的項目、實際執行時會保留的多餘項目）由 stat 執行緒做。
  2. 收尾移進決策者（`6b116a9`）：哪個目錄在等底下哪些目錄、收尾錯誤的分類，都在決策者手上；收尾自己的那條執行緒拿掉，16 個收尾的執行者只做、只回報。決策者到最後一個目錄收完尾才結束。
  3. 分段檔案的狀態與改名的時機移進決策者（`0e79539`）：block 依到達的順序交給決策者；它保存每個需要的檔案直到位元組到達、分段檔案的各段與雜湊、給寫入者的工作（每個最多兩件）、寫好的檔案（每批 256 個，或一批的第一個進來 1 秒後；最多兩批在改名，有一批在等空間時不再交工作給寫入者）。writing 這條執行緒拿掉了，驅動者把工作交給寫入者與改名者，就像交給建立者一樣。
  4. Want 比較完就送，改名等每一段做完（`1553e86`）：一段比較完，比較時找到了它的目錄，就要它的檔案；寫好的檔案要等它那一段做完才取得名稱，那時目錄多餘的已經刪完，目錄的其他項目也已建好。
  5. 驗收，與文件、CHANGELOG。
- 驗收（2026-10-24）：
  - 全部 526 個測試、本機的 `ferry test`（兩邊都在同一處，以及跨兩個檔案系統，各 36 個情境通過、2 個略過）、fuzz 的建置都通過；每一步推上去，CI 都通過，第一步與第二步各有一次是一個既有的時間相依測試失敗（見下）。
  - 決策者的合成事件測試涵蓋整個事件迴圈，不再只是它的狀態：從 root 到結束的一次 run、目錄的各段等它的多餘刪完、dry run 依序列出、root 是檔案、Source 的根在 Destination 裡面、檔案失敗、停止時仍記下警告、刪除線遇到的錯誤、收尾失敗、分段檔案一次寫一段且照順序、雜湊不符、Source 送錯的各種情形、要求之前就到的東西依序等待、寫入者的工作上限、改名的空間與背壓、串流到每個檔案改完名才結束、block 中途斷掉、提早要的檔案等那一段做完才取得名稱、等名稱的檔案上限、目錄不在時檔案只失敗一次。
  - 決策者自己的吞吐量，以 A1 同樣的方法量（`decider_rate`，合成事件，只算決策者的時間）：20 萬個小檔，從比較到取得名稱，0.13 秒，每秒約 150 萬個。
  - 量測，本機都釘在一個 NUMA node 上，取中位數；舊版是 B1 的補完之前的 `193fb8c`，加上 relay 的修正：20 萬個 4 KiB 的檔案到 tmpfs，1.70 → 1.56 秒；一個 2 GiB 的檔案 1.53 / 1.51 秒；一個目錄刪 2 萬、寫 2 萬，tmpfs 0.405 → 0.318 秒，ext4 0.615 → 0.515 秒，NFS 17.1 / 17.0 秒；200 個目錄刪 2 萬、寫 2 萬，0.375 → 0.33 秒；5 萬個空目錄 1.04 / 1.03 秒，ext4 1.24 → 1.06 秒；20 萬個檔案加上同樣多的多餘項目的 dry run，0.375 → 0.233 秒；NFS 上 2 萬個小檔，5.97 / 6.04 秒。WekaFS 沒有量（nano4 沒有登入）。
- 和計畫不同的地方：
  - writing 這條執行緒拿掉了，不是留著「只派工」：驅動者直接把工作交給寫入者與改名者，它們向驅動者回報。
  - 決策者沒有時鐘：1 秒的計時由驅動者做，決策者說要被叫醒，時間到了驅動者再告訴它。
  - root 由讀封包的執行緒套用，在任何一段送去比較之前；首次複製時，root 因此在比較之前就建好，不再和比較同時進行。
  - 改名的閘門是「那一段做完」，不只是「多餘已刪」：一段要等它目錄的多餘刪完、目錄建好才做，做完時它的其他項目也建好了，所以一段的其他項目與它的檔案取得名稱的先後和以前一樣；不分大小寫的檔案系統上，互為別名的兩個名稱，失敗的仍是同一個。
  - Want 比較完就送，只在檔案寫成沒有名稱的地方和刪除重疊。NFS 上沒有 O_TMPFILE，檔案以暫存名稱寫在同一個目錄，和刪除搶目錄的鎖：一個目錄刪 2 萬、寫 2 萬，提早寫時要 19.3 秒，等刪完再寫 17.0 秒，各 7 輪，範圍不重疊。stat 執行緒在有多餘項目的目錄試建一次沒有名稱的檔案，決定是哪一種；macOS 一律等刪完。
  - 目錄還在建立中的段，它的檔案在目錄建好時（父目錄那一段做完）就要求，不等自己那一段做完；比較時沒找到目錄、也不在建立中的段（例如它的項目比較失敗），照舊等自己那一段做完才要求。
  - 寫好、等名稱的檔案佔著檔案描述子：最多到批次之外剩下的描述子允許的數量，至多 65,536 個；等著時不再交工作給寫入者。
  - Destination 停止之後，什麼都不再做、不再說：建立者在原因之後遇到的失敗不再回報（以前會），已完成的目錄也不再收尾（以前收尾的執行緒會把已交給它的收完）；進入時改過權限的目錄，照舊設回原本的權限。
  - 驗收時發現並修正：改名者把整個檔案交回，決策者丟掉它時，在自己的執行緒裡關檔，每個檔案一次系統呼叫；改成改名者自己放掉（`fb06dfd`），決策者自己的時間從 0.31–0.54 秒降到 0.13 秒，tmpfs 上的小檔 1.68 → 1.56 秒。
  - 另外找到的兩件，各自修好：client 的 relay 在顯示的佇列滿時固定睡 10 ms，dry run 因此時快時慢（0.35 秒，或 1.1–1.8 秒），改成一有空位就送（`fe1c23f`）；`tests/sync.rs` 一個時間相依的測試在 CI 上失敗兩次，原因是預設的 in-flight 上限比整棵樹大，掃描可以在要求任何檔案之前就送完，改成 reader 先停 5 秒、in-flight 上限 200（`a13caff`、`097ec9a`）。
  - 建立者、刪除與收尾的執行者仍是 std 的執行緒，不是 `Stages`：在行程內直接跑 Destination 時（單元測試，以及直接呼叫 `destination::run` 的 `destination_stream` fuzz target），其中一個 panic，決策者等不到它的回報，要等到時間上限才失敗；`FERRY_TEST_PANIC` 與 `FERRY_TEST_STALL` 也指定不了它們。和以前一樣，沒有改。原本這句沒寫的是：正式的程式裡，全域的 panic hook（`src/cli.rs`）在任何執行緒第一次 panic 時，就說出是哪個角色的內部錯誤，並以 101 結束行程，所以傳輸不會卡住。2026-10-25 使用者要改：三者改用 `Stages` 起，panic 會經 stop 讓決策者立刻結束；`a_panic_is_an_internal_error_that_ends_the_transfer` 加上 make、delete、finish 三個角色，改之前會失敗。
  - 決策者的執行緒現在也做 writing 原本的事：20 萬個小檔要 1.6 秒，它約有 1.0 秒在忙，大半是把工作交給閒著的寫入者時喚醒它們，以前算在 writing 的「等寫入者」裡。端到端沒有變慢；可能的改進：小檔幾個一件交給寫入者。

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

每條 SSH 連線 K 個 channel 原本也列在這裡（`docs/design/architecture.md:280-292` 已經是 Proposed）；在 fatcat 與 nano4 上量到沒有好處，高 RTT 的好處又看主機的 TCP buffer（5.2 節），所以改列為 A3。2026-10-25 再改列為[改進選項](#改進選項)。

### 不想一次動這麼多時

只想先要重連：在現在每一段 striped 連線上加累計確認、重送緩衝與重新加入，再加 server 的寬限時間（見[其他做法](#其他做法)的第二項）。不動資料格式，但 HOL、大檔一條 thread、跨次續傳都還在，之後改成 block 時，這一層會變成多餘。（2026-10-25 起不重連，不適用。）

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
| block 大小 B 與段大小 S（A1 的第一步） | 同一批檔案，依 4.2 節的規則組成 pack 與 segment，B = S，從 256 KiB 到 16 MiB；每個 block 各自以 zstd 1、3 壓縮（tar 另加 9），16 條 thread。現在的格式以 Ferry 自己的 record 與 frame 程式對照。資料：crate 的原始碼、建置產物、`/usr`、原始碼的 tar，以及四個大檔各取 1 GiB | 8 MiB 比 2 MiB：zstd 1 少 0.2–1.1%，zstd 3 少 1.5–9.3%；16 MiB 再少不到 1.4%；大檔各種大小差不到 0.2%；2 MiB 的 block 和現在的 frame 差不多 | 先定為 B = S = 8 MiB；第二步改為 2 MiB（下一列） |
| 本機的速度與 block 大小（A1 的第二步） | 接上傳輸後，第一步之前的版本（`74b5e44`）與 block 的版本輪流，tmpfs → tmpfs、不壓縮：一個 4 GiB 的檔案，各 4 輪；20 萬個 4 KiB 的檔案，各 6 到 10 輪。block 為 2、4、8 MiB 各建一個版本 | 4 GiB：舊版約 3.0 秒，8 MiB 3.2–3.5 秒，4 MiB 約 3.15 秒，2 MiB 2.8–2.9 秒。小檔：中位數舊版 3.00–3.04 秒，8 MiB 3.01 秒但有 3.4、3.7 秒的慢輪，2 MiB 2.86–3.00 秒 | B = S = 2 MiB；本機不比之前慢 |
| 本機的基準（A1） | 現在的 Ferry，20 萬個 4 KiB 的檔案與一個 4 GiB 的檔案 | 小檔：tmpfs 3.1 秒，ext4 8–9 秒（同 `cp -r` 加 `sync`）；大檔：tmpfs 3.1 秒（1.35 GB/s，一個 writer 約 95% 在忙），ext4 9.9 秒（磁碟本身 8.8 秒） | A1 的驗收要在 tmpfs 上量 |

量 lister 與 K 個 lister 時，fatcat 約有 180 人登入，nano4 的 login node load 150–185（216 核），所以同一項各輪可以差到 2 倍。表中是 3 輪的中位數或範圍。高 RTT 的模擬只有一台機器、每項一輪，結果和 window 的推算吻合到幾個百分比。

### 還沒驗證

- **真實的長距離鏈路：** 模擬只有固定的延遲、固定的速率與隨機的掉包。真實路徑上的突發掉包、重排、限速、中間設備與別人的流量都沒有。這台的 kernel 不能用 BBR，所以也沒有比較 BBR。
- **啟動的 12 個 RTT：** 是哪些往返還沒查；250 ms 時是 3.1 秒，小量的同步會被它主導。2026-10-25：一次傳輸每個遠端 host 開幾個 session、在 nano4 上一個要多久，以及只開一個的提案，見 [one-session.md](one-session.md)。
- **真實大樹的增量同步：** 合成的樹已經量了（上表）。還差一棵真實的大樹（看它的形狀，以及伺服器沒有快取時的情況），以及 B2 做出來之後整個 Ferry 的掃描。之前在 NFS 上試過平行掃描，134,000 個項目快 1.5 到 2 倍，但共用伺服器最忙時的請求加倍（`docs/design/pipeline.md:178-187`）；這次的小程式也看到請求率約加倍。
- **pack 的閒置時間、資料額度**：要等 A1 接上傳輸。B 與 S 已量（上表）。
- **真正的擁有者的吞吐量**：B1 的 applier 量了，從 profile 算忙的時間（上表）；A1 讓 block 成為事件之後，要再量一次。- 讀過核心模組：transport、mux、relay、peer、source、scan、destination 的各模組；dashboard、testkit、遠端安裝只看了結構。
