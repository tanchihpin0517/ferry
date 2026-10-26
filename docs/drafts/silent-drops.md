# 網路無聲斷線：每條連線的每個階段都要有逾時

- 狀態：討論中：2026-10-26 使用者同意方向，健康以 host 為單位判斷，見[提案](#提案)；細節與計畫待排
- 開始：2026-10-26；最後修改：2026-10-26
- 會改動：`docs/design/architecture.md` 的「Transport」原則第 10 點（健康在哪裡判斷）、「Connections」裡每種連線怎麼結束，以及 launcher 的「它啟動的東西不依賴它」；兩者的 `.zh-TW.md`
- 相關：
  - [one-session.md](one-session.md) 的第 6、7 步：追蹤 launcher 的連線與 server 的 control connection 改走 stream 層；
  - `2204943`、`5fbdefa`：launcher 對開頭的位元組與第一個封包的時限，以及被追蹤之前出錯就結束；
  - 2026-10-23 的原則：連線的健康只在一處判斷（`docs/drafts/architecture-redesign.md` 原則 7）。

## 問題

**無聲斷線**指連線斷了，兩端卻都沒收到關閉（沒有 FIN 或 RST），例如網路中斷、筆電休眠、NAT 的對應被清掉。這時誰在等對方，誰就一直等：

- TCP 的 keepalive 預設 7200 秒才開始探測，再 9 次各 75 秒，約 2 小時 11 分才發現；
- sshd 的 `ClientAliveInterval` 預設 0，不主動探測。fatcat 是預設值；nano4 的 `sshd_config` 一般使用者讀不到，kernel 的 `tcp_keepalive_time` 一樣是 7200；
- client 這邊，Ferry 自己開的 master 有 `ServerAliveInterval`，使用者自己的 ControlMaster 不一定有。

Ferry 目前的保證是一處一處補上的：stream 有 5 秒的 keepalive 與 20 秒的沉默；client 以 20 秒判斷每條 control connection；第 6 步讓追蹤 launcher 的連線也有。做第 6 步時才發現還有沒有時限的地方，所以要有系統地盤點一次。

**目標**：每條連線的每個階段，任何一方在等另一方時，都有一個有上限的等待；時間到了，要嘛結束，要嘛放掉那條連線，交給負責判斷健康的那條決定。

### 已知的缺口

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

## 提案

2026-10-26 提出，使用者同意方向。

### 現在的「stream 層」不是給資料用的

`src/transport/stream.rs` 做的是通用的四件事：封包的分界、5 秒一次的 keepalive、20 秒的沉默判斷、正常結尾的標記，它不懂資料；one-session 的第 6、7 步就把追蹤 launcher 的連線與 control connection 套上它。它和傳輸用的 stream 同名，所以看起來像是給資料用的。

問題在於**健康是每條連線各自判斷的**：每條連線要自己帶 keepalive 與沉默判斷，帶不了的階段（shell 的輸出、握手、秘密、sudo）只能一個一個另外補時限。上面列的缺口全落在這些階段；照這個方向補，時限只會越來越分散。

### 以 host 為單位判斷健康

一個 host 只有一條 SSH 連線（2026-10-25 定的），所有 channel 走同一條 TCP：網路靜靜斷掉時，它們一起斷，不會只斷一條。所以**每個 host 一個心跳，就判斷得了那個 host 上的所有連線**。

分三層，各管一件事：

| 層 | 管什麼 | 不管什麼 |
|---|---|---|
| **Carrier**（怎麼接上） | `ssh -W`、loopback 的 port 加秘密、`--remote-join`、本機的管道或 socket pair；產出兩個 fd，與承載它的行程（若有） | 內容、健康 |
| **Wire**（一條連線） | 讀寫封包，也讀原始的位元組（shell 的雜訊、記號、握手、秘密）；**每一個等待都有上限**：等到資料、對方的結尾，或它所屬 host 的 Liveness 說不在了 | 自己不送 keepalive、不計時 |
| **Liveness**（一個 host 的健康） | 每個 host 一個：client 與 launcher 之間追蹤用的那條連線上的雙向心跳，5 秒送一次，20 秒沒收到就算斷；對外只提供一個能 poll 的「不在了」與原因 | 個別連線的內容 |

- **client**：每個 host 一個 `Host`，帶著它的 Liveness；那個 host 上的每條 Wire，等待時同時看資料與它。第一個 session 的讀取、等 start mark、握手、control、sudo 的檢查、fixture、結尾的等待，都因此有了上限，不必各處各加。
- **host**：launcher 是那個 host 的健康判斷者。心跳斷了，它結束自己，**與它啟動的一切**：先送 SIGTERM，讓 server 像收到 Ctrl-C 一樣收拾後停下。server 本身不判斷健康，也不送 keepalive。
- **本機**：本機的 server 是 client 的子行程，沒有網路；client 不在了，管道就關。不需要心跳。
- **心跳出現之前**，只剩三段，各自處理：
  1. 登入：照舊，ssh 的 `ConnectTimeout` 與等 ControlMaster 的 30 秒；
  2. 第一個 session：腳本在背景每 5 秒印一行加框的 `alive`，client 以 20 秒為限；在 host 上建 dev build 的那幾分鐘也就有了心跳。腳本 exec 成 launcher 之前先停掉它；
  3. launcher 說出位置之後、被追蹤之前：20 秒沒被追蹤就結束。上面第 1 個缺口因此補上。
- **正常結尾的標記保留**：它是一種封包，讀到就是結尾，用來分辨正常結束與被切斷。loopback port 上的秘密，因為連上來的可能不是 client，另有固定的上限。

會拿掉的：stream 層的 keeper thread、keepalive、沉默判斷；Deframer 裡拆 keepalive 的那段；`Ends.before` 與換手時的緩衝（資料裡不再混著 keepalive，原始位元組與封包是同一串，不必換手）；`follow.rs` 對 control 的判斷、server 的 control stream；launcher 與 `accept` 各自的 20 秒（`Silent`、`Within`），交給 Wire 的上限。

### 取捨

1. **launcher 會結束它啟動的程式。** one-session 原本的原則是「它啟動的東西不依賴它」。2026-10-26 使用者同意改。
2. **行程被暫停（SIGSTOP）不再由沉默發現。** 網路沒斷，只是 server 或 `ssh -W` 被暫停時，host 的心跳照常。可以讓 launcher 以 `waitpid` 的 `WUNTRACED` 說出 server 被暫停了，client 對自己的 ssh 子行程也一樣。死結兩種設計都抓不到（keepalive 是另一個 thread 送的），靠現有的「60 秒沒有進展就說出每個 thread 在哪裡」。
3. **心跳與資料走同一條 TCP。** 慢的鏈路上，大量的資料可能讓心跳排隊幾秒；20 秒的餘裕應該夠，做好後在 nano4 上量。

## 其他做法

- **每條連線都自帶 keepalive，帶不了的階段各加時限。** 沿現在的方向。好處是連線之間互不牽連；壞處是時限散在十幾處，每加一種連線或一個階段，都得記得補一個，這次就是這樣漏掉的。不採用。

## 待決定的問題

- [x] **健康在哪裡判斷。** 2026-10-26 使用者同意：以 host 為單位，見[提案](#提案)。
- [x] **launcher 結束它啟動的程式。** 2026-10-26 使用者同意。
- [ ] **被暫停的行程要不要回報**（取捨第 2 點）：建議要，以 `WUNTRACED`。
- [ ] **心跳的時間**：沿用 5 秒與 20 秒，還是在 nano4 上量過再定（取捨第 3 點）。
- [ ] **安裝的那幾分鐘**：`alive` 行由腳本的背景迴圈印，夠不夠；cargo 的輸出走標準錯誤，不影響。
- [ ] 原本列的「時限都用 `SILENCE`，還是依階段不同」「client 端的等待要不要各自加時限」：在這個提案下，心跳出現之後都交給 Liveness，只剩心跳出現之前的三段與 loopback 的秘密要各自定。

## 計畫

方向同意後再細排；大致的步驟：

1. 建 `Wire` 與 `Liveness`，先在 client 端套到現有的連線上，現有的 keepalive 暫時留著；
2. launcher 的心跳，與心跳斷了時結束它啟動的程式；
3. 第一個 session 的 `alive` 行，與 launcher 等待被追蹤的上限；
4. 拿掉各連線自帶的 keepalive，與各處另加的時限；
5. 以「不傳也不關」的假 ssh 逐個階段測試，每個缺口都先寫出會卡住的測試；文件；本機、fatcat、nano4 的驗收。
