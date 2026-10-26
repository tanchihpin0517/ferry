# 網路無聲斷線：每條連線的每個階段都要有逾時

- 狀態：草稿（2026-10-26 建立，還沒討論；使用者決定先把 one-session 做完，再回來處理）
- 開始：2026-10-26；最後修改：2026-10-26
- 會改動：`docs/design/architecture.md` 的「Transport」原則第 10 點（健康在哪裡判斷），以及「Connections」裡每種連線怎麼結束；兩者的 `.zh-TW.md`
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

還沒寫，先定盤點的方法：

1. **列出每種連線**：SSH 登入、第一個 session、追蹤 launcher 的連線、啟動用的每條連線、server 的 control connection、stream、`--remote-join` 經 shell 加入的連線、loopback port 的連線、本機交給 server 的 socket pair、本機 server 的管道。
2. **每種連線分階段**：建立、開頭的位元組或秘密、第一個封包、握手、傳輸中、結尾。
3. **每一格寫下**：誰在等誰、現在的時限（沒有就寫沒有）、時間到了怎麼辦、由哪條連線判斷健康。
4. **每個缺口**，以會讓那一段「不傳也不關」的假 ssh 或假連線重現，例如 one-session 第 6 步的 `stalling_ssh`：先寫出會卡住的測試，再補上時限。

初步的原則（待討論）：**有負責判斷健康的那條連線之前，每個等待各自有時限；有了之後，其他連線出錯只處理那條連線，生死由那條決定。** launcher 在被追蹤之前出錯就結束（`5fbdefa`）就是照這個原則。

## 其他做法

還沒寫。

## 待決定的問題

- [ ] 時限都用 stream 的 `SILENCE`（20 秒），還是依階段不同？例如安裝會在 host 上建 dev build，要好幾分鐘，那段的「沉默」怎麼算。
- [ ] client 端的等待要不要各自加時限，還是靠 ssh 的 `ServerAliveInterval`？後者只在 Ferry 自己的 master 上有，使用者的 ControlMaster 不一定設。
- [ ] 這份草稿與 2026-10-23 的「健康只在一處判斷」怎麼對齊：被追蹤之前的那些時限，算不算另外的計時器。

## 計畫

還沒排。使用者 2026-10-26 決定：先把 one-session 的第 7 步做完，再回來處理這份。
