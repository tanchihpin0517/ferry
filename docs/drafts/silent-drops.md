# 連線：無聲斷線時每個等待都有上限，以及同一條 SSH 連線上的多條 channel

- 狀態：討論中：2026-10-26 使用者同意方向，並要求範圍擴大到所有的等待：每個等待都屬於一個生命週期，見[提案](#提案)；同一天決定把 redesign 的改進選項「同一條 SSH 連線上多開 channel」（原本的 A3）併進來一起做，見[第 8 節](#8-同一條-ssh-連線上的多條-channel)與[計畫](#計畫)
- 開始：2026-10-26；最後修改：2026-10-26
- 會改動：`docs/design/architecture.md` 的「Transport」原則第 10 點（健康在哪裡判斷）、「Connections」裡每種連線怎麼結束、launcher 的「它啟動的東西不依賴它」，以及 transport 對外的介面；兩者的 `.zh-TW.md`
- 相關：
  - [one-session.md](one-session.md) 的第 6、7 步：追蹤 launcher 的連線與 server 的 control connection 改走 stream 層；
  - `2204943`、`5fbdefa`：launcher 對開頭的位元組與第一個封包的時限，以及被追蹤之前出錯就結束；
  - 2026-10-23 的原則：連線的健康只在一處判斷（`docs/drafts/architecture-redesign.md` 原則 7）；
  - [architecture-redesign.md](architecture-redesign.md) 的改進選項「同一條 SSH 連線上多開 channel」，與 5.2 節高 RTT 的模擬。

## 摘要

- **目標**：Ferry 裡每一個等待都有上限，而且上限都由同一套機制給。等待包括：讀、寫、等連線進來、等對方表明身分、等子行程結束。
- **等待分四種**：等網路上的對方（由 Scope 與 Liveness 管）、等一個人（不設上限，只有 Ctrl-C）、等第三方或檔案系統的進展（工具帶進展的下限，其餘回報不逾時）、host 上 launcher 出現之前（只有 shell，兩個做法待選）。判斷的是連線活著，不是事情有進展。
- **生命週期（Scope）**：每個等待都屬於一個 Scope。Scope 排成一棵樹，父的結束了，子的跟著結束；每個 Scope 只有一個「結束了」的來源。
- **一個 host 一個 Liveness**：它的來源隨階段換手，client 這頭是登入 → 第一個 session 的 `alive` 行 → 心跳；launcher 那頭是「20 秒內要被追蹤」→ 心跳。用它的程式碼不必知道現在是哪個階段。
- **只剩三種計時**：心跳、心跳出現之前的暫時上限、收尾的寬限，定義在同一處。transport 對外只給 `Wire`、`Listener`、`Child` 三種型別，連線的 fd 不外流，所以「每個等待都帶著 Scope」由型別保證，不靠記得去補。
- **多條 channel**：一次傳輸可以有 K 條 stream，都走同一條 SSH 連線，所以和其他連線一起斷，健康不必多做任何事；要做的是 block 的順序與 K 怎麼定。高 RTT、主機的 TCP buffer 又夠大時，才比一條快。

## 問題

**無聲斷線**指連線斷了，兩端卻都沒收到關閉（沒有 FIN 或 RST），例如網路中斷、筆電休眠、NAT 的對應被清掉。這時誰在等對方，誰就一直等：

- TCP 的 keepalive 預設 7200 秒才開始探測，再 9 次各 75 秒，約 2 小時 11 分才發現；
- sshd 的 `ClientAliveInterval` 預設 0，不主動探測。fatcat 是預設值；nano4 的 `sshd_config` 一般使用者讀不到，kernel 的 `tcp_keepalive_time` 一樣是 7200；
- client 這邊，Ferry 自己開的 master 有 `ServerAliveInterval`，使用者自己的 ControlMaster 不一定有。

Ferry 目前的保證是一處一處補上的：stream 有 5 秒的 keepalive 與 20 秒的沉默；client 以 20 秒判斷每條 control connection；第 6 步讓追蹤 launcher 的連線也有。做第 6 步時才發現還有沒有時限的地方，所以要有系統地盤點一次。

**目標**：每條連線的每個階段，任何一方在等另一方時，都有一個有上限的等待；時間到了，要嘛結束，要嘛放掉那條連線，交給負責判斷健康的那條決定。2026-10-26 使用者把範圍擴大：不只連線上的讀取，等連線進來、等對方表明身分、寫、等子行程結束都算，目標是**統一整個連線健康的管理**。

讀程式碼時看到的缺口列在[附錄](#附錄已知的缺口)。

## 提案

2026-10-26 提出，使用者同意方向；第一版只以 host 的心跳管已建立連線上的讀取，使用者指出範圍不足，擴大成下面的樣子。

### 1. 等待的種類

2026-10-26 對照程式碼檢查設計時發現：把所有等待都當成「等網路上的對方」，只給得出一種答案（Liveness）。實際上有四種，答案各不相同。**Scope 與 Liveness 判斷的是連線活著，不是事情有進展。**

![等待的四種：等網路上的對方，由 Scope 與 Liveness 管；等一個人，不設上限，只有 Ctrl-C；等第三方或檔案系統的進展，工具帶進展的下限，其餘回報不逾時；host 上 launcher 出現之前，只有 shell，做法 A 或 B 待選](../assets/silent-drops-kinds.svg)

| 種類 | 例子 | 怎麼處理 |
|---|---|---|
| 1. 等網路上的對方 | 連線上的讀寫、accept、對方表明身分 | Scope 與 Liveness，下面第 2 到 8 節 |
| 2. 等一個人 | 密碼、OTP；沒有 `--yes` 時問要不要安裝 | **不設上限**，只有 Ctrl-C 結束它；其他已經有心跳的 host 照常 |
| 3. 等第三方或檔案系統的進展 | 下載 release、cargo 抓 crate、NFS 卡住、sudo 的 PAM 等輸入、peer 死結 | Ferry 啟動的工具帶上**進展的下限**：host 下載時的 curl 已經有 `--connect-timeout 10 --speed-limit 1024 --speed-time 30`，wget 有 `-T 30`，cargo 有自己的 http 逾時；**client 自己抓 release 的 curl（`src/remote/release.rs`）沒有，要補上一樣的**。其餘的不逾時，靠現有的「60 秒沒有進展就說出每個 thread 在哪裡」回報，和今天一樣 |
| 4. host 上 launcher 出現之前 | 第一個 session 的 shell 等 client 的下一個指令、等上傳 | 那時 host 上還沒有 Ferry，只有 shell，沒有任何 Liveness；見下面的兩個做法，之後再決定 |

#### 第 4 種的兩個做法（之後再決定）

**做法 A：shell 的看門狗。** probe 腳本在背景起一個子 shell，例如 `(sleep 20; kill $$) &`，每收到一行指令就砍掉重起；exec 成 launcher 之前停掉它。上傳是一個指令讀很多位元組，中間沒辦法重新計時，所以要嘛 client 分段送（每段一個指令，每段重新計時），要嘛看門狗在上傳時改看收到的檔案有沒有在變大。

- 好處：流程不變，client 仍然一步一步決定。
- 壞處：只能用 POSIX sh 寫，計時粗；上傳要另外處理；沒有 `--yes` 時，client 問使用者要不要安裝的那段（第 2 種，不設上限）host 還是在等，看門狗會在使用者回答之前把 session 結束掉，所以那段要另外處理，例如 client 問完才送下一個指令前先告訴 host 暫停計時，或者 client 先問、再開 session。

**做法 B：調整流程，讓 host 盡量不等 client。** client 在第一個 session 的腳本裡，就把決定需要的都給 host：要哪個 build、可不可以安裝（`--yes`，或使用者已經答應）、能不能自己下載。host probe 完自己決定：已經裝好就直接 exec 成 launcher；沒裝、可以裝、又能自己下載，就自己下載、裝好、exec 成 launcher；只有要上傳（dev build 的原始碼，或下載失敗）時才等 client。

- 好處：等待少，也少一來一回，一次啟動更快。
- 壞處：上傳那段還是在等，仍要做法 A 的上傳處理；沒有 `--yes` 時，要在開 session 之前就問使用者，那時 client 還不知道 host 有沒有裝好，可能問了不必問的；不想先問的話，就得 probe 完讓 session 結束，問過再開一個，多一個 session，和 one-session 的目標相反。

兩個做法可以合用：B 減少等待，A 管剩下的上傳。

#### 其他對照時找到的

- **本機把 stream 交給 server（`hand`）**：現在等本機 server 回應最多 20 秒（`TAKING`）。本機 server 是 client 的子行程，傳輸的 Scope 不會因時間而結束，所以拿掉 `TAKING` 的話，本機 server 卡住時會一直等。兩個做法之後再決定：保留它，歸成「表明身分之前的暫時上限」；或者明寫本機 server 卡住屬於第 3 種，不逾時。
- **`--remote-join` 時 host 上的加入行程**：它由 sshd 的 session 啟動，不是 launcher 啟動的；它把自己的輸入輸出交給 launcher 或 server，然後等對方放開它握著的 socket。所以它跟著它加入的那個 Scope 結束：launcher 或 server 一結束，socket 就關，它也就結束。下面的 Scope 樹要把它算進去。

### 2. 每個等待都屬於一個生命週期（Scope）

![Scope 的樹：client 端是傳輸、每個 host、那個 host 上的每條連線與子行程；host 端是 launcher 與它啟動的程式；兩邊以心跳相連](../assets/silent-drops-scopes.svg)

- **client 端**：一次傳輸是根；底下是每個 host；每個 host 底下是它的連線（第一個 session、追蹤 launcher 的那條、每條 control、stream 的一半）與承載它們的 `ssh` 子行程。本機的 server 直接掛在傳輸底下：它是 client 的子行程，client 不在了管道就關，不需要 Liveness。
- **host 端**：launcher 是根；底下是它的 listener（等連線進來）與它啟動的程式（server、sudo 的檢查、fixture）。`--remote-join` 時 host 上的加入行程不是 launcher 啟動的，但它等的是它加入的那個（launcher 或 server）放開 socket，所以算在那個 Scope 底下（第 1 節）。
- **規則只有三條**：
  1. 每個 Scope 只有一個「結束了」的來源：傳輸是 Ctrl-C、失敗、或任何一個 host 的 Scope 結束；host 是它的 Liveness；launcher 是它暫時的上限或心跳；一條連線是它的父 Scope，或它自己的結尾。
  2. 父的結束了，子的跟著結束；子行程在結束前有一段收尾的寬限。
  3. 每一個會阻塞的操作都帶著一個 Scope，只因兩種原因返回：等的事發生了，或 Scope 結束了（附原因）。

### 3. 一個 host 的 Liveness，來源隨階段換手

![一個 host 的 Liveness 在各階段的來源：登入時是 ssh 自己的上限；第一個 session 是 alive 行；launcher 說出位置後是 20 秒內要被追蹤；之後是心跳。它說不在了的時候，client 端那個 host 上的每個等待都返回，launcher 結束它啟動的程式](../assets/silent-drops-liveness.svg)

| 階段 | client 看那個 host 的 Liveness | launcher 自己的 Liveness |
|---|---|---|
| 登入 | ssh 的 `ConnectTimeout`；等 ControlMaster 的 30 秒 | 還沒有 launcher |
| 第一個 session（probe、安裝） | 腳本在背景每 5 秒印一行加框的 `alive`，20 秒沒收到就算斷；在 host 上建 dev build 的那幾分鐘也有。exec 成 launcher 之前停掉它 | 還沒有 launcher |
| launcher 說出位置之後、被追蹤之前 | 等追蹤的那條連線的 start mark，上限 20 秒 | 20 秒內要被追蹤 |
| 被追蹤之後 | 心跳：5 秒送一次，20 秒沒收到就算斷 | 同一個心跳 |

Liveness 說不在了：client 那個 host 上的每個等待都返回並說出原因，傳輸失敗；launcher 先送 SIGTERM 給它啟動的程式，讓它們像收到 Ctrl-C 一樣收拾，寬限到期就砍掉，然後自己結束。

一個 host 只有一條 SSH 連線（2026-10-25 定的），所有 channel 走同一條 TCP，網路靜靜斷掉時一起斷，所以一個心跳就判斷得了那個 host 上的所有連線。

### 4. 等待一律經過 transport 的三種型別

![一個等待：Wire 的讀寫、Listener 的 accept、Child 的 wait，都同時等兩件事：等的事，或 Scope 結束；前者返回結果，後者返回結束的原因。整個程式只有三種計時，定義在同一處](../assets/silent-drops-wait.svg)

| 型別 | 做什麼 | 等什麼 |
|---|---|---|
| `Wire` | 一條連線：讀寫封包，也讀原始位元組（shell 的雜訊、記號、握手、秘密）；正常結尾的標記是一種封包，讀到就是結尾，用來分辨正常結束與被切斷 | 資料、空間、對方的結尾，或 Scope 結束 |
| `Listener` | 等連線進來，並等它表明身分（開頭的位元組、port 的秘密）；還沒表明身分的連線不屬於任何 host，掛在 listener 底下，用暫時的上限 | 連線、身分，或 Scope 結束 |
| `Child` | 承載連線的行程與啟動的程式 | 它結束，或 Scope 結束；後者給它收尾的寬限，到期就砍 |

transport 對外只提供這三種型別，連線的 fd 不外流，所以沒有人能繞過 Scope 直接讀寫。怎麼接上（`ssh -W`、loopback port 加秘密、`--remote-join`、本機的管道或 socket pair）只在建立 `Wire` 時不同，之後都一樣。

### 5. 所有的等待，與它的上限從哪來

| 等待 | 例子 | 上限從哪來 |
|---|---|---|
| 讀 | 第一個 session、start mark、握手、control、stream、sudo 的檢查、fixture | 所屬 host 的 Liveness |
| 寫 | 對方不讀，或連線已死但 TCP 還沒發現 | 同上 |
| 等連線進來 | launcher 等被追蹤；之後的每條連線；server 等 stream | launcher：暫時的上限，之後是心跳；server：它自己的 Scope，隨 launcher 或 client 結束它的 control 而結束 |
| 等對方表明身分 | 連線開頭的位元組、port 的秘密 | listener 底下的暫時上限 |
| 等子行程結束 | `ssh -W`、本機的 server、launcher 啟動的程式 | 父 Scope 結束後的收尾寬限 |
| 登入 | ssh 本身 | ssh 的 `ConnectTimeout`；等 ControlMaster 的 30 秒 |
| 等一個人 | 密碼、OTP、要不要安裝 | 不設上限，只有 Ctrl-C（第 1 節第 2 種） |
| 等第三方或檔案系統 | 下載、cargo、NFS、PAM、死結 | 工具帶進展的下限；其餘回報、不逾時（第 1 節第 3 種） |
| host 上 launcher 出現之前 | shell 等下一個指令、等上傳 | 做法 A 或 B，待決定（第 1 節第 4 種） |

### 6. 整個程式只剩三種計時

1. **心跳**：5 秒送一次，20 秒沒收到就算斷。
2. **暫時的上限**：心跳出現之前，或連線表明身分之前，20 秒。
3. **收尾的寬限**：Scope 結束後給子行程的時間，取代現在的 `ENDING`（5 秒）、`GIVEN`（5 秒）、`LINGER`（30 秒）、`close_within` 等。

都定義在同一處；測試以 `FERRY_TEST_SILENCE` 之類的注入縮短。

### 7. 會拿掉的

- stream 層的 keeper thread、keepalive、沉默判斷；Deframer 裡拆 keepalive 的那段；`Ends.before` 與換手時的緩衝：資料裡不再混著 keepalive，原始位元組與封包是同一串，不必換手。
- `follow.rs` 對 control 的判斷、server 的 control stream、`Carried::let_go`。
- 各處各自的時限：launcher 與 `accept` 的 `Silent`、`Within`，`hand` 的 `TAKING`，以及上面第 5 點列的收尾常數。

### 8. 同一條 SSH 連線上的多條 channel

原本是 redesign 的改進選項（原本的 A3）；2026-10-26 使用者決定和上面一起做。

![多條 channel：K 條 stream 與追蹤 launcher 的那條，都是同一條 SSH 連線、同一條 TCP 的 channel。第 0 條帶 block 以外的一切，block 交給佇列最短的一條；Destination 依序號把 block 放回，緩衝以 data 額度為上限，再交給 decider；第 0 條上依賴 block 的訊息等它。它們一起斷，一個心跳就涵蓋；但心跳會排在它們的資料後面，要量過](../assets/silent-drops-channels.svg)

- **為什麼要**：一條 channel 每個 RTT 最多送 2 MiB（OpenSSH 的 channel window）。redesign 5.2 節的模擬中，RTT 120 ms 約 17 MB/s、250 ms 約 8 MB/s；主機用預設的 TCP buffer 時，多開 channel 最多快 ×1.35，buffer 調大時接近 ×K。fatcat、nano4 這種短路徑上沒有好處。掉包的路徑上也沒有幫助，那要多條 TCP 連線，2026-10-25 決定不做。
- **怎麼做**：client 為一次傳輸建 K 條 stream，每條都是那個（或那兩個）host 的 Scope 底下的一條 `Wire`；兩個 server 的 pump 把 block 交給佇列最短的一條，其他所有封包（manifest、needs、results、額度、控制訊息）固定走第 0 條。短路徑上 K=1，就是現在的樣子。
- **健康**：K 條都走同一個 master、同一條 TCP，一定落在同一台、一起斷，所以由同一個心跳管，不必多做任何事。任何一條讀到沒有結尾標記的盡頭，就算被切斷，傳輸失敗，不需要確認與重送。堵塞（window 滿了、等空間）只在 Scope 結束時返回，沒有另外的計時器，慢但沒斷的鏈路不會被誤判。
- **順序（要先處理）**：現在 Destination 假設 block 照送出的順序到：解壓縮後由一個 thread 依序交給 decider（`src/destination/mod.rs`），協定裡沒有 block 的序號。K 條時 block 會亂序到，也可能和第 0 條上依賴它的訊息錯開，例如分段大檔的雜湊（`file hash`）比它最後的段先到。預定的做法：block 帶序號，Destination 收到後依序號放回再交給 decider，緩衝以 data 額度為上限（在途的 block 本來就不超過額度），decider 不用改；第 0 條上依賴 block 的訊息逐一盤點，各自等到它依賴的 block 放回之後才處理。盤點的結果寫回這裡。
- **K 怎麼定**：握手時量 RTT，例如每 20 ms 一條、最多 8 條；先以一個測試用的注入固定 K，量過再定公式。不給使用者選項：`--ssh-connections` 已經拿掉，這次也不加。
- **和心跳的交集**：心跳走追蹤 launcher 的那條 channel，各 channel 的 window 各自算，但最後都排在同一條 TCP 的送出佇列裡。多條 channel 只在 TCP buffer 調大時有用，那正是佇列最長的時候：例如 64 MiB 的 buffer 跑在 8 MB/s 的鏈路上，排在後面的心跳約要等 8 秒，仍小於 20 秒，但餘裕變小。用 5.2 節的 TUN 模擬（RTT 2、30、120、250 ms，buffer 用預設與 64 MiB）量心跳的延遲，再定 20 秒夠不夠（取捨第 3 點）。

### 取捨

1. **launcher 會結束它啟動的程式。** one-session 原本的原則是「它啟動的東西不依賴它」。2026-10-26 使用者同意改。
2. **行程被暫停（SIGSTOP）不再由沉默發現。** 網路沒斷，只是 server 或 `ssh -W` 被暫停時，host 的心跳照常。可以讓 launcher 以 `waitpid` 的 `WUNTRACED` 說出 server 被暫停了，client 對自己的 ssh 子行程也一樣。死結兩種設計都抓不到（keepalive 是另一個 thread 送的），靠現有的「60 秒沒有進展就說出每個 thread 在哪裡」。
3. **心跳與資料走同一條 TCP。** 慢的鏈路上，大量的資料可能讓心跳排隊幾秒；多條 channel 時更明顯（[第 8 節](#8-同一條-ssh-連線上的多條-channel)）。20 秒的餘裕應該夠，在 TUN 模擬與 nano4 上量過再定。
4. **改動大。** 所有建立連線、讀寫、accept、等子行程的地方都要改成帶 Scope；換來的是「有沒有上限」由型別保證。2026-10-26 使用者允許大幅重構，目標是程式簡潔、邏輯清楚、容易維護。

## 其他做法

- **每條連線都自帶 keepalive，帶不了的階段各加時限。** 沿 one-session 第 6、7 步的方向。好處是連線之間互不牽連；壞處是時限散在十幾處，每加一種連線或一個階段都得記得補一個，這次就是這樣漏掉的。不採用。
- **只以 host 的心跳管已建立連線上的讀取**（2026-10-26 的第一版）。等連線進來、等對方表明身分、寫、等子行程結束、心跳出現之前，仍各是特例。使用者指出範圍不足，改成上面的。

## 待決定的問題

- [x] **健康在哪裡判斷。** 2026-10-26 使用者同意：以 host 為單位，見[提案](#提案)。
- [x] **launcher 結束它啟動的程式。** 2026-10-26 使用者同意。
- [x] **範圍。** 2026-10-26 使用者要求：統一所有的等待，不只連線上的讀取。
- [ ] **被暫停的行程要不要回報**（取捨第 2 點）：建議要，以 `WUNTRACED`。
- [ ] **心跳的時間**：沿用 5 秒與 20 秒，還是在 nano4 上量過再定（取捨第 3 點）。
- [ ] **安裝的那幾分鐘**：`alive` 行由腳本的背景迴圈印，夠不夠；cargo 的輸出走標準錯誤，不影響。
- [ ] **收尾的寬限**：一個值，還是依子行程不同，例如 server 收拾暫存檔要多久。
- [ ] **host 上 launcher 出現之前怎麼辦**（第 1 節第 4 種）：做法 A（shell 的看門狗）、做法 B（調整流程，讓 host 盡量不等），或兩者合用。
- [ ] **本機的 `hand`**：保留 20 秒當作表明身分前的暫時上限，還是明寫本機 server 卡住屬於第 3 種、不逾時（第 1 節）。
- [x] **多條 channel 要不要一起做。** 2026-10-26 使用者決定：一起做，整合在同一份計畫。
- [ ] **block 的順序**：預定 block 帶序號、Destination 依序號放回；第 0 條上依賴 block 的訊息有哪些、各自怎麼等，要先盤點（[第 8 節](#8-同一條-ssh-連線上的多條-channel)）。
- [ ] **K 的公式**：每 20 ms 一條、最多 8 條只是起點，量過再定。

## 計畫

每一步結束時，都要能執行、測試過。先做連線的健康，讓 stream 層變薄、每個等待都有 Scope；多條 channel 再長在上面：那時 stream 只剩封包與結尾標記，K 條只是 K 條 `Wire`。

1. **量測的基準。** 用 redesign 5.2 節的 TUN 模擬，量現在一條 channel 在 RTT 2、30、120、250 ms、buffer 預設與 64 MiB 下的速度，以及一條「心跳」（另一條 channel 上每 5 秒一個小封包）在大量資料下的延遲，當作之後的對照。
2. **`Scope` 與三種計時。** 只有型別與常數，以 `FERRY_TEST_*` 縮短；還沒接上。同一步把 client 抓 release 的 curl 補上和 host 一樣的逾時與速度下限（第 1 節第 3 種）。
3. **`Wire`、`Listener`、`Child`。** 先包住現有的連線，行為不變；transport 之外不再拿得到連線的 fd。
4. **host 的 Liveness。** client 端依序是登入、第一個 session 的 `alive` 行、心跳；launcher 端是 20 秒內要被追蹤，然後是心跳；心跳斷了，launcher 對它啟動的程式送 SIGTERM，寬限到期就砍。host 上 launcher 出現之前的等待，照第 1 節選定的做法。等一個人（密碼、OTP、要不要安裝）不設上限，測試確認那時其他 host 的心跳照常。
5. **每個等待改經 Scope。** 拿掉 stream 層的 keepalive 與沉默判斷、`follow.rs` 對 control 的判斷、各處各自的時限與收尾常數。每種等待各一個「不傳也不關」或「不來」的測試，先寫出會卡住的。
6. **健康的文件與驗收。** architecture 的原則第 10 點、Connections、launcher；CHANGELOG；本機、fatcat、nano4 的 `ferry test`，與第 1 步的心跳延遲再量一次。
7. **block 的順序。** 盤點 Destination 對順序的假設；block 帶序號，Destination 依序號放回；第 0 條上依賴 block 的訊息各自等。這時 K 仍是 1，行為不變；以亂序送 block 的測試確認。
8. **K 條 stream。** client 建 K 條；pump 把 block 交給佇列最短的一條，其他固定走第 0 條；任何一條被切斷就整個失敗。先以測試用的注入固定 K，測試 K=1、2、8，與其中一條被切斷。
9. **K 怎麼定。** 握手時量 RTT；用 TUN 模擬量各 K 的速度與心跳的延遲，定 K 的公式與 20 秒夠不夠。
10. **多條 channel 的文件與驗收。** architecture 的 Transport（原則第 4 點「只有一條 stream」）與 Connections；redesign 的改進選項標成已做；CHANGELOG；本機、fatcat、nano4，與 TUN 模擬。

## 附錄：已知的缺口

以下是 2026-10-26 讀程式碼時看到的，**不是完整的盤點**，也還沒逐一以測試確認：

1. **launcher 被追蹤之前，一條連線都沒來。** launcher 在第一個 session 上說出 socket 在哪裡之後，在 `Listening::accept`（`src/transport/join.rs`）裡等連線進來，`poll` 沒有逾時。若 SSH 連線此時靜靜斷了，client 的追蹤 channel 永遠到不了；launcher 看著第一個 session 的輸出，但 sshd 不會關它。`5fbdefa` 的時限要等連線進來才開始算，幫不上。被追蹤之後沒有這個問題：追蹤那條連線的 reader 在另一個 thread，20 秒沒聲音就結束整個 process。
2. **server 等 `Hello` 與 `Proceed`。** server 在標準輸入上讀握手的 `Hello`，以及之後的 `Proceed`，都沒有時限（`src/peer.rs` 的 `serve`）。第 7 步只把握手**之後**的 control 改走 stream 層，握手本身與它之前仍要另外處理。
3. **server 等 stream 進來。** server 收到 `Proceed` 之後在 accept 裡等 stream。現在 server 不判斷 control 的沉默，靜靜斷線時會一直等；第 7 步之後，control 的 reader 20 秒沒聲音就讓 server 結束，應該就補上了，要以測試確認。
4. **loopback port 上讀秘密。** `Listening::prove` 在自己的 thread 裡以 `read_exact` 讀秘密，沒有時限。一條不說話的連線會一直佔著 16 個名額（`UNPROVEN`）之一；名額用完，之後的連線都會被拒。
5. **client 端的幾個等待**，都經過 `LetGo`，它只看連線有沒有被放掉，沒有時限：
   - 讀第一個 session 的輸出：probe、安裝、launcher 說出位置的那幾行（`FirstSession::read_part`）；
   - 開一條 channel 後等 start mark（`Transport::connect` 裡的 `start::skip`）；
   - 握手時等 `Welcome`（`handshake::client_handshake`）；
   - launcher 跑到結束的程式（sudo 的檢查、fixture），讀到它的輸出結尾（`Launcher::run`）。

已有時限的，供對照：登入時等一個不回應的 ControlMaster（30 秒）；把 stream 的一端交給本機的 server（`hand`，20 秒）；stream 的讀寫；client 判斷每條 control connection；第 6 步之後，追蹤 launcher 的連線兩端；launcher 讀連線開頭的位元組與第一個封包，server 讀 stream 開頭的位元組（`2204943`）。
