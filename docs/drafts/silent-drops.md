# 連線與生命週期：monitor、失去連線時的收拾，與多條 channel

- 狀態：已做完（2026-10-26）：計畫的十一步都做了，見[進度](#進度)；第 7 步量到心跳會排在資料後面好幾秒，使用者決定從對方來的任何東西都算心跳（已做），限制在途資料量列為 redesign 的改進選項；沒有 push
- 開始：2026-10-26；最後修改：2026-10-26
- 會改動：`docs/design/architecture.md` 的「Transport」原則第 4 點（只有一條 stream）與第 10 點（健康在哪裡判斷）、「Connections」裡每種連線怎麼結束、launcher 的部分（改名為 monitor；拿掉「它啟動的東西不依賴它」），以及 transport 對外的介面；`pipeline.md` 裡的 launcher；以上各處的 `.zh-TW.md`
- 相關：
  - [one-session.md](one-session.md) 的第 6、7 步：追蹤 launcher 的連線與 server 的 control connection 改走 stream 層；
  - `2204943`、`5fbdefa`：launcher 對開頭的位元組與第一個封包的時限，以及被追蹤之前出錯就結束；
  - 2026-10-23 的原則：連線的健康只在一處判斷（`docs/drafts/architecture-redesign.md` 原則 7）；
  - [architecture-redesign.md](architecture-redesign.md) 的改進選項「同一條 SSH 連線上多開 channel」（原本的 A3），與 5.2 節高 RTT 的模擬。

這份草稿把現在的 launcher 稱為 **host 的 monitor**：照決定，它在實作時改名為 monitor（第 3 節）。

## 摘要

- **問題**：連線靜靜斷了（沒有 FIN 或 RST）時，誰在等對方，誰就一直等，TCP 與 sshd 可能要兩小時才發現。Ferry 現在的時限是一處一處補上的，還有漏的。
- **兩條原則**（第 1 節）：
  - **狀態可知**：強保證只放在「失去連線」上：失去連線之後，每一頭都在上限內自己收拾。只要連線還在，任何狀態都看得到：回報出來，不逾時。
  - **操作可控**：使用者在 client 上的每個操作都傳到每一頭；連線還在時明確傳過去並確認結果，不在時由失去連線的規則接手。
  - 加上一條設計準則：盡可能減少額外規則，新的情況先放進同一套設計。
- **等待分五種**，答案各不相同：等網路上的對方、等網路上的第三方、等一個人、等系統、host 上 monitor 出現之前（第 2 節）。
- **Monitor**：每個地方一個，client 的機器上一個，在 client 行程裡；每個 host 上一個，就是現在的 launcher。它是那裡的頂層，管那裡所有的資源；核心是不做 I/O 的狀態機，兩邊共用。server 是它的 worker：只收「停」，只說做完或失敗，從標準錯誤上的生命線知道 monitor 還在不在。兩個 monitor 經追蹤用的連線配對，那是唯一的健康連結，也是唯一的控制連結（第 3 節）。
- **操作**：第一次 Ctrl-C 經追蹤用的連線送「停」，等每一頭回報結果；3 秒內的第二次 Ctrl-C 強制結束（第 4 節）。
- **狀態偵測**：行程自己說每個 thread 在做什麼，加上 monitor 從作業系統外面看；行程整個卡住、回不了話時，外面的觀點照樣有（第 5 節）。
- **機制**（第 6 到 11 節）：
  - 每個等待都屬於一個 Scope，Scope 排成一棵樹；
  - 一個 host 一個 Liveness，來源隨階段換手；
  - transport 對外只給 `Wire`、`Listener`、`Child` 三種型別，所以「每個等網路的等待都會在失去連線時返回」由型別保證；
  - 會結束東西的計時只剩三種，都是 30 秒：心跳的沉默、心跳出現之前的暫時上限、失去連線的 host 給它的 worker 的寬限。
- **多條 channel**：一次傳輸用 K 條 stream，都走同一條 SSH 連線，和其他連線一起斷，健康不必多做事；要改的是 block 的順序。K 由握手時量到的 RTT 決定（第 12 節）。

## 問題

**無聲斷線**指連線斷了，兩端卻都沒收到關閉（沒有 FIN 或 RST），例如網路中斷、筆電休眠、NAT 的對應被清掉。這時誰在等對方，誰就一直等：

- TCP 的 keepalive 預設 7200 秒才開始探測，再 9 次各 75 秒，約 2 小時 11 分才發現；
- sshd 的 `ClientAliveInterval` 預設 0，不主動探測。fatcat 是預設值；nano4 的 `sshd_config` 一般使用者讀不到，kernel 的 `tcp_keepalive_time` 一樣是 7200；
- client 這邊，Ferry 自己開的 master 有 `ServerAliveInterval`，使用者自己的 ControlMaster 不一定有。

Ferry 目前的保證是一處一處補上的：stream 有 5 秒的 keepalive 與 20 秒的沉默；client 以 20 秒判斷每條 control connection；one-session 第 6 步讓追蹤 launcher 的連線也有。做那一步時才發現還有沒有時限的地方（[附錄](#附錄已知的缺口)），所以要有系統地處理。

**目標**（2026-10-26 使用者）：統一整個連線健康的管理。每條連線的每個階段，任何一方在等另一方時，失去連線都不會讓它一直等下去，兩頭都在上限內收拾；讀、寫、等連線進來、等對方表明身分、等子行程結束都算。

## 提案

### 1. 原則

**狀態可知。** 網路不可控：連線斷了，另一頭的狀態就不可知，我們也碰不到它。只要連線還在，任何狀態，出錯也好、卡住也好，都看得到、也處理得了。所以：

> 強保證只放在「失去連線」上：失去連線之後，每一頭都在上限內自己收拾，不留下看不到也收拾不了的狀態。只要連線還在，任何狀態都看得到：回報出來，讓使用者，或 Ferry 原本的錯誤處理，決定怎麼做。每個可能卡住的東西，都由一個不是它自己的觀察者看著。

就算沒有人在看（例如排程執行），連線還在時的狀態也是確定的：啟動它的人隨時能 Ctrl-C 或 kill，不會有東西在我們不知道的地方繼續跑。

| 情況 | 看得到嗎 | 處理 |
|---|---|---|
| 網路靜靜斷了 | 看不到 | 兩頭各自在 30 秒內收拾 |
| 心跳路上的行程被暫停：client（Ctrl-Z）、host 的 monitor、追蹤用的 `ssh -W`、ssh 的 master | 從另一頭看，和網路斷了分不出來 | 照網路斷了處理 |
| 心跳路以外的行程被暫停、檔案系統卡住、PAM、死結 | 看得到：連線還在 | 回報，讓使用者判斷 |
| host 下載時，它和 GitHub 之間的連線 | 那條連線的狀態看不到 | 要有上限：工具的速度下限 |
| host 上 monitor 出現之前 | 那時的連線就是第一個 session | 雙向心跳：失去它，兩頭各自收拾（第 2 節第 5 種） |

**操作可控。** 狀態可知說的是看得到；操作可控說的是做得到：

> 使用者在 client 上的每個操作，都傳到每一頭。連線還在時，明確傳過去，並確認結果；連線不在時，由「失去連線就在上限內收拾」接手。

所以它不需要另一套保證，只要求「明確傳過去」這條路存在（第 4 節）。

**看得到就等，看不到才給寬限。** 由兩條原則推出：client 這頭永遠有使用者在（或啟動它的程式），只要狀態看得到，就能等，並說出在等什麼；等不了，使用者按第二次 Ctrl-C 強制。真正沒有人能介入的，只有**失去連線之後的 host**：client 碰不到它，使用者也看不到它，只有這裡要由它自己在上限內收拾，也就是**寬限**。

| 情況 | 狀態可知嗎 | 處理 |
|---|---|---|
| 傳輸正常結束，等承載 channel 的 `ssh` 結束 | 可知：連線還在時，對方的狀態由 host 的 monitor 回報 | 等；太久就說出在等誰 |
| 某一頭失敗，等另一個 server 結束（現在的 `GIVEN`） | 可知 | 等，說出在等誰；不再 5 秒就砍 |
| 一個 server 做完了，另一個遲遲沒做完（現在的 `LINGER`） | 可知 | 等，以現有的診斷說出它卡在哪裡；不再 30 秒就砍 |
| 第一次 Ctrl-C，等每個 host 回報結果 | 可知 | 等，說出還在等哪一頭；第二次 Ctrl-C 強制 |
| client 停本機的 server | 可知：本機 | 等；第二次 Ctrl-C 強制 |
| 某個 host 失去連線，client 這邊承載它的 `ssh` | 那一頭不可知；這些 `ssh` 只是承載工具，等它們沒有意義 | 立刻結束，不等 |
| host 的 monitor 收到「停」，連線還在 | 可知：client 看得到 host 的回報 | 等 worker 收拾，持續回報 |
| **host 的 monitor 失去連線**：靜靜斷了、client 被 `kill -9`、或使用者按了第二次 Ctrl-C 讓 client 直接結束 | **不可知**：沒有人看得到它 | **寬限**：SIGTERM，等寬限，到期 SIGKILL，自己結束 |

所以：

1. **等待一定要說出來。** 既然不逾時，client 等超過約 1 秒就顯示在等什麼，例如「在等 nano4 的 server 收拾」，不然使用者沒有資訊判斷要不要強制。
2. **強制是 3 秒內的第二次 Ctrl-C**（第 4 節）。
3. **client 這頭的收尾時限都拿掉**：`ENDING`、`GIVEN`、`close_within`、`LINGER`；寬限只留在 host 的 monitor 裡（第 10 節）。

**由原則推出的兩條規則。**

1. **心跳路上 vs. 心跳路外。** 心跳走的路是：client 送心跳的 thread → client 追蹤用的 `ssh -W` → ssh 的 master → TCP → sshd → host 的 monitor。這條路上任何東西停了，從另一頭看都和網路斷了分不出來，所以當成失去連線，30 秒後兩頭各自收拾；這條路以外的停了，心跳持續確認連線還在，所以是看得到的狀態，回報。
2. **每個可能卡住的東西都有不是它自己的觀察者。** host 上的 worker（server、sudo 的檢查、fixture）由 host 的 monitor 看著；client 的子行程（`ssh`、本機的 server）由 client 的 monitor 看著。worker 就算整個卡住、連自己的狀況都送不出來，它的 monitor 還是說得出來。觀察的方式統一在 transport 的 `Child`（第 8 節）：它以 `waitpid` 的 `WUNTRACED`、`WCONTINUED` 說出被暫停與繼續，並從作業系統看它的每個 thread（第 5 節）。host 上看到的經追蹤用的連線回報給 client。

![心跳的路：client 送心跳的 thread、追蹤用的 ssh -W、ssh 的 master、TCP、sshd、host 的 monitor。這條路上任何東西停了或沒聲音，都當成失去連線，兩頭各自在 30 秒內收拾。路以外的，由不是它自己的觀察者看著：client 看著 stream 的 ssh -W 與本機的 server，host 的 monitor 看著 server、sudo 的檢查與 fixture；它們被暫停時看得到，由觀察者回報，讓使用者判斷](../assets/silent-drops-path.svg)

**設計準則：盡可能減少額外規則（2026-10-26 使用者）。** 遇到新的情況，先試著放進同一套設計，沿用已有的規則；只有放不進去，才考慮新的。照這個方向收斂的有：

- 第一個 session 的雙向心跳和之後的心跳同一條規則，只換了承載方式；
- 暫時的上限、寬限和心跳的沉默是同一個值；
- 本機與 host 上的 worker 同一種停法、同一條生命線；
- `LINGER`、`GIVEN` 併進「看得到就等」；
- 本機的 `hand` 是 `Wire` 建立時等對方回應的那一步，和遠端等 start mark 同一個模式（第 2 節）。

### 2. 等待的五種

**Scope 與 Liveness 判斷的是連線活著，不是事情有進展。** 把所有等待都當成「等網路上的對方」，只給得出一種答案；照第 1 節的原則，等待有五種，答案各不相同。

![等待的五種：等網路上的對方，由 Scope 與 Liveness 管；等網路上的第三方，工具帶進展的下限；等一個人，不設上限，只有 Ctrl-C；等系統，連線還在、看得到，回報不逾時；host 上 monitor 出現之前，只有 shell：第一個 session 上的雙向心跳，host 那頭由 shell 的看門狗實作](../assets/silent-drops-kinds.svg)

| 種類 | 例子 | 怎麼處理 |
|---|---|---|
| 1. 等網路上的對方 | 連線上的讀寫、accept、對方表明身分 | Scope 與 Liveness（第 6 到 11 節） |
| 2. 等網路上的第三方 | 下載 release、cargo 抓 crate | 網路，所以要有上限：Ferry 啟動的工具帶上**進展的下限**。host 下載時的 curl 已經有 `--connect-timeout 10 --speed-limit 1024 --speed-time 30`，wget 有 `-T 30`，cargo 有自己的 http 逾時；**client 自己抓 release 的 curl（`src/remote/release.rs`）沒有，要補上一樣的** |
| 3. 等一個人 | 密碼、OTP；沒有 `--yes` 時問要不要安裝 | **不設上限**，只有 Ctrl-C 結束它；其他已經有心跳的 host 照常 |
| 4. 等系統 | 檔案系統卡住（例如 NFS）、心跳路以外的行程被暫停、sudo 的 PAM 等輸入、peer 死結 | 連線還在，看得到：**回報，讓使用者判斷**，不逾時（第 5 節） |
| 5. host 上 monitor 出現之前 | 第一個 session 的 shell 等 client 的下一個指令、等上傳 | 那時 host 上還沒有 Ferry，只有 shell：第一個 session 上的雙向心跳，見下面 |

安裝的那幾分鐘也照這張表回答：`alive` 行（下面）確認的是連線還在，這就夠；安裝本身的進展是看得到的狀態，cargo 的輸出顯示給使用者；cargo 抓 crate 屬於第 2 種。

#### 第 5 種：第一個 session 上的雙向心跳

host 上 monitor 出現之前，第一個 session 自己帶著兩個方向的心跳，規則和之後的心跳一樣，5 秒送一次、30 秒沒收到就算斷，只是由 shell 實作：

- **host → client**：腳本在背景每 5 秒印一行加框的 `alive`；client 30 秒沒收到東西就算那個 host 不在了（第 7 節）。
- **client → host**：client 每 5 秒送一行什麼都不做的指令 `:`，包括它在等使用者回答要不要安裝的時候。
- **host 的看門狗**：probe 腳本跑自己的指令迴圈（`while IFS= read -r line; do …; done`），每讀到一行，就在一個記錄動靜的檔案後面附加一個位元組；上傳時，正在寫入的檔案變大也算動靜。背景的看門狗每 5 秒比一次兩者的大小（`wc -c`，POSIX 沒有 `stat`），30 秒都沒變，就結束整個 session，連同還在讀上傳的子行程（`kill -TERM -$$`：sshd 為沒有終端機的 session 開的 shell 是 session leader，要在 nano4 與 fatcat 上實測確認）。exec 成 monitor 之前，停掉背景的 `alive` 與看門狗，並刪掉記錄動靜的檔案。

所以 monitor 出現之前，host 的 Liveness 來源就是這個雙向心跳；monitor 出現之後，換成真正的心跳。承載的方式換了，規則沒變，第 5 種也就不再是特例。

- 好處：沒有 `--yes` 時，client 等使用者回答的那段照樣送 `:`，看門狗不會誤殺，不必多開 session，也不必在開 session 之前先問使用者；上傳不必分段，看門狗看的是檔案有沒有變大。
- 代價：shell 的 `sleep 5` 讓判斷落在 30 到 35 秒之間，不像 Rust 那邊準；只能用 POSIX sh 的工具；要確實砍掉卡在讀取上的子行程；腳本裡的時間要能以注入縮短，讓測試用假的 ssh 停止送 `:`、看 host 的 shell 在時限內結束。

#### 兩個特別的等待

- **本機把 stream 交給 server（`hand`）**：client 把 socket pair 的一端經本機 server 的 Unix socket 傳過去（`SCM_RIGHTS`），並等 server 回一個位元組表示收下，現在最多等 20 秒（`TAKING`）。照設計準則，它不另訂規則：它就是 `Wire` 建立時等對方回應的那一步，遠端等的是 start mark，本機等的是那個位元組；看得到就等，說出在等什麼，Scope 結束就返回；本機 server 結束時 socket 一關，立刻返回。`TAKING` 拿掉。收下之前 client 一直握著自己那份，因為 macOS 會清掉傳送途中、對方還沒 accept、又沒人握著的 socket（`b8cf4e2`）；這是 macOS 上傳 fd 才正確的做法，留在本機那種接法（Carrier）的實作裡，和健康無關。
- **`--remote-join` 時 host 上的加入行程**：它由 sshd 的 session 啟動，不是 host 的 monitor 啟動的；它把自己的輸入輸出交給 monitor 或 server，然後等對方放開它握著的 socket。所以它跟著它加入的那個 Scope 結束：monitor 或 server 一結束，socket 就關，它也就結束（第 6 節）。

### 3. Monitor：每個地方一個，管那裡所有的資源

peer 的 owner、decider、executor 解決的是資料怎麼流、I/O 放在哪裡；誰活著、誰該結束、失去連線怎麼收拾、使用者的操作怎麼傳到每一頭、卡住時誰說出來，是另一組問題，交給 **monitor**。第 6 到 8 節的 Scope、Liveness、三種型別，都是 monitor 用來做事的東西。

![Monitor：每個地方一個，client 的機器上一個，每個 host 上一個（現在的 launcher）。核心不做 I/O，持有 Scope 樹，只做決定；觀察者（心跳、等子行程、收訊號、各條 Wire）做 I/O，把事件送進核心；核心的動作是取得與結束資源（啟動 worker、結束 Scope）、送「停」、回報、結束。worker 在它底下，只收「停」，只回報做完或失敗。兩個 monitor 經追蹤用的連線配對](../assets/silent-drops-monitor.svg)

**這些現在散在好幾處：**

| 現在在哪裡 | 管什麼 |
|---|---|
| `follow.rs` | client 追蹤兩個 server：`LINGER`、`GIVEN`、誰先結束 |
| `interrupt.rs` | 訊號、收拾暫存檔、結束子行程 |
| `peer.rs` 的 `serve` | server 自己的生命週期：`DONE`、等 client 結束 control |
| `launcher.rs` | launcher 的生命週期、它啟動的程式 |
| `transport` 各處 | 等子行程結束的 `ENDING`、`close_within`、`end_within`；stream 層的 keepalive 與沉默 |

**形狀：**

1. **每個地方一個 monitor**，它是那裡的頂層，管那裡所有的資源：
   - **client 的 monitor**：在 client 行程裡，管傳輸、每個 host、client 的連線與子行程（`ssh`、本機的 server）。client 行程除了 monitor，還有準備、建立 stream、畫面這些工作：monitor 是它的頂層，不是整個 client。
   - **host 的 monitor**：現在的 launcher 行程。它管那個 host 上的一切：它的 listener、它啟動的程式。「啟動 server」只是它取得一個子資源的方式，所以它本來就兼任 launcher。第一個 session 照樣 exec 成它，one-session 不受影響。
2. **兩個 monitor 共用同一個核心**，照 owner 的架構（2026-10-23 的原則）：一個不做 I/O 的狀態機，持有 Scope 樹，只做決定。輸入是事件：心跳斷了；子行程被暫停、繼續或結束；使用者的訊號；worker 做完或失敗；對方的 monitor 送來「停」。輸出是動作：取得資源（啟動 worker、開連線）、結束某個 Scope、送「停」、回報、結束。時間由外面給，以合成的事件與時間就能做確定性的測試，不必真的等 30 秒。
3. **觀察者做 I/O**，把事件送進核心：送心跳與聽心跳的 thread、等子行程的 thread（`Child`）、收訊號的 thread、各條 `Wire`。
4. **兩個 monitor 隔著網路配對**，經追蹤用的連線（client 追蹤 host 的 monitor 的那條）：那是**唯一的健康連結**（心跳），也是**唯一的控制連結**（「停」與結果的回報）。host 的 monitor 是 client 的 monitor 經第一個 session「啟動」的，所以在樹裡，它的父節點是 client 的 monitor：連線還在時，client 的 monitor 看得到它、也管得到它；失去連線後，它自己在上限內收拾。
5. **server 是 worker，不是 monitor**：它沒有心跳，也沒有自己的 Scope 樹，只在三種情況結束，它的每個等待也都因這三件事返回：
   1. 被告知停下：control 結束，或收到 SIGTERM；
   2. 它的 monitor 不在了：**生命線**被關掉（下面）；
   3. 自己做完了。

   peer 在 worker 裡，只和這段很薄的「worker 怎麼結束」說兩件事：收到「停」（附原因）；回報「做完了」或「失敗了」。peer 不知道連線、訊號、計時器的存在。這是 2026-10-23「peer 只收 Lost 與 Closed 事件，不持有網路計時器」的完整版。本機與 host 上的 server 共用這段結束邏輯。

**client 的 monitor。** 它不是另一個行程，而是 client 裡管生命週期的那一層，而且是兩個地方裡最忙的：

| 它的工作 | 現在在哪裡 |
|---|---|
| 每個 host 的 Liveness：心跳的一頭；host 不在了就讓傳輸失敗、收拾 | `transport/launcher.rs`、`follow.rs` |
| 看著自己的子行程：`ssh`、本機的 server（被暫停、繼續、結束） | `transport` 各處的 `ENDING`、`end_within` |
| 使用者的操作：第一次、第二次 Ctrl-C，SIGTERM，SIGHUP；把「停」送到每個 host、收它們的結果 | `interrupt.rs` |
| 決定傳輸什麼時候結束：哪個 server 做完了、失敗了、不在了 | `follow.rs`（`LINGER`、`GIVEN`） |
| 把狀態說給使用者：誰被暫停了、在等誰、為什麼失敗 | 各處 |

和 host 的 monitor 共用核心，心跳的兩頭行為一致（多久沒收到算斷、斷了做什麼，同一段程式），Scope 樹、「停」怎麼傳、看得到就等也是同一套，測試只寫一套。它和 host 的不同在於：

- **沒有生命線**：它上面沒有別的 monitor；看著它的是使用者本人，看得到畫面，按得了 Ctrl-C。
- **多了使用者這個輸入**：訊號、兩段式的 Ctrl-C，以及等一個人（密碼、OTP、要不要安裝）時不設上限。
- **管好幾個 host**；host 的 monitor 只管自己那個地方。

**生命線：worker 的標準錯誤。** host 的 monitor 自己被 `kill -9` 或當掉時，host 上沒有人看著它；若網路剛好也斷了，client 碰不到 host，worker 的 channel 又不會關，worker 就會留到 sshd 發現為止。所以每個 worker 都要知道它的 monitor 還在不在。生命線放在 **worker 的標準錯誤**上，所有 worker 都一樣，經不經過 `sudo`、在本機或 host 上都是：

- monitor 啟動 worker 時，把它的標準錯誤接成 **socket pair** 的一端，自己握著另一端，收到的照樣轉到自己的標準錯誤，錯誤訊息不會不見。本機的 server 也一樣，由 client 的 monitor 收、轉到畫面；畫面在畫儀表板時，也能把 server 說的話整齊地插進去。
- worker 照常寫標準錯誤，也以 poll 等 2 號 fd **可讀**：monitor 從不往裡寫，所以平常不會被叫醒；monitor 一不在（不論怎麼結束），它那端關掉，worker 這端就讀到結束（`POLLIN|POLLHUP`），有秩序地停下。它是純本機的事件，沒有計時器，也沒有網路。
- **為什麼是標準錯誤**：`sudo` 預設會關掉 3 號以上的檔案描述符（sudoers 的 `closefrom`），只留 0、1、2，標準錯誤本來就會被保留，所以一種做法就夠，不必為 `sudo` 另寫（其他做法裡有不採用的幾種）。
- **為什麼是 socket pair**：管道的讀端關掉時，寫端在 macOS 上沒有安靜的等法（第 0 步）；socket 可以從 worker 這端讀，兩個平台上等「可讀」都只在對方關掉時醒來。
- **順帶的好處**：monitor 看得到 worker 說了什麼，可以附在回報裡，和現在的 launcher 對跑到結束的程式（sudo 的檢查、fixture）的做法一樣。

**要注意的：**

1. **寫標準錯誤失敗時，`eprintln!` 會 panic。** monitor 一不在，worker 下一次寫標準錯誤就收到 EPIPE；Rust 的 `eprintln!` 寫入失敗會 panic，而 Ferry 的 panic 直接結束行程，等於在有秩序地停下之前就當掉，暫存檔可能來不及收拾。所以 worker 寫標準錯誤一律改用不 panic 的寫法，寫不出去就忽略，並盡快看到生命線斷了、進入停止。這個風險今天也有（server 的標準錯誤接在 session 上，session 一關也會 EPIPE），只是更常遇到。
2. **monitor 要一直把標準錯誤讀走。** socket 的緩衝滿了，worker 寫標準錯誤就卡住；所以 monitor 有一個 thread 專門讀它、轉出去。和今天差不多：今天 worker 直接寫 session 的標準錯誤，ssh 不讀時一樣會卡。
3. **monitor 那端不能漏給別的子行程。** 別的子行程也握著它的話，monitor 死了那端也不算關掉，生命線就不觸發。socket pair 要以 close-on-exec 建立，只有 worker 那端在它自己的子行程裡接到 2 號 fd；以測試確認。
4. **經過 `sudo`**：照預設的 sudoers，2 號 fd 原樣傳到 worker；開了 `use_pty` 也一樣，因為沒有終端機時 `sudo` 不開 pty。**開了 I/O 記錄（`log_output`）就失效**：`sudo` 改用自己的管道夾在中間，monitor 死後 `sudo` 照樣活著，worker 發現不了（第 0 步）。這種 host 上，worker 自己看得出來：它拿到的 2 號 fd 不是 socket，而是管道；照狀態可知，**說出來**：「這個 host 的 sudo 夾在中間，host 的 monitor 若死掉、網路又同時斷了，這個 worker 不會自己結束」。只有三件事同時發生才會留下東西：sudo 開了 I/O 記錄、monitor 當掉、網路靜靜斷了。

**界線：**

- **只管生命週期，不管效能。** `control.rs` 的「是什麼拖住了傳輸」、`--profile` 照舊分開；monitor 只在卡住時把診斷送出去（第 5 節）。
- **只做決定，不搬資料、不碰檔案。** peer 內部怎麼停它的 stage，仍是 peer 自己的事：`stop.rs` 的第一個錯誤、`stage.rs` 的 stage panic 時停下，都留在 peer 裡。
- **不寫兩份。** client 與 host 的 monitor 共用核心；不共用的只有觀察者與 Scope 樹。worker 的那段結束邏輯，本機與 host 上的 server 也共用。

**名字。** launcher 這個名字當初是因為「它只做啟動」；現在它除了啟動，還有心跳、看著它啟動的程式、失去連線時收拾，所以改名為 **monitor**，子命令 `ferry launcher` 改成 `ferry monitor`，程式碼與文件在實作時一起改（計畫第 4 步）。

### 4. 操作可控：使用者的操作怎麼傳到每一頭

| 使用者的操作 | 連線還在時 | 連線不在時 |
|---|---|---|
| 第一次 Ctrl-C、SIGTERM、關掉終端機（SIGHUP） | client 在追蹤用的連線上對每個 host 的 monitor 送「停」；monitor 對它啟動的程式送 SIGTERM，收集各自怎麼結束，回報 client；client 等它們回報，等超過約 1 秒就說出還在等哪一頭 | host 的 monitor 自己發現失去連線，照樣收拾 |
| 3 秒內的第二次 Ctrl-C | client 對自己的子行程送 SIGKILL，立刻結束；host 讀到連線結束（不是沉默），立刻收拾，不必等 30 秒 | 同左 |
| client 被 `kill -9` | 攔不到；client 的 `ssh` 讀到輸入結束而關掉 channel，或心跳停止，host 在 30 秒內收拾 | 同左 |

![操作可控：第一次 Ctrl-C，client 在追蹤用的連線上送「停」，host 的 monitor 對它啟動的程式送 SIGTERM，收集結果回報，client 說出每一頭的結果；第二次 Ctrl-C，client 立刻結束，host 讀到連線結束，立刻收拾；client 被 kill -9，心跳停止或 channel 關掉，host 在 30 秒內收拾](../assets/silent-drops-stop.svg)

**現在的做法，以及為什麼要改。** 現在 Ctrl-C 也讓 host 停下，但是間接的：`ssh` 子行程和 client 在同一個 process group（`src/transport/ssh.rs` 沒有另外分組，`interrupt::unblock_in_child` 讓它們收得到訊號），終端機的 SIGINT 同時打到它們，它們立刻結束、channel 關掉，host 上的 server 讀到 control 結束而自己停下。問題是：client 來不及有秩序地停，channel 已經沒了，送不出「停」、也收不到回覆；而且**結果不可知**，client 不知道 host 上收拾乾淨了沒，例如暫存檔刪了沒。

**做法：**

1. **分組。** 經 master 的 `ssh`（第一個 session、每條 `-W` channel）與本機的 server 各自放進自己的 process group，終端機的訊號只到 client，由 client 的 monitor 決定怎麼停。本機的 server 和 host 上的 worker 停法一樣：SIGTERM，等它結束，等太久就說出在等它，第二次 Ctrl-C 才 SIGKILL；結果從它的 control 與結束狀態看得到。
2. **第一次 Ctrl-C** 照上表明確傳達，結果回報給使用者。畫面說出「停止中，3 秒內再按一次 Ctrl-C 強制結束」。SIGTERM 與 SIGHUP 照第一次 Ctrl-C 處理。
3. **第二次 Ctrl-C 只在 3 秒內才強制。** 3 秒內沒有第二次，就重新計算：之後再按，算是新的第一次，只再說一次在等什麼，並重新開始 3 秒；免得誤觸兩下就強制結束（2026-10-26 使用者）。現在的 `interrupt.rs` 沒有這個計時：第一次之後，任何時候再收到 SIGINT 都立刻結束。
4. **host 遲遲沒有回覆時**，client 說出還在等它；若它其實失去了連線，心跳 30 秒內發現，交給失去連線的規則；使用者也可以按第二次 Ctrl-C，host 讀到連線結束，自己在寬限內收拾。

**要注意的：**

- **登入用的 `ssh` 不能分組**：它要從終端機讀密碼或 OTP，在背景的 process group 裡讀終端機，會被系統暫停（SIGTTIN）。所以只有登入之後、經 master 的 `ssh` 才分組。
- **Ctrl-Z 的行為不變**：分組之後，Ctrl-Z 只停住 client，`ssh` 不跟著停；client 送心跳的 thread 停了，30 秒後 host 照樣判定失去連線（取捨第 5 點）。

### 5. 狀態偵測：自己說，加上從外面看

看得到的程度，決定使用者能不能判斷；這是狀態可知的實質內容，所以要做得完整（2026-10-26 使用者）。

![狀態偵測：worker 自己說每個 thread 的角色、狀態、多久與等的對象，經 control 回報；monitor 的 Child 從外面看那個行程與每個 thread：執行、睡眠、被暫停、不可中斷的等待，在 Linux 上還有卡在核心的哪裡、哪個系統呼叫、哪個檔案。兩者合起來給使用者；worker 回不了話時，外面的觀點照樣有](../assets/silent-drops-state.svg)

**現在有的：自己說。** `profile.rs` 讓每個 thread 記錄它的角色（`scan`、`pump`……）、它現在的狀態（`ReadFile`、`WriteConnection`、`ToDecider`……）與進入這個狀態多久。按 `Ctrl-\`，或 60 秒沒有進展時，client 向每個 server 要一份 snapshot，說出每個 thread 在哪裡、等了多久。它的限制是：這是**行程自己說的**；整個行程卡住時（例如所有 thread 都卡在 NFS 上），它回不了 snapshot，使用者就什麼都看不到，違反「觀察者不是它自己」（第 1 節規則 2）。

**兩個觀點：**

1. **自己說（內部觀點，現有的）**：每個 thread 的角色、狀態、多久。要補的：等的**對象**，例如哪個檔案的路徑、哪條連線；各角色的進展計數。
2. **從外面看（外部觀點，新的）**：由 monitor 的 `Child` 從作業系統看那個行程與它的每個 thread，行程自己回不了話也看得到：
   - **Linux**：`/proc/<pid>/task/<tid>/stat` 說出執行中、睡眠、被暫停、**不可中斷的等待（D）**；`wchan` 說出卡在核心的哪個函式，例如 `nfs_wait_bit_killable` 就能說成「在等 NFS」；`syscall` 加上 `/proc/<pid>/fd/<n>` 說出卡在哪個系統呼叫、哪個檔案，例如「`read()`，`/work/x/y`」。讀 `syscall` 要和 ptrace 一樣的權限，但 monitor 是 worker 的父行程，系統限制 ptrace（Yama 的 `ptrace_scope=1`）時也讀得到。
   - **macOS**：`proc_pidinfo` 讀得到同一個使用者的其他行程，不論是不是子行程。每個 thread 的狀態（`PROC_PIDTHREADINFO`）看得到「執行中」與「等待」；被暫停要看行程層級的狀態（`PROC_PIDTBSDINFO` 的 `pbi_status`，會是 `stopped`），thread 層級仍顯示「等待」。「不可中斷的等待」在 macOS 上造不出來，沒有驗證；也沒有 Linux 的 `wchan`、`syscall` 那樣的資訊（第 0 步）。
   - **經 `sudo` 的 worker** 以 root 執行，兩個平台上都讀不到這些（macOS 是 `EPERM`），只說得出它還在、怎麼結束。
   - **其他平台**：只看得到它還在。

**合起來顯示。** monitor 一邊向 worker 要 snapshot，一邊立刻附上外部觀點；snapshot 回來了就補上。worker 回不了話時，使用者照樣看得到，例如：「nano4 的 server 沒有回應 snapshot；從外面看：16 個 thread，15 個在睡，1 個在不可中斷的等待（NFS），`read()` `/work/x/y`」。要 snapshot 只是一次觀察，不因此做任何決定，所以不是計時，不違反「看得到就等」。host 上的外部觀點由 host 的 monitor 讀，經追蹤用的連線送到 client。

**什麼時候說**，都沿用既有的時機，不新增規則：

- `Ctrl-\`，使用者主動要；
- 60 秒沒有進展（現有的）；
- 等待超過約 1 秒時，說出在等誰，附上對方的狀態；
- 行程被暫停或繼續的時候；
- 失敗的時候，附上最後一份狀態。

### 6. 每個等待都屬於一個生命週期（Scope）

![Scope 的樹：client 端是傳輸、每個 host、那個 host 上的每條連線與子行程；host 端是 host 的 monitor 與它啟動的程式；兩邊以心跳相連](../assets/silent-drops-scopes.svg)

- **client 端**（client 的 monitor）：一次傳輸是根；底下是每個 host；每個 host 底下是它的連線（第一個 session、追蹤用的連線、每條 control、stream 的一半）與承載它們的 `ssh` 子行程。本機的 server 是 worker，直接掛在傳輸底下：它是 client 的子行程，生命線和 host 上的 worker 一樣是它的標準錯誤，不需要 Liveness。
- **host 端**（host 的 monitor）：它是根，它的父節點是 client 那棵樹上那個 host 的 Scope，中間隔著網路，以心跳與「停」相連；底下是它的 listener（等連線進來）與它啟動的 worker（server、sudo 的檢查、fixture），每個 worker 都拿著一條生命線。`--remote-join` 時的加入行程算在它加入的那個 Scope 底下（第 2 節）。
- **規則只有三條**：
  1. 每個 Scope 只有一個「結束了」的來源：傳輸是 Ctrl-C、失敗、或任何一個 host 的 Scope 結束；host 是它的 Liveness；host 的 monitor 是它暫時的上限或心跳；一條連線是它的父 Scope，或它自己的結尾。
  2. 父的結束了，子的跟著結束：看得到的子，就送 SIGTERM 等它結束，並說出在等什麼，第二次 Ctrl-C 強制；只有失去連線的 host 端，給它的 worker 寬限（第 1 節）。傳輸的 Scope 因使用者的操作結束時，先在每條追蹤用的連線上送「停」，等 host 的 monitor 回報各自的結果（第 4 節）。
  3. 每一個會阻塞的操作都帶著一個 Scope，只因兩種原因返回：等的事發生了，或 Scope 結束了（附原因）。

### 7. 一個 host 的 Liveness，來源隨階段換手

![一個 host 的 Liveness 在各階段的來源：登入時是 ssh 自己的上限；第一個 session 是 alive 行；host 的 monitor 說出位置後是 30 秒內要被追蹤；之後是心跳。monitor 出現之前，host 那頭由 shell 的看門狗看 client 每 5 秒送的指令。它說不在了的時候，client 端那個 host 上的每個等待都返回，host 的 monitor 結束它啟動的程式](../assets/silent-drops-liveness.svg)

| 階段 | client 看那個 host 的 Liveness | host 那頭自己的 Liveness |
|---|---|---|
| 登入 | ssh 的 `ConnectTimeout`；等 ControlMaster 的 30 秒 | 還沒有 monitor |
| 第一個 session（probe、安裝） | 腳本在背景每 5 秒印一行加框的 `alive`，30 秒沒收到就算斷；在 host 上建 dev build 的那幾分鐘也有。exec 成 monitor 之前停掉它 | shell 的看門狗：client 每 5 秒送一行 `:`，30 秒沒有動靜就結束 session（第 2 節） |
| host 的 monitor 說出位置之後、被追蹤之前 | 等追蹤用的連線的 start mark，上限 30 秒 | 30 秒內要被追蹤 |
| 被追蹤之後 | 心跳：5 秒送一次，30 秒沒收到就算斷 | 同一個心跳 |

Liveness 說不在了：client 那個 host 上的每個等待都返回並說出原因，傳輸失敗；host 的 monitor 先送 SIGTERM 給它啟動的程式，讓它們像收到 Ctrl-C 一樣收拾，寬限到期就砍掉，然後自己結束。

一個 host 只有一條 SSH 連線（2026-10-25 定的），所有 channel 走同一條 TCP，網路靜靜斷掉時一起斷，所以一個心跳就判斷得了那個 host 上的所有連線。

### 8. 等待一律經過 transport 的三種型別

![一個等待：Wire 的讀寫、Listener 的 accept、Child 的 wait，都同時等兩件事：等的事，或 Scope 結束；前者返回結果，後者返回結束的原因。整個程式只有三種計時，定義在同一處](../assets/silent-drops-wait.svg)

| 型別 | 做什麼 | 等什麼 |
|---|---|---|
| `Wire` | 一條連線：讀寫封包，也讀原始位元組（shell 的雜訊、記號、握手、秘密）；正常結尾的標記是一種封包，讀到就是結尾，用來分辨正常結束與被切斷。建立時等對方回應：遠端是 start mark，本機交給 server 的（`hand`）是 server 回的那個位元組 | 資料、空間、對方的回應與結尾，或 Scope 結束 |
| `Listener` | 等連線進來，並等它表明身分（開頭的位元組、port 的秘密）；還沒表明身分的連線不屬於任何 host，掛在 listener 底下，用暫時的上限 | 連線、身分，或 Scope 結束 |
| `Child` | 承載連線的行程與啟動的程式；每個可能卡住的東西的觀察者（第 1 節規則 2） | 它結束，或 Scope 結束；後者送 SIGTERM 並等它，說出在等它；在失去連線的 host 端，寬限到期就砍。等待時也說出它被暫停與繼續，並提供外部觀點（第 5 節） |

transport 對外只提供這三種型別，連線的 fd 不外流，所以沒有人能繞過 Scope 直接讀寫。怎麼接上（`ssh -W`、loopback port 加秘密、`--remote-join`、本機的管道或 socket pair）只在建立 `Wire` 時不同，之後都一樣。

### 9. 所有的等待，與它的上限從哪來

| 等待 | 例子 | 上限從哪來 |
|---|---|---|
| 讀 | 第一個 session、start mark、握手、control、stream、sudo 的檢查、fixture | 所屬 host 的 Liveness |
| 寫 | 對方不讀，或連線已死但 TCP 還沒發現 | 同上 |
| 等連線進來 | host 的 monitor 等被追蹤；之後的每條連線；server 等 stream | host 的 monitor：暫時的上限，之後是心跳；server：它自己的 Scope，隨 control 結束、生命線關掉或 SIGTERM 而結束 |
| 等對方表明身分 | 連線開頭的位元組、port 的秘密 | listener 底下的暫時上限 |
| 等子行程結束 | `ssh -W`、本機的 server、host 的 monitor 啟動的程式 | 看得到就等，說出在等什麼，第二次 Ctrl-C 強制；失去連線的 host 端是寬限（第 1 節） |
| 登入 | ssh 本身 | ssh 的 `ConnectTimeout`；等 ControlMaster 的 30 秒 |
| 等網路上的第三方 | 下載、cargo 抓 crate | 工具帶進展的下限（第 2 節第 2 種） |
| 等一個人 | 密碼、OTP、要不要安裝 | 不設上限，只有 Ctrl-C（第 2 節第 3 種） |
| 等系統 | NFS、心跳路以外的行程被暫停、PAM、死結 | 回報，不逾時（第 2 節第 4 種） |
| host 上 monitor 出現之前 | shell 等下一個指令、等上傳 | 第一個 session 上的雙向心跳，shell 的看門狗（第 2 節第 5 種） |

### 10. 計時

**會結束東西的計時只剩三種，都是 30 秒，定義在同一處：**

1. **心跳**：5 秒送一次，30 秒沒收到就算斷。
2. **暫時的上限**：心跳出現之前，或連線表明身分之前，30 秒。
3. **寬限**：只用在一處：host 的 monitor 失去連線之後，給它的 worker 收拾的時間，30 秒，到期 SIGKILL，然後自己結束。它是原則的後半：失去連線之後「在上限內收拾」的那個上限。例外要老實說：卡在不可中斷等待（D state）的行程連 SIGKILL 都砍不掉，Ferry 只能放棄它、自己照樣結束；它沒有網路可卡，等 I/O 回來就會結束。

其他的時間只影響顯示或怎麼解讀使用者的輸入，不結束任何東西：等待超過約 1 秒就說出來；兩次 Ctrl-C 之間的 3 秒；60 秒沒有進展就說出每個 thread 在哪裡。測試以 `FERRY_TEST_SILENCE` 之類的注入縮短。

### 11. 會拿掉的

- stream 層的 keeper thread、keepalive、沉默判斷；Deframer 裡拆 keepalive 的那段；`Ends.before` 與換手時的緩衝：資料裡不再混著 keepalive，原始位元組與封包是同一串，不必換手。
- `follow.rs` 對 control 的判斷、server 的 control stream、`Carried::let_go`。
- 各處各自的時限：launcher 與 `accept` 的 `Silent`、`Within`，`hand` 的 `TAKING`，以及收尾的 `ENDING`（5 秒）、`GIVEN`（5 秒）、`close_within`、`LINGER`（30 秒）。

### 12. 同一條 SSH 連線上的多條 channel

原本是 redesign 的改進選項；2026-10-26 使用者決定和上面一起做。

![多條 channel：K 條 stream 與追蹤用的那條，都是同一條 SSH 連線、同一條 TCP 的 channel。第 0 條帶 block 以外的一切，block 交給佇列最短的一條；Destination 依序號把 block 放回，緩衝以 data 額度為上限，再交給 decider；block 的結束是第 0 條上的 END(N)，排在雜湊與 Problem 之後。它們一起斷，一個心跳就涵蓋；心跳排在它們的資料後面，量過最多慢約 0.6 秒](../assets/silent-drops-channels.svg)

- **為什麼要**：一條 channel 每個 RTT 最多送 2 MiB（OpenSSH 的 channel window）。redesign 5.2 節的模擬中，RTT 120 ms 約 17 MB/s、250 ms 約 8 MB/s；主機用預設的 TCP buffer 時，多開 channel 最多快 ×1.35，buffer 調大時接近 ×K。fatcat、nano4 這種短路徑上沒有好處。掉包的路徑上也沒有幫助，那要多條 TCP 連線，2026-10-25 決定不做。
- **怎麼做**：client 為一次傳輸建 K 條 stream，每條都是那個（或那兩個）host 的 Scope 底下的一條 `Wire`；兩個 server 的 pump 把 block 交給佇列最短的一條，其他所有封包（manifest、needs、results、額度、控制訊息）固定走第 0 條。短路徑上 K=1，就是現在的樣子。
- **健康**：K 條都走同一個 master、同一條 TCP，一定一起斷，所以由同一個心跳管，不必多做任何事。任何一條讀到沒有結尾標記的盡頭，就算被切斷，傳輸失敗，不需要確認與重送。堵塞（window 滿了、等空間）只在 Scope 結束時返回，沒有另外的計時器，慢但沒斷的鏈路不會被誤判。
- **順序**：沒有訊息要等 block；反過來，是**「block 結束」要等第 0 條上的訊息**（第 0 步盤點）：
  - 分段大檔的雜湊（`FileHash`）、一個檔案讀不到的 `Problem`，現在都靠「比 block 的結束（`END`）先到」：Destination 在 block 結束時，會把還沒收齊的分段檔案判定失敗（`decider.rs` 的 `written_all`），也把「從沒送來」的檔案判定失敗（`stream_ended`）；Source 的註解明寫依賴這個順序（`source/mod.rs`）。K 條時，`END` 在快的那條上先到，就會錯判檔案失敗，甚至提早結束傳輸。
  - block 沒有對齊封包：`Mux::data` 把 block 的位元組不分邊界切成 64 KiB 的封包（`mux.rs`），Destination 用一個 `Assembler` 拼回來。所以不能「每個封包」分到不同條，只能整個 block 走同一條。
- **所以順序這樣做，五點：**
  1. **一個 block 整個走一條 stream**，每條一個 `Assembler`；序號放在 block 的前綴裡（`block.rs` 的 `PREFIX`），不必解碼 header 就讀得到；pump 有 K 個 data 佇列，由送 block 的那一段選。
  2. **`END` 改走第 0 條，帶著 block 的總數 N**：看到 `END(N)`，而且 0 到 N−1 都交給 decider 了，block 才算結束。`FileHash`、`Problem` 本來就排在 `END` 前面、在同一個佇列裡，所以照樣先到。
  3. **依序號放回**：沿用 pool 裡已有的放回順序的機制（BTreeMap），改用線上的序號；緩衝以 data 額度為上限。decider 其實不需要依序（分段檔案在任何位移先到都能處理，寫入用 `write_all_at`），放回順序是為了寫入照順序、現有的測試照樣成立。
  4. **第 0 條的事件走同一條路**：`FileHash`、`Problem`、`END(N)` 依第 0 條的順序經同一個 channel 到 decider；`FileHash` 不再經過 block 的 pool。這也順便消掉現在一個很小的競賽（`apply.rs` 的 `select!` 在兩邊都好時隨機挑）。
  5. **stream 的結束與失敗的原因**：一條 data stream 正常結束，不再代表 block 被切斷；只有第 0 條結束了還沒看到 `END(N)`，或某條斷了，才是。第 0 條上的 `Control::Error` 要比 data stream 的結束優先，當成傳輸的原因。也要檢查每條上序號重複、超出範圍、結束之後還有資料。
- **留在第 0 條的**：Destination 往 Source 的方向（needs、results、額度、Done）彼此有順序（`Through` 在 files 之後、Done 在 results 之後），全部留在第 0 條。manifest 是一整條 zstd 串流，也留在第 0 條。一個連帶的影響：現在 pump 有「data 佇列夠滿就先送 data，否則先送 manifest」的優先順序（`mux.rs` 的 `DATA_FIRST`）；K 條之後，manifest 與 data 誰先走，交給 SSH 在 channel 之間的排程，Ferry 管不到。
- **K 怎麼定**：**K = min(8, max(1, ⌈RTT ÷ 15 ms⌉))**，握手時量 RTT；RTT 2 ms 是 1 條，30 ms 是 2 條，120 ms 以上是 8 條。不給使用者選項：`--ssh-connections` 已經拿掉，這次也不加。依據：
  - 一條 channel 每個 RTT 最多 2 MiB，所以要跑滿約 1 Gbps，要 RTT／15 ms 條。
  - redesign 5.2 節的資料（1 Gbps 的模擬鏈路）：預設的 TCP buffer 下，4 條就到了 TCP 本身的上限（RTT 30、120、250 ms 時 89、22.8、11.0 MB/s，TCP 是 92、24、11.8），8 條多不了多少；buffer 調大到 64 MiB 時，條數才有用（RTT 120 ms 時 4 條 67.7、8 條 81.0 MB/s；250 ms 時 4 條 33.5、8 條 56.5）。
  - 多開的 channel 在短路徑上不會變慢（RTT 2 ms 時 1、4、8 條都是 118.7 MB/s），代價只有心跳排隊變長（下一點）、放回順序的緩衝（以額度為上限）與多幾個 `ssh` 行程。
- **和心跳的交集**：心跳走追蹤用的那條 channel，各 channel 的 window 各自算，但最後都排在同一條 TCP 的送出佇列裡。第 0 步以 TUN 模擬量過（同一個 master 上 K 條全速的 channel，另一條每 0.5 秒送一行、由對方回傳）：1 條資料 channel 時心跳幾乎不受影響；8 條時，最壞是 RTT 250 ms、buffer 64 MiB、host 往 client 的方向，最慢一次多了約 0.63 秒（885 ms，RTT 本身 251 ms），離 30 秒很遠。只量了 1 Gbps 的鏈路；更慢的鏈路上，排在 TCP 送出佇列裡的資料量大約和擁塞視窗相當，換成時間仍是 RTT 的量級，應該差不多，但沒有量。（2026-10-26 第 7 步量到這個假設不成立：延遲約是「排在前面的資料 ÷ 當下速率」，慢而掉包的路徑上要好幾秒。使用者決定：從對方來的任何東西都算心跳，誤判斷線因此解決；控制訊息排在資料後面的延遲接受，限制在途資料量列為 redesign 的改進選項。見進度的「第 7 步之後」。）

### 取捨

1. **host 的 monitor 會結束它啟動的程式。** one-session 原本的原則是「它啟動的東西不依賴它」。2026-10-26 使用者同意改。
2. **行程被暫停（SIGSTOP）不再由沉默發現，改成回報。** 網路沒斷，只是心跳路以外的行程被暫停時，心跳照常；照原則它是看得到的狀態：`Child` 說出被暫停與繼續，client 顯示出來，不設上限，由使用者判斷。心跳路上的被暫停則照失去連線處理（第 1 節規則 1）。死結兩種設計都抓不到（keepalive 是另一個 thread 送的），靠現有的「60 秒沒有進展就說出每個 thread 在哪裡」。
3. **心跳與資料走同一條 TCP。** 大量的資料會讓心跳排隊；多條 channel 時更明顯。第 0 步以 TUN 模擬量過，最多多約 0.63 秒，30 秒夠用；nano4 上在第 7、11 步的驗收時再量一次。（2026-10-26：慢而掉包的路徑上會晚好幾秒；改成從對方來的任何東西都算，見進度的「第 7 步之後」。）
4. **改動大。** 所有建立連線、讀寫、accept、等子行程的地方都要改成帶 Scope；換來的是「失去連線時會不會返回」由型別保證。2026-10-26 使用者允許大幅重構，目標是程式簡潔、邏輯清楚、容易維護。
5. **client 被 Ctrl-Z 停住超過 30 秒，傳輸失敗，刻意和 rsync 不同。** client 停住時，它送心跳的 thread 也停了；host 分不出「client 被暫停」與「網路斷了」，而網路是要強保證的部分，所以照樣在 30 秒後判定 client 不在，收掉它啟動的一切。rsync 預設沒有逾時（`--timeout` 預設 0），Ctrl-Z 多久都能 `fg` 回來。曾考慮讓 client 攔下 Ctrl-Z（SIGTSTP），先告訴 host 的 monitor「暫停判斷」再停下，但暫停中若網路靜靜斷了，host 那頭就控制不了；使用者對此態度保留，不做，維持現狀（A2 之後就是如此）。

## 其他做法

- **每條連線都自帶 keepalive，帶不了的階段各加時限。** 沿 one-session 第 6、7 步的方向。好處是連線之間互不牽連；壞處是時限散在十幾處，每加一種連線或一個階段都得記得補一個，這次就是這樣漏掉的。不採用。
- **只以 host 的心跳管已建立連線上的讀取**（2026-10-26 的第一版）。等連線進來、等對方表明身分、寫、等子行程結束、心跳出現之前，仍各是特例。使用者指出範圍不足，改成上面的。
- **每個行程一個 monitor**（monitor 的第一版）。server 自己也有 Scope 樹與心跳；改成每個地方一個，server 只是 worker，一個地方的資源由一個頂層管，少一層。
- **調整流程，讓 host 盡量不等 client**（第 5 種原本的做法 B）。client 在第一個腳本裡把決定所需的都給 host，host 自己下載、裝好、exec 成 monitor，只有要上傳時才等。好處主要是速度：省下幾個來回，每個在 nano4 上約 40 ms，相對一個 session 的 1 到 2 秒很小；對健康的幫助只是等待少一點，上傳那段仍要靠雙向心跳；而且會改變沒有 `--yes` 時何時問使用者。要讓啟動更快，可以另外列成改進選項，和健康分開考慮。
- **生命線用另一條管道（3 號 fd）**：`sudo` 預設的 `closefrom` 會關掉它，`-C` 要 sudoers 設了 `closefrom_override` 才能用，不能指望；`sudo` 開了 I/O 記錄時，它也一樣失效。
- **生命線用 Linux 的 `PR_SET_PDEATHSIG`**：`sudo` 是 setuid 程式，exec 它時這個設定會被清掉；macOS 也沒有。
- **生命線用管道而不是 socket pair**：管道的讀端關掉時，Linux 上 poll 寫端會回報 `POLLERR`，不訂閱事件也會；macOS 卻要訂閱「可寫」才會回報 `POLLHUP`，而訂閱「可寫」平常就一直被叫醒，沒辦法安靜地等（第 0 步）。

## 待決定的問題

都已決定（2026-10-26）；依主題整理，細節在各節：

- [x] **範圍**：統一所有的等待，不只連線上的讀取（使用者要求）。
- [x] **原則**：狀態可知、操作可控；設計準則：盡可能減少額外規則（第 1 節）。
- [x] **健康以 host 為單位**，一個心跳：5 秒送一次，30 秒沒收到就算斷；暫時的上限、寬限也是 30 秒，整個設計只有一個沉默的值（第 7、10 節）。
- [x] **Monitor**：每個地方一個，host 的就是 launcher，改名為 `ferry monitor`；client 也要一個；server 是 worker（第 3 節）。
- [x] **host 的 monitor 結束它啟動的程式**（取捨第 1 點）。
- [x] **生命線**：worker 的標準錯誤，所有 worker 一樣，接成 socket pair；`sudo` 開了 I/O 記錄時回報不可靠（第 3 節）。
- [x] **看得到就等**，說出在等什麼，第二次 Ctrl-C 強制；寬限只給失去連線的 host；`LINGER`、`GIVEN`、`ENDING`、`close_within` 拿掉（第 1 節）。
- [x] **第二次 Ctrl-C** 只在第一次之後 3 秒內才強制（第 4 節）。
- [x] **分組**：經 master 的 `ssh` 與本機的 server 各自分組，登入用的 `ssh` 不分（第 4 節）。
- [x] **被暫停的行程**：心跳路上的當成失去連線；路以外的由 `Child` 回報，不設上限（第 1 節、取捨第 2 點）。
- [x] **Ctrl-Z**：維持現狀，client 停住超過 30 秒傳輸失敗（取捨第 5 點）。
- [x] **host 上 monitor 出現之前**：第一個 session 上的雙向心跳，host 那頭是 shell 的看門狗；做法 B 不採用（第 2 節）。
- [x] **安裝的那幾分鐘**：`alive` 行確認連線還在就夠（第 2 節）。
- [x] **本機的 `hand`**：`Wire` 建立時等對方回應的那一步；`TAKING` 拿掉（第 2 節）。
- [x] **狀態偵測**：做成完整的外部觀點，與行程自己說的合起來（第 5 節）。
- [x] **多條 channel**：和這份一起做；block 的順序照五點；K = min(8, max(1, ⌈RTT ÷ 15 ms⌉))（第 12 節）。

## 第 0 步：事前查證的結果（2026-10-26）

開始寫程式之前，使用者要求把要實測、要盤點的都先查一查。結果都已寫進上面各節；這裡是總表。實驗的程式在分支 `experiment/silent-drops-checks`（`experiments/silent-drops/` 與它自己的 workflow，只在那個分支上跑，不碰 main 的 CI；留到第 4、6 步做完，它們的測試可以借用，之後刪掉）；心跳的模擬在本機的 `.scratch/sim/beat.py`，沒有進 git。

| 項目 | 怎麼查 | 結果 | 改到的設計 |
|---|---|---|---|
| block 的順序 | 子代理讀程式碼盤點，關鍵處再抽查 | 沒有訊息要等 block；反而是 block 的結束要等第 0 條上的 `FileHash`、`Problem`；block 沒有對齊封包 | 第 12 節的五點 |
| 生命線：poll 的訊號 | 小程式，Linux 本機與 CI 的 Linux、macOS 15、26（arm64 與 x86_64） | 管道在 macOS 上沒辦法安靜地等；socket pair 的另一端關掉、或握著它的行程被 `kill -9`，等「可讀」在兩個平台都得到 `POLLIN|POLLHUP`，另一端還在時不會醒來 | 第 3 節：生命線用 socket pair |
| 生命線：經過 `sudo` | CI 的 Ubuntu 24.04（sudo 1.9.15）與 26.04（sudo-rs 0.2.13） | 預設與 `use_pty`：monitor 被 `kill -9` 後 1.5 秒內 worker 就發現；`log_output`：20 秒都沒發現（sudo-rs 不支援這個設定） | 第 3 節：失效時 worker 自己看得出來，回報 |
| macOS 的外部觀點 | CI 的 macOS 15、26，arm64 與 x86_64 | 讀得到同使用者的行程；thread 的執行中、等待；行程的被暫停；root 讀不到；不可中斷的等待造不出來 | 第 5 節 |
| 心跳排在資料後面的延遲 | 本機的 TUN 模擬，RTT 30、120、250 ms，buffer 預設與 64 MiB，0、1、8 條全速的 channel，兩個方向 | 最壞多約 0.63 秒 | 30 秒夠用（第 12 節） |
| K 的公式 | 用 redesign 5.2 節（2026-10-23）的資料 | 預設 buffer 下 4 條就到 TCP 的上限；buffer 調大時條數才有用；多開不會變慢 | 第 12 節的公式 |

## 計畫

每一步結束時，都要能執行、測試過。先做 monitor 與連線的健康，讓 stream 層變薄、每個等待都有 Scope；多條 channel 再長在上面：那時 stream 只剩封包與結尾標記，K 條只是 K 條 `Wire`。測試照原則分成「失去連線」「連線還在、事情卡住」「操作」三組。

1. **量測的基準。** 第 0 步已用 TUN 模擬量了心跳在大量資料下的延遲（`.scratch/sim/beat.py`），一條與多條 channel 的速度沿用 redesign 5.2 節的資料；這一步只把這些數字記成之後的對照，實作後（第 7、11 步）以同樣的方式再量。
2. **monitor 的核心。** client 與 host 的 monitor 共用的、不做 I/O 的狀態機：Scope 樹、三種計時、事件與動作；時間由外面給。以合成的事件與時間測試：失去連線、子行程被暫停又繼續、使用者的訊號（含 3 秒的窗）、worker 做完或失敗、對方送來「停」，各自得到的動作。還沒接上。同一步把 client 抓 release 的 curl 補上和 host 一樣的逾時與速度下限（第 2 節第 2 種）。
3. **觀察者：`Wire`、`Listener`、`Child`，與收訊號的 thread。** 先包住現有的連線與子行程，行為不變；transport 之外不再拿得到連線的 fd。`Child` 同時說出被暫停與繼續（第 1 節規則 2）。
4. **host 的 monitor 換上核心，並改名。**
   - `ferry launcher` 改成 `ferry monitor`，程式碼裡的 launcher 跟著改名。
   - worker 那段結束邏輯（control 結束、SIGTERM、生命線關掉、做完）寫成本機與 host 上的 server 共用的一段；worker 寫標準錯誤改用不 panic 的寫法。
   - 生命線：啟動 worker 時，標準錯誤接成 socket pair，monitor 收它並轉出去，worker 以 poll 等它可讀；worker 拿到的 2 號 fd 不是 socket 時（`sudo` 開了 I/O 記錄），回報生命線不可靠。
   - 心跳：30 秒內要被追蹤，然後是心跳。收到「停」：對它啟動的程式送 SIGTERM，等它們結束，收集各自怎麼結束，回報 client，不逾時。失去連線（沉默或連線結束）：SIGTERM，寬限 30 秒，到期就砍，然後自己結束。
   - 第一個 session 上的雙向心跳：client 每 5 秒送 `:`，腳本的看門狗 30 秒沒有動靜就結束 session。
   - 測試與量測：host 的 monitor 被 `kill -9` 時，worker 立刻停下，經 `sudo` 的也一樣（Linux 與 macOS）；`sudo` 開了 I/O 記錄時，worker 回報了生命線不可靠；monitor 那端沒有漏給別的子行程。在 nano4 與 fatcat 上確認看門狗砍得掉卡在讀取上的子行程，並量 worker 從 SIGTERM 到結束要多久，確認 30 秒的寬限夠。
5. **client 的 monitor 換上核心，server 換成 worker。**
   - `follow.rs`、`interrupt.rs` 的生命週期部分搬進 client 的 monitor，`serve` 的生命週期換成 worker 那段結束邏輯；每個等待改經 Scope。
   - 拿掉第 11 節列的：stream 層的 keepalive 與沉默判斷、`follow.rs` 對 control 的判斷、各處各自的時限與收尾常數。
   - 操作可控（第 4 節）：經 master 的 `ssh` 與本機的 server 放進自己的 process group（登入用的 `ssh` 不放）；本機的 server 由 client 的 monitor 以 SIGTERM 停下、等它結束，它的標準錯誤也由 client 的 monitor 收、轉到畫面；第一次 Ctrl-C 送「停」、等結果；3 秒內的第二次 Ctrl-C 對自己的子行程送 SIGKILL，立刻結束。
   - 等待超過約 1 秒就說出在等什麼。等一個人（密碼、OTP、要不要安裝）不設上限，其他 host 的心跳照常。
   - 測試照原則分三組，每個都先寫出會卡住的：
     - **失去連線**：在每個階段（登入、第一個 session、等被追蹤、被追蹤之後的傳輸、收尾）讓連線「不傳也不關」，以及讓心跳路上的每個行程被暫停：兩頭都在上限內收拾，host 上不留下行程，client 說出原因；
     - **連線還在、事情卡住**：心跳路以外的行程被暫停又繼續、server 停住（`FERRY_TEST_STALL`）、一個 server 做完而另一個遲遲沒做完、等一個人：不逾時，看得到（回報、或 60 秒的診斷），繼續之後照常完成；
     - **操作**：第一次 Ctrl-C，host 上的程式都停下、暫存檔收拾掉，client 說出每一頭的結果；3 秒內的第二次 Ctrl-C，host 讀到連線結束，立刻開始收拾，在寬限內結束；超過 3 秒才按第二次，不強制，只再說一次在等什麼；client 被 `kill -9`，host 在 30 秒內發現、再在寬限內收拾；host 的 monitor 遲遲不回覆「停」時，client 說出在等它，第二次 Ctrl-C 強制結束。
6. **狀態偵測。** `Child` 的外部觀點：Linux 讀 `/proc` 的 thread 狀態、`wchan`、`syscall` 與 fd；macOS 用 `proc_pidinfo`；其他平台只說它還在。snapshot 補上等的對象與進展計數；monitor 要 snapshot 時立刻附上外部觀點，在既有的時機顯示（第 5 節）。測試：一個所有 thread 都卡住、回不了 snapshot 的 worker，使用者照樣看到外部觀點；Linux 上看得到 D state 與卡住的檔案（以 FUSE 或停住的 NFS 模擬；做不到就以被暫停的狀態代替，並寫明）。
7. **monitor 與健康的文件與驗收。** architecture 新增 monitor 一節，改原則第 10 點、Connections、launcher 的部分；pipeline 裡的 launcher；CHANGELOG；本機、fatcat、nano4 的 `ferry test`，與第 1 步的心跳延遲再量一次。
8. **block 的順序。** 照第 12 節的五點。這時 K 仍是 1，行為不變。測試：以亂序送 block；`END(N)` 比最後幾個 block 先到；`FileHash` 與 `Problem` 在 `END(N)` 之前、block 之後到；序號重複或超出範圍；data stream 正常結束而第 0 條還沒有 `END(N)`。
9. **K 條 stream。** client 建 K 條；pump 有 K 個 data 佇列，把整個 block 交給佇列最短的一條，其他固定走第 0 條；每條一個 `Assembler`；任何一條被切斷就整個失敗。先以測試用的注入固定 K，測試 K=1、2、8，與其中一條被切斷。
10. **K 怎麼定。** 握手時量 RTT，照第 12 節的公式；用 TUN 模擬確認各 K 的速度與心跳的延遲。
11. **多條 channel 的文件與驗收。** architecture 的 Transport（原則第 4 點「只有一條 stream」）與 Connections；redesign 的改進選項標成已做；CHANGELOG；本機、fatcat、nano4，與 TUN 模擬。

## 附錄：已知的缺口

以下是 2026-10-26 讀程式碼時看到的，**不是完整的盤點**，也還沒逐一以測試確認；每一項後面是這份設計由哪裡補上。

1. **launcher 被追蹤之前，一條連線都沒來。** launcher 在第一個 session 上說出 socket 在哪裡之後，在 `Listening::accept`（`src/transport/join.rs`）裡等連線進來，`poll` 沒有逾時。若 SSH 連線此時靜靜斷了，client 的追蹤 channel 永遠到不了；launcher 看著第一個 session 的輸出，但 sshd 不會關它。`5fbdefa` 的時限要等連線進來才開始算，幫不上。被追蹤之後沒有這個問題：追蹤那條連線的 reader 在另一個 thread，20 秒沒聲音就結束整個行程。**補上**：`Listener` 的等待帶著 host 的 monitor 的 Scope，被追蹤之前它的 Liveness 是 30 秒的暫時上限（第 7 節）。
2. **server 等 `Hello` 與 `Proceed`。** server 在標準輸入上讀握手的 `Hello`，以及之後的 `Proceed`，都沒有時限（`src/peer.rs` 的 `serve`）。one-session 第 7 步只把握手**之後**的 control 改走 stream 層。**補上**：這是 `Wire` 的讀取，帶著 worker 的 Scope；失去連線時 host 的 monitor 送 SIGTERM，monitor 不在了則生命線關掉，兩者都讓它返回（第 3 節）。
3. **server 等 stream 進來。** server 收到 `Proceed` 之後在 accept 裡等 stream。one-session 第 7 步之後，control 的 reader 20 秒沒聲音就讓 server 結束，應該就補上了，要以測試確認。**補上**：同上，`Listener` 的等待帶著 worker 的 Scope。
4. **loopback port 上讀秘密。** `Listening::prove` 在自己的 thread 裡以 `read_exact` 讀秘密，沒有時限。一條不說話的連線會一直佔著 16 個名額（`UNPROVEN`）之一；名額用完，之後的連線都會被拒。**補上**：還沒表明身分的連線掛在 listener 底下，用暫時的上限（第 8 節）。
5. **client 端的幾個等待**，都經過 `LetGo`，它只看連線有沒有被放掉，沒有時限：
   - 讀第一個 session 的輸出：probe、安裝、launcher 說出位置的那幾行（`FirstSession::read_part`）；
   - 開一條 channel 後等 start mark（`Transport::connect` 裡的 `start::skip`）；
   - 握手時等 `Welcome`（`handshake::client_handshake`）；
   - launcher 跑到結束的程式（sudo 的檢查、fixture），讀到它的輸出結尾（`Launcher::run`）。

   **補上**：每個都是所屬 host 的 Scope 底下的 `Wire` 讀取，由那個 host 的 Liveness 給上限，來源隨階段換手（第 7、9 節）。

已有時限的，供對照：登入時等一個不回應的 ControlMaster（30 秒）；把 stream 的一端交給本機的 server（`hand`，20 秒）；stream 的讀寫；client 判斷每條 control connection；one-session 第 6 步之後，追蹤 launcher 的連線兩端；launcher 讀連線開頭的位元組與第一個封包，server 讀 stream 開頭的位元組（`2204943`）。

## 進度

### 第 1 步：量測的基準（2026-10-26）

之後的對照用這些數字，實作後以同樣的方式再量（第 7、11 步）。

**心跳在大量資料下的延遲**：本機的 TUN 模擬（`.scratch/sim/beat.py`，1 Gbps，不掉包），同一個 ssh master 上 K 條全速傳資料的 channel，另一條 channel 每 0.5 秒送一行、由對方原樣送回，記下每行來回的時間，各 34 次。「上」是 client 往 host 傳資料，「下」是反方向。單位 ms。

| RTT | buffer | 方向 | 沒有資料：中位數／最慢 | 1 條：中位數／最慢 | 8 條：中位數／最慢 |
|---|---|---|---|---|---|
| 30 | 預設（6 MiB） | 上 | 31.5／31.8 | 31.5／32.4 | 36.9／123.1 |
| 30 | 預設 | 下 | 31.4／31.7 | 31.1／32.0 | 36.6／43.8 |
| 120 | 預設 | 上 | 121.5／121.7 | 121.4／122.1 | 145.1／170.0 |
| 120 | 預設 | 下 | 121.3／121.5 | 121.4／122.0 | 142.2／172.9 |
| 250 | 預設 | 上 | 251.5／251.7 | 251.5／251.6 | 362.2／422.2 |
| 250 | 預設 | 下 | 251.5／251.8 | 251.3／251.6 | 346.4／408.4 |
| 30 | 64 MiB | 上 | 31.3／31.5 | 31.5／32.1 | 138.9／216.7 |
| 30 | 64 MiB | 下 | 31.4／31.5 | 31.4／32.4 | 138.9／139.5 |
| 120 | 64 MiB | 上 | 121.4／121.7 | 121.4／122.2 | 200.8／286.5 |
| 120 | 64 MiB | 下 | 121.4／121.6 | 121.4／122.2 | 230.6／329.1 |
| 250 | 64 MiB | 上 | 251.4／251.6 | 251.4／251.5 | 284.5／715.1 |
| 250 | 64 MiB | 下 | 251.4／251.6 | 251.4／251.5 | 251.6／885.3 |

**一條與多條 channel 的速度**：沿用 redesign 5.2 節（2026-10-23）的資料，第 12 節引了用得到的部分。

### 第 2 步：monitor 的核心（2026-10-26）

`src/monitor/mod.rs`：client 與 host 的 monitor 共用的狀態機，不做 I/O，時間由外面給；`src/monitor/tests.rs` 以合成的事件與時間測試失去連線（沉默、來不及、連線結束）、子行程被暫停又繼續、Ctrl-C 與 3 秒的窗、SIGTERM 不強制、worker 做完、失敗、沒說做完就結束、host 的 monitor 被告知停、寬限到期。還沒接上。client 抓 release 的 curl 補上 `--connect-timeout 10 --speed-limit 1024 --speed-time 30`（`src/remote/release.rs`）。

核心的形狀：Scope 樹的節點分成傳輸（client 的根）、host 的 monitor（host 的根）、host、worker、行程（承載連線的或做事的）與其他（連線、listener）；事件是方法（`heard`、`phase`、`lose`、`done`、`failed`、`worker_ended`、`exited`、`paused`、`interrupt`、`terminate`、`stop`、`tick`），動作由 `take` 取出：結束一個 Scope、送訊號、關掉 worker 的 control、對 host 送「停」、告訴使用者（或 client）、全部結束。

與計畫的差異，都是草稿沒有寫到、照原則補上的，實作接上時再確認：

- **還沒有連結的 host 被停時，切斷它。** 草稿只寫了第一次 Ctrl-C 在追蹤用的連線上送「停」；登入、第一個 session 那時還沒有這條連線，沒有地方送。核心照「連線不在時由失去連線的規則接手」處理：殺掉承載它的 `ssh`、不等，host 上的 shell 讀到輸入結束，或看門狗聽不到 client，自己結束。
- **worker 說做完了，就立刻關掉它的 control**，不等另一個也做完；現在的 `follow.rs` 是兩個都結束（或時限到）才一起關。worker 做完之後不再送東西，早關不影響結果。
- **一個 Scope 底下，連線比做事的晚結束**：傳輸正常結束時，host 的連結（追蹤用的連線）等那個 host 上的 worker 都結束了才關，這樣 host 的 monitor 來得及回報每個 worker 怎麼結束。
- **在等什麼只說一次**：開始等之後約 1 秒說一次，之後每按一次 Ctrl-C（3 秒窗以外）再說一次；草稿沒有說要不要每隔一段時間重複。
- **host 的 monitor 自己收到 SIGTERM 或 SIGHUP 怎麼辦，核心還沒有處理**：草稿沒有寫，第 4 步接上時再看。

### 第 3 步：觀察者（2026-10-26）

- `7c6a727`：`transport::scope::Scope`，等待在裡面等的那個 Scope：一個 socket pair，結束時關掉一端，每個在等的 poll 立刻醒來，說出原因，沒有計時器；`transport::child::Child`，Ferry 啟動的每個行程，由它自己的一個 thread 以 `waitpid` 的 `WUNTRACED`、`WCONTINUED` 等，說出被暫停、繼續與結束，也能在 Scope 裡或到期限為止等它。`ssh`、它的 holder、stream 的承載行程、本機的 server、launcher 啟動的程式都換成它；收到訊號結束時要結束的子行程，改經它送訊號，已結束的不會再收到。
- `c5b9711`：連線（`Link`）、stream 與 listener 都在 Scope 裡等：放掉一條連線、stream 被判定斷了，就結束它的 Scope，讀寫兩頭立刻醒來，不再每 250 ms、每秒回頭看一次；control connection 的連線與它上面的 stream 共用一個 Scope。

與計畫的差異：

- **收訊號的 thread 還沒有改**：它要送事件給 client 的 monitor，而第一次 Ctrl-C 送「停」、3 秒的窗都是第 5 步的行為；現在先改，就是沒人用的程式。移到第 5 步。
- **「transport 之外拿不到連線的 fd」還沒做到**：`peer.rs` 與 `launcher.rs` 仍直接拿標準輸入輸出與 `Joined` 的 fd。這兩處在第 4、5 步本來就要重寫（host 的 monitor、worker 那段結束邏輯），到時一起收進 transport。
- **還沒改名成 `Wire`**：`Link` 與 stream 層在第 5 步拿掉 keepalive 之後才是同一串，那時合成 `Wire`。
- **`Child` 說得出被暫停與繼續，但還沒有人聽**：接到 monitor 是第 4、5 步的事。
- **一個測試改了**：`the_way_not_kept_closes` 在整套測試一起跑時偶爾失敗：別的測試啟動子行程時，子行程在執行它的程式之前，短暫握著這個 process 所有的 fd，剛關掉的 port 還連得上。改成在 1 秒內等到連不上為止。

### 第 4 步：host 的 monitor（2026-10-26）

| commit | 內容 |
|---|---|
| `e71e051` | launcher 改名為 monitor：`ferry monitor`、`monitor::host`、`transport::monitor`、`protocol::Monitor`；dashboard 原本叫 `Monitor` 的型別改叫 `Traffic`。設計文件留到第 7 步 |
| `32af27d` | 生命線：monitor 啟動的程式，標準錯誤接成 socket pair；`ferry server --lifeline` 以 poll 等它可讀，monitor 一不在就像收到 SIGTERM 一樣停下；標準錯誤不是 socket 時，告訴 client 生命線不可靠。寫標準錯誤失敗不再 panic |
| `5bc99cf` | host 的 monitor 換上核心：追蹤用的連線上，兩頭每 5 秒送一次心跳，30 秒沒收到算斷，這條連線不再用 stream 層的 keepalive 與沉默；收到「停」就對它啟動的程式送 SIGTERM，回報每個怎麼結束，連線保留；它啟動的程式被暫停或繼續時，在連線上說出來；失去連線（沉默、連線結束、還沒被追蹤就沒了第一個 session）時送 SIGTERM，給寬限，砍掉剩下的，然後結束 |
| `333d31e` | 第一個 session 的雙向心跳：host 每拍印一行 `alive`；client 每拍送一行 `:`；看門狗 30 秒沒有動靜就結束整個 session；client 由一個 thread 讀第一個 session 說的一切，30 秒沒聽到就判定不在 |
| `abacf59` | 驗收找到的：心跳的兩個迴圈被結束時，它們正在跑的 `sleep` 還握著 session 的輸出，sshd 因此不關 session，每次傳輸結束 client 都要等滿 5 秒 |

測試：monitor 被 `kill -9`，它啟動的 server 在等 hello 時就停下（拿掉 `--lifeline` 時這個測試失敗）；monitor 被告知停、它啟動的程式被暫停又繼續、失去連線後在寬限內收拾（包括訊號處理延遲 5 秒、到寬限才被砍的 worker）；看門狗、有心跳的 session 照常、一個什麼都不說的第一個 session 在沉默之後被放棄。

驗收（fatcat，2026-10-26）：

- `ferry test` 從本機到 fatcat、fatcat 到 fatcat、從 fatcat 到本機加 `--remote-join`，各 39 個全過；之後 fatcat 上沒有留下 monitor、server，也沒有看門狗的檔案。
- 和這項工作之前的 build（`5cef29e`）輪流跑，1 KB 的複製各 10 次：之前中位數 0.21 秒（0.19–0.22），現在 0.20 秒（0.20–0.21）；`ferry test` 從本機到 fatcat 各兩次：之前 7.7、7.7 秒，現在 8.7、7.8 秒，每個情境的中位數都是 0.15–0.16 秒。第一次跑時 fatcat 負載 11–23，`ferry test` 要 18 秒，不同時間的數字不能比。
- 看門狗在真的 sshd 下：fatcat 的登入 shell 是 bash，`bash -c` 只有一個指令時直接 exec `sh`，所以 `sh` 是 session 與 process group 的首領；靜默 3 秒、每拍 1 秒時，4.1 秒後整個 session 結束，連同在讀輸入的 `cat`。
- worker 從 SIGTERM 到結束：複製 1 GB 到 fatcat 途中，對 Destination 的 server 送 SIGTERM，6 次都在 61–97 ms 內結束，沒有留下暫存檔；30 秒的寬限綽綽有餘。
- nano4 還沒驗收：要使用者的 master 與 OTP。

與計畫的差異：

- **client 這頭的心跳與判斷，暫時是 `transport::monitor` 裡的兩個 thread**，不是 client 的 monitor 的核心；client 的核心在第 5 步接上。
- **host 回報的被暫停、繼續與還在等什麼，client 先收下不顯示**：顯示給使用者是 client 的 monitor 的事，第 5 步。
- **上傳完之後，看門狗就停了**：上傳讀到 client 輸入的盡頭，之後 host 再也聽不到 client。草稿只寫了上傳時檔案變大也算動靜；之後在 host 上建 dev build 的那段，靠 cargo 自己的逾時，然後 monitor 的 30 秒暫時上限（等不到被追蹤就結束）。
- **下載時，正在寫的檔案變大也算動靜**，和上傳一樣；草稿只寫了上傳。
- **dev build 的原始碼先寫成一個檔案再解開**，這樣才看得到它變大；原本是直接 `tar` 讀輸入。
- **host 的 monitor 自己收到 SIGTERM 或 SIGHUP，立刻結束**，照舊；它啟動的程式由生命線發現它不在了而停下。第 2 步留下的問題就此回答。
- **第一個 session 在失敗時被丟棄，會被切斷**（它的 `ssh` 被結束），原本會留著。
- **測試用的 relay 可以只卡住一個方向**：「monitor 不再有消息時 client 說它不在了」那個測試只卡住 host 往 client 的方向；monitor 現在失去連線會停掉它的 server，兩邊都卡住時，client 可能先聽到 server 結束。
- **`sudo` 開了 I/O 記錄時回報不可靠，沒有在這裡測**：這台機器沒有 sudo。第 0 步在 CI 上確認過 worker 這時拿到的是管道；這裡只測了「標準錯誤不是 socket 時看得出來」。

### 第 5 步：client 的 monitor，server 換成 worker（2026-10-26）

| commit | 內容 |
|---|---|
| `e60a03c` | Scope 排成樹：在另一個底下開的，跟著它結束，原因相同；host 的 Scope 一結束，它所有連線上的等待都返回 |
| `f518fb0` | client 的 monitor（還沒接上）：一個 thread 驅動核心，從每個 Scope 的 `Node` 聽事件，也聽使用者的 Ctrl-C、SIGTERM、SIGHUP（monitor 跑的時候，它們交給它，不再直接結束行程）；照核心說的結束 Scope、送訊號、關 control、對 host 送「停」、告訴使用者在等什麼與誰被暫停。強制時立刻結束；被訊號停下時，照那個訊號結束 |
| `d69c958` | 第一個 session 的心跳隨指令迴圈讀到輸入的盡頭就停，不再在 shell 結束後還握著輸出一拍；probe 的測試原本每個多等 5 秒（lib 測試 35 秒變回 4 秒） |
| `43fb512` | client 的 monitor 接上傳輸：每個 host、server、client 啟動的行程都是它的 Scope；host 的 Liveness 先是登入，再是第一個 session 說的每一行，再是被追蹤之後的心跳，由核心判斷；server 是 worker；host 上都做完了，才關掉追蹤用的連線；本機的 server 也有生命線與自己的 process group，經 master 的 session 與 channel 也是；control 連線與 stream 不再有 keepalive 與沉默；`follow` 不再決定傳輸何時結束 |
| `5d064cd` | 拿掉 stream 層的 keepalive、沉默判斷、keeper，以及各處的收尾時限（`ENDING`、`close_within`、`Streams::end`）；dashboard 上 SSH 連線的「多久沒聽到」改看 host 的心跳 |
| `09a8552`、`5b29816` | 操作的測試：3 秒內第二次 Ctrl-C 立刻結束、超過 3 秒只再說一次在等什麼、對 host 的 Ctrl-C、client 被 `kill -9` |
| `a58fdc9` | 問使用者的問題（要不要安裝、要不要同步進家目錄）照樣不設上限，但第一次 Ctrl-C 就結束等待 |

測試，照原則分三組：

- **失去連線**：一個什麼都不說的第一個 session 在沉默後被放棄；一個 host 的所有 channel 一起卡住，client 在心跳沉默後判定它不在了（原本的測試只卡住 control 連線，那不是真實的情況：同一條 SSH 連線的 channel 一起斷）；追蹤用的連線只卡住 host 往 client 的方向。
- **連線還在、事情卡住**：本機的 Destination 被 SIGSTOP，client 說出它被暫停並等，SIGCONT 之後複製照常完成（原本的測試期待它在沉默後失敗，照取捨第 2 點改了）。
- **操作**：本機複製被 Ctrl-C，server 收拾暫存檔，以 130 結束；3 秒內的第二次 Ctrl-C 立刻結束，被暫停而收不到 SIGTERM 的 server 被砍；超過 3 秒的第二次只再說一次在等什麼；對 host 的 Ctrl-C，host 上的 server 與 monitor 都停下；client 被 `kill -9`，host 的 monitor 收拾它的 server 再結束。

驗收（fatcat，2026-10-26，負載 13–16）：`ferry test` 三種組合各 39 個全過，之後 host 上沒有留下任何東西；和 `5cef29e` 輪流跑 1 KB 的複製各 10 次，中位數都是 0.20 秒（新的 0.19–0.40，舊的 0.18–0.20）。

與計畫的差異：

- **核心多了兩條**：一個 host 在依序結束時失去連線，照樣切斷它（不再等它的 worker 回報，它們不會再有消息）；一條連結（Plain）開始依序結束時，先送 Close 讓 driver 關掉追蹤用的連線，等承載它的行程都結束才算結束。沒有後者，連結要等行程結束，行程要等連結關掉，互相等住；這是第一次在 fatcat 以外的假 ssh 上跑整套時找到的。
- **各個 place 留到 monitor 說結束為止**：`prepare` 把它們交回，client 在 monitor 結束後才丟掉；原本 `follow` 結束時就丟掉，host 的 monitor 的 handle 在丟掉時關掉了追蹤用的連線，搶在 host 回報 server 怎麼結束之前。
- **client 自己找到的失敗（準備時被拒絕之類），由丟掉 control 連線告訴 worker 停下**：關 control 的 closer 只握弱參照，連線丟掉就斷，server 照舊停下；核心不另外送 Close。
- **本機 server 的標準錯誤轉到 client 的標準錯誤**，和以前一樣，沒有轉進 dashboard。
- **`ControlConnection::close` 留著**，給測試用，沒有時限。
- **「transport 之外拿不到連線的 fd」還是沒做到**：`peer.rs` 與 `monitor::host` 仍直接拿標準輸入輸出與 `Joined` 的 fd；**也還沒改名成 `Wire`**。這兩件和健康無關，留著，列在已知的缺口裡。
- **`ferry test` 有它自己的 client monitor**，用來到達 host、跑 fixture；不接管訊號，照它原本的方式處理 Ctrl-C。
- **問問題時第一次 Ctrl-C 就結束等待，沒有測試**：問題只在終端機上問，測試裡沒有終端機。
- **fuzz 的 target 漏了第 4 步的改名**（它們不在 workspace 裡），這一步才補上。

### 第 6 步：狀態偵測（2026-10-26）

| commit | 內容 |
|---|---|
| `a26671b` | 外部觀點（`monitor::look`）：Linux 從 `/proc` 讀每個 thread 的狀態、`wchan`，不可中斷的等待時再讀 `syscall` 與它的 fd；macOS 以 `proc_pidinfo` 讀行程是否被暫停、每個 thread 執行或等待；其他平台只說它還在。host 的 monitor 在 client 經追蹤用的連線問時，說出它啟動的程式的外部觀點；client 自己的行程，client 直接讀。在說出等誰、worker 被暫停或繼續、`Ctrl-\`、60 秒沒有進展時說出來 |
| `e7c6922` | snapshot 補上等的對象與進展：reader 開的檔案、writer 寫的檔案、scan 與比對開與列的目錄、finisher 與 deleter 的目錄；每個 thread 的步數 |
| `68ad57e` | `wchan` 的名字說得出時，說出在等什麼（NFS、FUSE、SMB、Lustre、Ceph）；已結束、行程還沒結束的 thread 說成 ending；Ferry 的 thread 以角色命名，外部觀點與 `ps -L` 看得到是哪個 thread；以 FUSE 測 D state |
| `39434bd` | host 上被暫停的 server：client 說出它被暫停，與 host 的 monitor 看到的外部觀點 |

測試：

- **一個回不了 snapshot 的 worker**：本機的 Destination 被 SIGSTOP 之後按 `Ctrl-\`，它回不了話，使用者照樣看到「the Destination on this machine, from outside: 72 threads, 72 stopped」，Source 照常回 snapshot（含步數）；host 上的 server 被 SIGSTOP，client 印出 host 的 monitor 讀到的外部觀點；host 的 monitor 被問時回報它啟動的程式，被暫停時每個 thread 都是 stopped。
- **D state 與卡住的檔案，以真的 FUSE 測**：測試在自己的 user 與 mount namespace（`unshare -rm`）裡，直接在 `/dev/fuse` 上講 FUSE 協定，掛一個檔案系統，其中一個檔案的 read 永遠不回；先讓一次被中斷的 read 收到 ENOSYS，核心之後就以只有致命訊號能結束的方式等（D）。複製它時，使用者在 60 秒（測試裡 2 秒）沒有進展時看到：Source 自己說「reader ×1: read files, for 4.26 s, on stuck」；外部觀點說「reader is in a wait nothing interrupts, in the kernel at request_wait_answer, which waits for a FUSE filesystem, in pread64() on …/mnt/stuck」；Ctrl-C 之後，等 Source 結束時再說一次。這個測試要 `unshare`、`/dev/fuse` 與 `python3`，標成 ignored，以 `--ignored` 跑；預設的測試以被暫停的狀態代替。

與計畫的差異：

- **被問時，host 的 monitor 說出它啟動的每一個程式**，不只被問的那個：協定上的 `Look` 不帶是哪一個。一個 worker 被暫停時，同一台 host 上的另一個也會被說出來。
- **「失敗的時候，附上最後一份狀態」沒有另外保存一份**：失敗時還在跑的，monitor 會停下並等它們，那時照「說出等誰」附上外部觀點；先前讀到的不再重說，失去連線的 host 也讀不到了。
- **`Ctrl-\` 在 profiling 關掉時也說出外部觀點**；snapshot 照舊只在 profiling 開著時要。
- **等的對象只在路徑就在手邊的地方**：Source 讀檔案的一段（segment）時說不出檔案，那時只握著打開的檔案；Destination 寫一段時只說得出檔名，不是完整路徑；等連線的 thread 不另外說對象，它的角色（pump、receive）已經說了是哪條。
- **進展計數是步數**：每個 thread 在「執行」與「等工作」以外的狀態待過的次數（讀一個檔案、寫一次連線、等一次別的階段），不是每個角色各自的項目數；兩份 snapshot 的步數一樣，就是沒有動。
- **新增：thread 以角色命名**（主 thread 除外，它的名字是行程的名字），否則外部觀點只能說「ferry is in a wait…」。Linux 最多 15 個位元組。
- **新增：worker 自己的行程以 worker 的名字稱呼**；原本被暫停時說「its process was stopped」。
- **新增：`wchan` 翻成在等什麼**，草稿只舉了 NFS 的例子；只認名字明確的幾種，其他照實說出函式名。
- **強制結束之後，卡在 D 的行程可能還在**：FUSE 的 read 已經交給檔案系統時，連 SIGKILL 都結束不了它，要等檔案系統回答或連線中斷。實測時第二次 Ctrl-C 讓 client 以 130 結束，那個 Source 留下，client 沒有說。要說，得在強制時再看一次被砍的行程；強制就是不再等，所以先記在這裡。
- **macOS 只做了型別檢查**：在這台機器上，`cargo check --target aarch64-apple-darwin` 整個 Ferry 會卡在 mimalloc 要 macOS SDK；把 macOS 的那段（`proc_pidinfo`、thread 命名）放進一個只依賴 `libc` 的小 crate 檢查通過。真正在 macOS 上跑，要等下一次 push 的 CI（`a_process_is_seen_waiting_then_stopped_from_outside`）。
- **實驗分支 `experiment/silent-drops-checks` 還在**：第 4、6 步做完了，照計畫可以刪；它也在 origin 上，刪掉要 push，留給使用者決定。

### 第 7 步：文件與驗收（2026-10-26）

| commit | 內容 |
|---|---|
| `24e0520` | 附錄的第 4 個缺口：經 loopback port 進來、遲遲不給秘密的連線，等 30 秒（和 socket 等第一個位元組一樣）就放掉，不再無限期佔著 16 個名額之一 |
| `cdf1451` | 文件：architecture 新增 Monitors 一節（每個地方的 monitor 管什麼、不做 I/O 的核心、每個等待都在 Scope 裡、host 的 Liveness、server 是 worker 與生命線、看得到就等、使用者的操作與看到的）；原則第 10 點改成健康只由 monitor 判斷，stream 與 control 沒有 keepalive；launcher 都改成 monitor，三張圖也是；pipeline 的 keeper 換成生命線；dashboard 的 heard 改看連結；profiling 的 snapshot 補上對象與步數；cli 的 Ctrl-C、`ferry monitor`、`ferry server --lifeline`；中文版同步；CHANGELOG 的 Unreleased 改寫成做好的樣子（launcher 與 20 秒 keepalive 的條目都還沒發布，所以改寫，不另加相反的條目） |
| `6a2d218` | `FERRY_TEST_BEAT_LOG`：debug build 裡，每個心跳在兩端送出與收到時，以單調時鐘記下時間；兩端在同一台機器、中間以模擬的網路相連時，量得到單程延遲。第 10 步量各 K 時也用它 |

驗收（2026-10-26）：

- `ferry test`：本機 36 個過、2 個照例略過（兩邊都在本機時沒有 host 上的 server）；fatcat 三種組合（本機到 fatcat、fatcat 到 fatcat、fatcat 到本機加 `--remote-join`）各 39 個全過；nano4 三種組合各 39 個全過，經使用者自己的 ControlMaster，不必 OTP。之後兩台 host 上都沒有留下 monitor 或 server。
- 和這項工作之前的 build（`5cef29e`）輪流跑 1 KB 的複製：fatcat 各 10 次，中位數都是 0.20 秒（負載 11）；nano4 各 8 次，中位數都是 1.275 秒（登入節點負載約 20；舊 build 有一次 4.07 秒）。

**心跳延遲再量一次。** 方法和第 1 步不同：第 1 步量的是裸 channel 上一行字的來回，旁邊是 `cat /dev/zero` 的資料 channel；這次量真正的實作，在 TUN 模擬（1 Gbps）上複製一個隨機內容的檔案（K=1），以 `FERRY_TEST_BEAT_LOG` 記下每個心跳的單程延遲，心跳每秒一次（`FERRY_TEST_SILENCE=6`），每格 33 到 37 個。「同向」是心跳與資料同方向（上傳時 client 的心跳、下載時 host 的），「反向」是另一個方向。單位 ms，中位數／最慢：

| RTT | buffer | 方向 | 速率 | 同向 | 反向 |
|---|---|---|---|---|---|
| 30 | 預設 | 上 | 62 MB/s | 15.9／52.3 | 15.5／52.3 |
| 30 | 預設 | 下 | 62 MB/s | 16.0／96.7 | 15.5／97.3 |
| 120 | 預設 | 上 | 14.9 MB/s | 60.8／201.7 | 60.7／61.0 |
| 120 | 預設 | 下 | 14.8 MB/s | 60.7／200.8 | 60.7／61.0 |
| 250 | 預設 | 上 | 6.5 MB/s | 125.8／429.5 | 125.8／125.9 |
| 250 | 預設 | 下 | 6.5 MB/s | 125.8／429.2 | 125.7／126.0 |
| 30 | 64 MiB | 上 | 61.8 MB/s | 15.8／119.8 | 15.5／119.6 |
| 120 | 64 MiB | 上 | 14.8 MB/s | 60.8／205.2 | 60.8／61.9 |
| 250 | 64 MiB | 上 | 6.5 MB/s | 125.8／444.0 | 125.7／126.3 |

（64 MiB 的下載方向與上傳對稱，數字幾乎一樣。）乾淨的連線上，心跳單程延遲的中位數就是 RTT/2，沒有排隊；最慢的一次在傳輸剛開始時，約 1.7 個 RTT。

**但第 12 節的假設不成立。** 第 12 節說「更慢的鏈路上，排在 TCP 送出佇列裡的資料量大約和擁塞視窗相當，換成時間仍是 RTT 的量級，但沒有量」。這次量到兩個反例：

1. **連線途中掉過包。** 第一次量測時，模擬腳本讓同一條 SSH 連線跨過鏈路的變更（RTT 從 30 ms 換成 120 ms，中間掉了一些包）。之後在 120 ms 上傳時，client 的心跳最慢晚到 3.8 秒，再慢慢降回 60 ms；250 ms 時超過測試用的 6 秒靜默，host 判定 client 不在了，傳輸失敗。刻意重現 4 次中 3 次如此。從 `/proc/net/tcp` 看：TCP 送出佇列一直是滿的約 2 MiB（一條 channel 的 window），ssthresh 被壓到 31 個封包，cwnd 從 36 個封包開始每秒只長一點；心跳排在這 2 MiB 後面，以 cwnd/RTT 的速率送出，所以要好幾秒。
2. **會掉包的長路徑。** 新的連線，RTT 250 ms、掉包 1%，真正的 30 秒靜默（心跳每 5 秒）：速率約 0.1 MB/s，心跳同向延遲中位數 6.2 秒，最慢 9.2 秒（34 個）。這次沒有到 30 秒，傳輸成功。

所以心跳延遲沒有被 RTT 限住，而是約等於「排在這條 TCP 前面的資料 ÷ 當下的速率」：前面最多是送出 buffer（預設上限 4 MiB，調過的 host 64 MiB）與各 channel 的 window 中較小的那個。速率因掉包或擁塞掉下來時，延遲可以到幾十秒；K 條 channel 時，前面可排的量是 K 倍。延遲超過 30 秒，就會把還活著的連線誤判成斷了。同樣地，「停」從 client 送到 host、host 的回報送到 client，也都排在這些資料後面，使用者的 Ctrl-C 在慢的上傳中會晚好幾秒才到 host。這要使用者決定怎麼處理，第 8 步之前先停下。

與計畫的差異：

- **心跳延遲換了量法**（上面），數字不能直接和第 1 步的來回比；第 1 步的 1 條資料 channel 那一欄，來回也是 RTT，兩者一致。
- **補了附錄的第 4 個缺口**（`24e0520`），它原本屬於第 3 步的 Listener，那時漏了。
- **CHANGELOG 改寫既有的 Unreleased 條目**，而不是另加條目：launcher、20 秒 keepalive 都在這次發布之前就被取代了，使用者從 0.16.0 看過來，只會看到最後的樣子。
- **nano4 經使用者自己的 ControlMaster 驗收**，不必 OTP；沒有另外開連線。

### 第 7 步之後：從對方來的任何東西都算心跳（2026-10-26）

使用者在第 7 步的量測之後決定（兩個做法中只做第一個）：判斷一個 host 在不在時，從對方來的任何東西都和心跳一樣算數，資料也算；送心跳的一方不變，照舊每 5 秒。第二個做法，在 peer 之間以回條限制在途的資料量，讓「停」與回報不必排在資料後面，使用者接受這個延遲，之後要求列為改進選項，寫在 [architecture-redesign.md](architecture-redesign.md#改進選項) 的「限制 SSH 連線裡在途的資料量」。

做法，沿用既有的「從對方聽到」與 server 每秒的 Carried 回報，沒有新的計時器：

- **核心**：`heard` 帶著聽到的時間，只往後移，不往前；晚一秒才說的也照它來的時間算。
- **client**：某個 host 的 server 的 control connection 上來的任何封包，算聽到那個 host；任一 server 回報它的 stream 剛收到東西，算聽到它 stream 另一端那個 server 所在的 host。本機對 host 時，本機 server 的回報走本機管道，不排隊；host 對 host 時，B 收得到資料，就是 A 和 A 的 SSH 連線都還在。`Transport::heard_from` 是唯一的入口，儀表板的 heard 也看它。
- **host**：server 每秒在它的生命線（標準錯誤）上寫一行加了標記的行（`start::frame` 的格式，標籤 `lifeline`），說它的 stream 多久前收到東西；monitor 讀標準錯誤時把這種行取出，不轉給使用者，算成聽到 client。走標準錯誤，是因為 `sudo` 會保留它。本機的 server 也寫，client 讀它的標準錯誤時同樣取出（client 已經從 control 收到同樣的事）。

測試：

- 核心：晚到的消息照它來的時間算，也不讓已聽到的時間往回走。
- 生命線：標記行不論怎麼被切開都被取出、說出多久前；像標記行但不是的，原樣轉給使用者。
- 整合（`a_heartbeat_behind_the_data_takes_nothing_for_lost`）：假 ssh 讓 stream 每秒只送 256 KiB，並在 stream 開始時把連結卡住 5 秒（靜默設 3 秒）。push 時卡住往 host 的方向，host 的 monitor 靠 server 的標記行撐過；pull 時卡住往 client 的方向，連 host 上 server 的 control connection 也卡住，client 靠本機 server 的回報撐過。兩個都完成，內容一樣。拿掉這次的改動時，兩個都失敗（push：「the connection to it ended」，host 的 monitor 先放棄了；pull：「nothing came from it for 3 s」）。
- 測試用的 relay 多了三件事：卡住的檔案拿掉就繼續（原本一卡住就到被結束為止，既有測試不受影響）、`FERRY_TEST_RELAY_STALL_WAY=up` 只卡住往 host 的方向，以及 `FERRY_TEST_RELAY_PACE` 限速。

與計畫的差異與附帶的：

- **對方的 monitor 被暫停時**：資料還在流的時候發現不了，要等那頭安靜下來；monitor 被殺則連結立刻結束，不受影響。使用者知道這個代價。
- **修了一個測試的競態**：`servers_of` 等 client 有兩個子行程就把它們停住，其中一個可能是剛 fork、還沒 exec 成 server 的；停在 exec 之前，client 拿不到它的 PID，強制結束時砍不到，它還握著 client 的標準錯誤，測試就一直等。這次在整套測試裡遇到一次。改成等子行程真的是 `ferry server`。
- **一次沒有重現的失敗**：同一次整套測試裡，`a_panic_is_an_internal_error_that_ends_the_transfer` 失敗了一次（結束碼不是 101），訊息沒有留下；之後單獨跑 3 次、copy 整個跑 3 次、整套跑 1 次都過。原因不明。


驗收（TUN 模擬，RTT 250 ms、掉包 1%，靜默設 6 秒，心跳每秒，12 MiB 的隨機內容）：上傳 176 秒、下載 166 秒，都完成。和資料同方向的心跳，上傳時中位數晚到 6.4 秒、最慢 9.2 秒，下載時中位數 4.3 秒、最慢 10.9 秒，都超過 6 秒的靜默；反方向的心跳照常（中位數 125.5–125.8 ms）。改動之前，同樣的延遲會讓還活著的連線被當成斷了。

### 第 8 步：block 的順序（2026-10-26）

| commit | 內容 |
|---|---|
| `0d2ae29` | 照第 12 節的五點，K 仍是 1：block 的前綴多了 8 個位元組的序號（`block::number`／`block::sequence`），由 Source 送 block 的那一段依序填入；data 裡的結束標記拿掉，改成第 0 條上的訊息 `BlocksEnd(N)`（封包類型 12），和 `FileHash`、`Problem` 同一個訊息佇列，所以排在它們後面。Destination 依序號把 block 放回 pool（`Submitter::submit_at`）；序號重複、或超出 N 的拒絕（`Numbers` 只一個一個記住跳號之後到的，其餘只記一個數）；`FileHash`、`Problem`、`BlocksEnd(N)` 依到達順序經同一個 channel 交給 decider，雜湊不再經過解碼的 pool；decider 數 block，N 個都到了才算結束；block 不再來時，先把第 0 條已經說的拿完，再判斷是不是被切斷 |
| `0b75bdd` | 驗收時找到的、第 5 步就留下的問題：stream 的讀取端在 Scope 結束時，先把已經到的讀完再說結束。server panic 時先在 control 上說出原因再以 101 結束，client 的 monitor 看到它結束、結束了它的 Scope，讀取端就不再讀，原因丟了，只說「an internal error, a bug in Ferry」。負載下 20 次失敗 7 次，改了之後 0 次 |

測試：亂序送 block、`BlocksEnd(N)` 比最後幾個 block 先到、雜湊比它的分段先到，檔案照樣寫好；序號重複、在 `BlocksEnd` 之前或之後超出、說了兩次結束、說了 2 個卻只到 1 個，都失敗並說出原因；decider 在 `BlocksEnd(2)` 只到一個 block 時不結束，第二個到了才結束，之後 block 不再來不算被切斷。Source 的測試另外檢查序號從 0 依序、和 `BlocksEnd` 說的數量一樣。fuzz 的兩個 target 跟著改（以 `--cfg fuzzing` 檢查過能建）。

量測：和前一個 commit 輪流跑，各 4 輪，tmpfs 到 tmpfs：1 GiB 的檔案，之前 0.56–0.68 秒，現在 0.44–0.56 秒；20 萬個 4 KiB 的檔案，之前 1.48–1.78 秒，現在 1.46–1.72 秒。沒有變慢。

與計畫的差異：

- **K 個 data 佇列沒有在這一步做**：第 1 點的「pump 有 K 個 data 佇列，由送 block 的那一段選」屬於第 9 步，這一步只讓送 block 的那一段負責編號。
- **block 不再來時的判斷**：草稿說「第 0 條結束了還沒看到 `END(N)`，或某條斷了，才是被切斷」。做法是讓 decider 在 block 的 channel 關掉時，先把第 0 條那個 channel 裡已經有的事件拿完，再看到的數量夠不夠；`BlocksEnd(N)` 一定在 block 的 channel 關掉之前送出，所以不會誤判。
- **pump 的「data 夠滿就先送 data」不變**：K=1 時照舊。

### 第 9 步：K 條 stream（2026-10-26）

| commit | 內容 |
|---|---|
| `29f8361` | client 建 K 條 stream，每條開頭的標頭帶它的編號（0 就是原本的那一條），到 host 時各是同一條 SSH 連線上的一條 channel；在 `Proceed` 裡告訴兩個 server 有幾條。server 依編號收齊。第 0 條帶 block 以外的一切；Source 的 mux 對第 1 條以後每條有一個 lane 與一個 pump，只帶 block，每個 block 整個交給排隊的 data 封包最少的一條（同樣少時選前面的），第 0 條的 pump 照舊讓訊息先走、data 夠滿時 data 先於 manifest。Destination 每條一個讀取 thread、各自一個 `Assembler`，block 依編號交給同一個解碼的 pool，序號的檢查跨各條共用；第 1 條以後送來 block 以外的東西就失敗。各條送收的量加總給儀表板與「從對方來的東西」。K 由 `FERRY_TEST_STREAMS` 決定（只在 debug build），預設 1；第 10 步改成照 RTT |
| `9c2d80d` | 測試找到的、第 5 步就留下的競態：worker 的結束要等它在 control 連線上說的都讀完，才算知道。原本行程結束（由看著行程的 thread 或 host 的 monitor 得知）與它說的失敗原因（由讀 control 的 thread 得知）各走各的路，誰先到誰算數；負載下常常只說「server 以 1 結束」，說不出原因。control 連線握手成功時告訴 client 的 monitor 它正在被讀，讀取端放掉時告訴它讀完了；從沒被讀的，不等 |

測試：本機以 2 條與 8 條複製、再同步一棵有 1,000 個小檔與一個 10 MiB 大檔（分段）的樹，內容一樣；經 host（假 ssh 轉送 channel）以 8 條推、拉、兩個 host 之間，確認推送時開了 8 條 stream 的 channel（加上連結與 control 共 10 條）；4 條中切斷第 0 條或第 2 條（`FERRY_TEST_CUT_STREAM`），本機與經 host 都失敗、說出是 stream 出了問題、不留暫存檔。核心：worker 被讀時，行程結束不算結束，它說的失敗原因先到就是原因，讀完才以「沒做完就結束」判定。經 host 切斷第 0 或第 1 條各 5 次，每次都說出原因（改之前有時只說以 1 結束）。

與計畫的差異：

- **K 放在 `Proceed` 裡**（新的 `protocol::Proceed { guard, streams }`），不在 `Hello`：K 要等量過 RTT 才知道（第 10 步），而 `Proceed` 是 stream 之前最後一則。
- **「佇列最短」以排隊的 data 封包數比較**，第 0 條算的是它自己的 data 佇列，不含訊息與 manifest。
- **Source 不讀第 1 條以後的 stream**：Destination 在那些上面只寫結尾；Source 握著它們到結束，不讀，所以斷了也只在寫的時候發現（寫不出去就失敗），Destination 那邊則在讀的時候發現。
- **測試用的注入多了兩個**：`FERRY_TEST_STREAMS`、`FERRY_TEST_CUT_STREAM`。

### 第 10 步：K 怎麼定（2026-10-26）

| commit | 內容 |
|---|---|
| `7f96436` | client 在每個 host 的連結上對 monitor 送三次 `Ping`，monitor 立刻回 `Pong`，取最短的來回；傳輸對最遠的 host 的 RTT，照 K = min(8, max(1, ⌈RTT ÷ 15 ms⌉)) 定 K，本機一條；測試仍可用 `FERRY_TEST_STREAMS` 指定。等 `Pong` 的那段跟著連結的生死，沒有自己的時限 |

測試：公式（無 RTT、0、2、15、16、30、31、120、250 ms 分別是 1、1、1、1、2、2、3、8、8 條）；host 的 monitor 收到 `Ping` 立刻以同一個 ID 回 `Pong`。

TUN 模擬（1 Gbps，不掉包，上傳一個隨機內容的檔案：RTT 2 與 30 ms 1 GiB、120 ms 512 MiB、250 ms 256 MiB；debug assertion 打開的 release build；各 1 次）。速率 MB/s，心跳是和資料同向的單程延遲，中位數／最慢，ms：

| RTT | buffer | K=1 | K=2 | K=4 | K=8 | 自動（選了幾條） | 自動時的心跳 |
|---|---|---|---|---|---|---|---|
| 2 | 預設 | 113.2 | 114.4 | 113.4 | 112.5 | 114.2（1） | 16.4／71.1 |
| 30 | 預設 | 59.8 | 79.8 | 79.5 | 80.2 | 79.4（3） | 17.6／24.8 |
| 120 | 預設 | 14.9 | 19.3 | 19.2 | 19.9 | 19.6（8） | 76.9／107.6 |
| 250 | 預設 | 6.4 | 8.1 | 8.1 | 8.3 | 8.2（8） | 151.5／746.7 |
| 2 | 64 MiB | 113.1 | 114.0 | 112.6 | 111.5 | 113.1（1） | 15.9／68.1 |
| 30 | 64 MiB | 59.9 | 102.3 | 103.2 | 102.9 | 101.9（3） | 35.7／36.3 |
| 120 | 64 MiB | 14.9 | 26.3 | 42.9 | 58.3 | 47.9（8） | 118.5／144.5 |
| 250 | 64 MiB | 6.5 | 10.6 | 15.6 | 17.2 | 18.8（8） | 126.2／752.5 |

和第 12 節引的 redesign 5.2 節一致：預設的 TCP buffer 下，多開 channel 最多快約三成（TCP 本身的上限），buffer 調大時接近隨條數增加；短路徑上（2 ms）不變也不變慢。自動選的條數和公式一致（30 ms 時量到 30.7 ms，所以是 3 條）。心跳在所有情況下最慢 0.75 秒；buffer 調大、條數多時，短路徑上中位數也會到 135 ms（2 ms、K=8，強制的），但自動在那裡只開 1 條。120 ms、64 MiB 時自動（8 條）47.9 對強制 8 條 58.3，只量了一次，差在 TCP 起步的變化，沒有再追。

與計畫的差異：

- **RTT 用連結上的 `Ping` 量，不在 server 的握手裡量**：握手的來回還包含 server 開 listener、看根目錄這些事，在 NFS 上會多出幾 ms，而公式在 15 ms 的倍數附近對幾 ms 很敏感；連結上的 `Ping` 只量網路與兩個 monitor 的一次喚醒。
- **心跳延遲只量了不掉包的情況**；掉包的長路徑上延遲會到好幾秒（第 7 步），條數多時排在前面的量更多，靠「從對方來的任何東西都算」。

### 第 11 步：多條 channel 的文件與驗收（2026-10-26）

| commit | 內容 |
|---|---|
| `82413a7` | architecture 的 Transport 原則第 4 點改成「stream 的條數照往返時間需要」，原本「未實作」的那段改成已決定，Connections 的「一條 stream」改成「它的 stream」；兩張圖與 pipeline 跟著改；redesign 的改進選項「同一條 SSH 連線上多開 channel」標成已做；CHANGELOG 加上模擬的速率；中文版同步 |

驗收（2026-10-26，最後的 build）：

- `ferry test`：本機 36 個過、2 個照例略過；fatcat 三種組合各 39 個全過（負載約 11）；nano4 三種組合各 39 個全過（經使用者自己的 ControlMaster，登入節點負載約 20）。兩台 host 上都沒有留下 monitor 或 server。兩台都在 15 ms 以內，K 是 1。
- 和這整項工作之前的 build（`5cef29e`）輪流跑 1 KB 的複製，各 8 次：fatcat 之前 0.18–0.41 秒（中位數約 0.19），現在 0.18–0.26 秒（中位數約 0.19）；nano4 之前 1.25–2.10 秒（中位數 1.70），現在 1.38–2.09 秒（中位數 1.60），負載重、晃得大，看不出變慢。新的多了三次量 RTT 的 ping，在 nano4 上約 20 ms。
- TUN 模擬：見第 10 步。

計畫的十一步到此做完。還沒做的：

- **實驗分支 `experiment/silent-drops-checks`**：第 4、6 步都做完了，照計畫可以刪；它也在 origin 上，刪掉要 push，留給使用者。
- **macOS 的外部觀點只做了型別檢查**，真正在 macOS 上跑要等下一次 push 的 CI。
- **附錄的已知缺口裡，「transport 之外拿不到連線的 fd」與改名成 `Wire`** 沒有做，和健康無關，第 5 步時已列出。
- **限制 SSH 連線裡在途的資料量**列為 redesign 的改進選項（使用者決定）。
