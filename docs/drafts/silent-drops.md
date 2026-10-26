# 連線與生命週期：每個地方一個 monitor，失去連線時兩頭都在上限內收拾，以及同一條 SSH 連線上的多條 channel

- 狀態：討論中：2026-10-26 使用者同意方向，並要求範圍擴大到所有的等待：每個等待都屬於一個生命週期，見[提案](#提案)；同一天決定把 redesign 的改進選項「同一條 SSH 連線上多開 channel」（原本的 A3）併進來一起做，見[第 9 節](#9-同一條-ssh-連線上的多條-channel)與[計畫](#計畫)；同一天決定在 peer 之上加 monitor，管穩定性與生命週期，當作整份提案的骨架；再改成每個地方一個：client 的機器上一個，每個 host 上一個（就是 launcher），server 是 worker，見[第 2 節](#2-monitor每個地方一個管那裡所有的資源)
- 開始：2026-10-26；最後修改：2026-10-26
- 會改動：`docs/design/architecture.md` 的「Transport」原則第 10 點（健康在哪裡判斷）、「Connections」裡每種連線怎麼結束、launcher 的「它啟動的東西不依賴它」，以及 transport 對外的介面；兩者的 `.zh-TW.md`
- 相關：
  - [one-session.md](one-session.md) 的第 6、7 步：追蹤 launcher 的連線與 server 的 control connection 改走 stream 層；
  - `2204943`、`5fbdefa`：launcher 對開頭的位元組與第一個封包的時限，以及被追蹤之前出錯就結束；
  - 2026-10-23 的原則：連線的健康只在一處判斷（`docs/drafts/architecture-redesign.md` 原則 7）；
  - [architecture-redesign.md](architecture-redesign.md) 的改進選項「同一條 SSH 連線上多開 channel」，與 5.2 節高 RTT 的模擬。

## 摘要

- **Monitor**：peer 的 owner、decider、executor 管的是資料怎麼流、I/O 放在哪裡；穩定性與生命週期交給 monitor。**每個地方一個**：client 的機器上一個，在 client 行程裡；每個 host 上一個，就是 launcher 行程。它是那個地方的頂層，管那裡所有的資源：連線、listener、子行程，「啟動 server」只是它取得子資源的方式。核心是不做 I/O 的狀態機，兩個地方共用；server 是 worker，不是 monitor。兩個 monitor 經追蹤用的連線配對，那是唯一的健康連結，也是唯一的控制連結。
- **兩條原則（2026-10-26 與使用者討論定下）**：**狀態可知**與**操作可控**。狀態可知：強保證只放在「失去連線」上：失去連線之後，每一頭都在上限內自己收拾，不留下看不到也收拾不了的狀態。只要連線還在，任何狀態都看得到：回報出來，讓使用者，或 Ferry 原本的錯誤處理，決定怎麼做。每個可能卡住的東西，都由一個不是它自己的觀察者看著。操作可控：使用者在 client 上的每個操作（Ctrl-C、終止、關掉終端機）都傳到每一頭；連線還在時明確傳過去並確認結果，連線不在時由失去連線的規則接手。
- **目標**：所以等網路的每一個等待，在失去連線時都會返回，由同一套機制給上限；等待包括讀、寫、等連線進來、等對方表明身分、等子行程結束。其餘的等待不逾時，而是看得到。
- **等待分五種**：等網路上的對方（由 Scope 與 Liveness 管）、等網路上的第三方（工具帶進展的下限）、等一個人（不設上限，只有 Ctrl-C）、等系統（連線還在、看得到：回報，不逾時）、host 上 launcher 出現之前（第一個 session 上的雙向心跳，host 那頭是 shell 的看門狗）。
- **生命週期（Scope）**：每個等待都屬於一個 Scope。Scope 排成一棵樹，父的結束了，子的跟著結束；每個 Scope 只有一個「結束了」的來源。
- **一個 host 一個 Liveness**：它的來源隨階段換手，client 這頭是登入 → 第一個 session 的 `alive` 行 → 心跳；launcher 那頭是「30 秒內要被追蹤」→ 心跳。用它的程式碼不必知道現在是哪個階段。
- **只剩三種計時**：心跳、心跳出現之前的暫時上限、收尾的寬限，定義在同一處。transport 對外只給 `Wire`、`Listener`、`Child` 三種型別，連線的 fd 不外流，所以「每個等待都帶著 Scope」由型別保證，不靠記得去補。
- **多條 channel**：一次傳輸可以有 K 條 stream，都走同一條 SSH 連線，所以和其他連線一起斷，健康不必多做任何事；要做的是 block 的順序與 K 怎麼定。高 RTT、主機的 TCP buffer 又夠大時，才比一條快。

## 問題

**無聲斷線**指連線斷了，兩端卻都沒收到關閉（沒有 FIN 或 RST），例如網路中斷、筆電休眠、NAT 的對應被清掉。這時誰在等對方，誰就一直等：

- TCP 的 keepalive 預設 7200 秒才開始探測，再 9 次各 75 秒，約 2 小時 11 分才發現；
- sshd 的 `ClientAliveInterval` 預設 0，不主動探測。fatcat 是預設值；nano4 的 `sshd_config` 一般使用者讀不到，kernel 的 `tcp_keepalive_time` 一樣是 7200；
- client 這邊，Ferry 自己開的 master 有 `ServerAliveInterval`，使用者自己的 ControlMaster 不一定有。

Ferry 目前的保證是一處一處補上的：stream 有 5 秒的 keepalive 與 20 秒的沉默；client 以 20 秒判斷每條 control connection；第 6 步讓追蹤 launcher 的連線也有。做第 6 步時才發現還有沒有時限的地方，所以要有系統地盤點一次。

**目標**：每條連線的每個階段，任何一方在等另一方時，失去連線都不會讓它一直等下去；兩頭都在上限內收拾。2026-10-26 使用者把範圍擴大：不只連線上的讀取，等連線進來、等對方表明身分、寫、等子行程結束都算，目標是**統一整個連線健康的管理**。同一天與使用者討論定下原則，見[第 1 節](#1-原則與等待的種類)。

讀程式碼時看到的缺口列在[附錄](#附錄已知的缺口)。

## 提案

2026-10-26 提出，使用者同意方向；第一版只以 host 的心跳管已建立連線上的讀取，使用者指出範圍不足，擴大成下面的樣子。

### 1. 原則與等待的種類

**原則（2026-10-26 與使用者討論定下）。** 網路不可控：連線斷了，另一頭的狀態就不可知，我們也碰不到它。只要連線還在，任何狀態，出錯也好、卡住也好，都看得到、也處理得了。所以：

> 強保證只放在「失去連線」上：失去連線之後，每一頭都在上限內自己收拾，不留下看不到也收拾不了的狀態。只要連線還在，任何狀態都看得到：回報出來，讓使用者，或 Ferry 原本的錯誤處理，決定怎麼做。每個可能卡住的東西，都由一個不是它自己的觀察者看著。

就算沒有人在看（例如排程執行），連線還在時的狀態也是確定的：啟動它的人隨時能 Ctrl-C 或 kill，不會有東西在我們不知道的地方繼續跑。

| 情況 | 看得到嗎 | 處理 |
|---|---|---|
| 網路靜靜斷了 | 看不到 | 兩頭各自在 30 秒內收拾 |
| 心跳路上的行程被暫停：client（Ctrl-Z）、launcher、追蹤用的 `ssh -W`、ssh 的 master | 從另一頭看，和網路斷了分不出來 | 照網路斷了處理 |
| 心跳路以外的行程被暫停、檔案系統卡住、PAM、死結 | 看得到：連線還在 | 回報，讓使用者判斷 |
| host 下載時，它和 GitHub 之間的連線 | 那條連線的狀態看不到 | 要有上限：工具的速度下限 |
| launcher 出現之前 | 那時的連線就是第一個 session | 雙向心跳：失去它，兩頭各自收拾（第 5 種） |

由這條原則推出兩條規則：

1. **心跳路上 vs. 心跳路外。** 心跳走的路是：client 送心跳的 thread → client 追蹤用的 `ssh -W` → ssh 的 master → TCP → sshd → launcher。這條路上任何東西停了，都當成失去連線，30 秒後兩頭各自收拾；這條路以外的停了，心跳持續確認連線還在，所以是看得到的狀態，回報。
2. **每個可能卡住的東西都有不是它自己的觀察者。** host 上的 server、sudo 的檢查、fixture 由 launcher 看著；client 的子行程（`ssh`、本機的 server）由 client 看著。所以 server 就算整個卡住、連自己的狀況都送不出來，launcher 還是能說出來。觀察的方式統一在 transport 的 `Child`（第 5 節）：它以 `waitpid` 的 `WUNTRACED`、`WCONTINUED` 說出被暫停與繼續，launcher 的子行程經追蹤用的連線回報給 client，client 的子行程直接回報。做不到的要老實說：行程卡在不可中斷的等待（D state，例如 NFS）時，`Child` 只看得到它還在、沒被暫停；要說出「卡在檔案系統上」，得讀 `/proc/<pid>/stat`，只有 Linux 能做，見待決定的問題。

![心跳的路：client 送心跳的 thread、追蹤用的 ssh -W、ssh 的 master、TCP、sshd、launcher。這條路上任何東西停了或沒聲音，都當成失去連線，兩頭各自在 30 秒內收拾。路以外的，由不是它自己的觀察者看著：client 看著 stream 的 ssh -W 與本機的 server，launcher 看著 server、sudo 的檢查與 fixture；它們被暫停時看得到，由觀察者回報，讓使用者判斷](../assets/silent-drops-path.svg)

**第二條原則：操作可控（2026-10-26 與使用者討論定下）。** 狀態可知說的是看得到；操作可控說的是做得到：

> 使用者在 client 上的每個操作，都傳到每一頭。連線還在時，明確傳過去，並確認結果；連線不在時，由「失去連線就在上限內收拾」接手。每一步都有上限。

所以它不需要另一套保證，只要求「明確傳過去」這條路存在，而且有上限：

| 使用者的操作 | 連線還在時 | 連線不在時 |
|---|---|---|
| 第一次 Ctrl-C、SIGTERM、關掉終端機（SIGHUP） | client 在追蹤用的連線上對每個 launcher 送「停」；launcher 對它啟動的程式送 SIGTERM，收集各自怎麼結束，回報 client；client 最多等收尾的寬限，然後說出每一頭的結果 | launcher 自己發現失去連線，照樣收拾 |
| 第二次 Ctrl-C | client 立刻結束；host 讀到連線結束（不是沉默），立刻收拾，不必等 30 秒 | 同左 |
| client 被 `kill -9` | 攔不到；client 的 `ssh` 讀到輸入結束而關掉 channel，或心跳停止，host 在 30 秒內收拾 | 同左 |

**現在的做法，以及為什麼要改。** 現在 Ctrl-C 也讓 host 停下，但是間接的：`ssh` 子行程和 client 在同一個 process group（`src/transport/ssh.rs` 沒有另外分組，`interrupt::unblock_in_child` 讓它們收得到訊號），終端機的 SIGINT 同時打到它們，它們立刻結束、channel 關掉，host 上的 server 讀到 control 結束而自己停下。問題是：client 來不及有秩序地停，channel 已經沒了，送不出「停」、也收不到回覆；而且**結果不可知**，client 不知道 host 上收拾乾淨了沒，例如暫存檔刪了沒，違反狀態可知。

**做法。**

1. 經 master 的 `ssh`（第一個 session、每條 `-W` channel）放進它們自己的 process group，終端機的訊號只到 client，由 client 決定怎麼停。
2. 第一次 Ctrl-C 照上表明確傳達，結果回報給使用者；第二次 Ctrl-C 立刻結束。
3. launcher 在收尾的寬限內沒有回覆，client 就不再等，交給失去連線的規則。

**要注意的。**

- **登入用的 `ssh` 不能分組**：它要從終端機讀密碼或 OTP，在背景的 process group 裡讀終端機，會被系統暫停（SIGTTIN）。所以只有登入之後、經 master 的 `ssh` 才分組。
- **Ctrl-Z 的行為不變**：分組之後，Ctrl-Z 只停住 client，`ssh` 不跟著停；client 送心跳的 thread 停了，30 秒後 host 照樣判定失去連線（取捨第 5 點）。
- **本機的 server 也分組**（2026-10-26 使用者決定）：現在它和 client 在同一個 group，直接收到 SIGINT，自己停下。改成放進它自己的 process group，由 client 的 monitor 照 host 的 monitor 停 worker 的方式停它：SIGTERM，等收尾的寬限，到期才砍；結果從它的 control 與結束狀態看得到。這樣本機與 host 上的 worker 停法一致。

![操作可控：第一次 Ctrl-C，client 在追蹤用的連線上送「停」，launcher 對它啟動的程式送 SIGTERM，收集結果回報，client 說出每一頭的結果；第二次 Ctrl-C，client 立刻結束，host 讀到連線結束，立刻收拾；client 被 kill -9，心跳停止或 channel 關掉，host 在 30 秒內收拾](../assets/silent-drops-stop.svg)

**等待的種類。** 2026-10-26 對照程式碼檢查設計時發現：把所有等待都當成「等網路上的對方」，只給得出一種答案（Liveness）。照上面的原則，等待有五種，答案各不相同。**Scope 與 Liveness 判斷的是連線活著，不是事情有進展。**

![等待的五種：等網路上的對方，由 Scope 與 Liveness 管；等網路上的第三方，工具帶進展的下限；等一個人，不設上限，只有 Ctrl-C；等系統，連線還在、看得到，回報不逾時；host 上 launcher 出現之前，只有 shell：第一個 session 上的雙向心跳，host 那頭由 shell 的看門狗實作](../assets/silent-drops-kinds.svg)

| 種類 | 例子 | 怎麼處理 |
|---|---|---|
| 1. 等網路上的對方 | 連線上的讀寫、accept、對方表明身分 | Scope 與 Liveness，下面第 3 到 9 節 |
| 2. 等網路上的第三方 | 下載 release、cargo 抓 crate | 網路，所以要有上限：Ferry 啟動的工具帶上**進展的下限**。host 下載時的 curl 已經有 `--connect-timeout 10 --speed-limit 1024 --speed-time 30`，wget 有 `-T 30`，cargo 有自己的 http 逾時；**client 自己抓 release 的 curl（`src/remote/release.rs`）沒有，要補上一樣的** |
| 3. 等一個人 | 密碼、OTP；沒有 `--yes` 時問要不要安裝 | **不設上限**，只有 Ctrl-C 結束它；其他已經有心跳的 host 照常 |
| 4. 等系統 | 檔案系統卡住（例如 NFS）、心跳路以外的行程被暫停、sudo 的 PAM 等輸入、peer 死結 | 連線還在，看得到：**回報，讓使用者判斷**，不逾時。行程被暫停與繼續由 `Child` 說出來（規則 2）；其餘靠現有的「60 秒沒有進展就說出每個 thread 在哪裡」 |
| 5. host 上 launcher 出現之前 | 第一個 session 的 shell 等 client 的下一個指令、等上傳 | 那時 host 上還沒有 Ferry，只有 shell：第一個 session 上的雙向心跳，由 shell 實作，見下面 |

#### 第 5 種：第一個 session 上的雙向心跳

2026-10-26 使用者決定：採用改良的做法 A。launcher 出現之前，第一個 session 自己帶著兩個方向的心跳，規則和之後的心跳一樣，5 秒送一次、30 秒沒收到就算斷，只是由 shell 實作：

- **host → client**：腳本在背景每 5 秒印一行加框的 `alive`；client 30 秒沒收到東西就算那個 host 不在了（第 4 節）。
- **client → host**：client 每 5 秒送一行什麼都不做的指令 `:`，包括它在等使用者回答要不要安裝的時候。
- **host 的看門狗**：probe 腳本跑自己的指令迴圈（`while IFS= read -r line; do …; done`），每讀到一行，就在一個記錄動靜的檔案後面附加一個位元組；上傳時，正在寫入的檔案變大也算動靜。背景的看門狗每 5 秒比一次兩者的大小（`wc -c`，POSIX 沒有 `stat`），30 秒都沒變，就結束整個 session，連同還在讀上傳的子行程（`kill -TERM -$$`：sshd 為沒有終端機的 session 開的 shell 是 session leader，要在 nano4 與 fatcat 上實測確認）。exec 成 launcher 之前，停掉背景的 `alive` 與看門狗，並刪掉記錄動靜的檔案。

所以 launcher 出現之前，host 的 Liveness 來源就是這個雙向心跳；launcher 出現之後，換成真正的心跳。承載的方式換了，規則沒變，第 5 種也就不再是特例。

- 好處：沒有 `--yes` 時，client 等使用者回答的那段照樣送 `:`，看門狗不會誤殺，不必多開 session，也不必在開 session 之前先問使用者；上傳不必分段，看門狗看的是檔案有沒有變大。
- 代價：shell 的 `sleep 5` 讓判斷落在 30 到 35 秒之間，不像 Rust 那邊準；只能用 POSIX sh 的工具；要確實砍掉卡在讀取上的子行程；腳本裡的時間要能以注入縮短，讓測試用假的 ssh 停止送 `:`、看 host 的 shell 在時限內結束。

**不採用：調整流程，讓 host 盡量不等 client**（原本的做法 B）。client 在第一個腳本裡把決定所需的都給 host，host 自己下載、裝好、exec 成 launcher，只有要上傳時才等。它的好處主要是速度：省下幾個來回，每個在 nano4 上約 40 ms，相對一個 session 的 1 到 2 秒很小；對健康的幫助只是等待少一點，上傳那段仍要靠上面的機制；而且會改變沒有 `--yes` 時何時問使用者。要讓啟動更快，可以另外列成改進選項，和健康分開考慮。

#### 其他對照時找到的

- **本機把 stream 交給 server（`hand`）**：現在等本機 server 回應最多 20 秒（`TAKING`）。本機沒有連線可失去，本機 server 由 client 的 `Child` 看著：它結束了，socket 就關，`hand` 隨即失敗；它被暫停了，`Child` 會說出來。所以照原則，它屬於第 4 種（等系統），拿掉 `TAKING`、不逾時（依原則提議，待使用者確認）。
- **`--remote-join` 時 host 上的加入行程**：它由 sshd 的 session 啟動，不是 launcher 啟動的；它把自己的輸入輸出交給 launcher 或 server，然後等對方放開它握著的 socket。所以它跟著它加入的那個 Scope 結束：launcher 或 server 一結束，socket 就關，它也就結束。下面的 Scope 樹要把它算進去。

### 2. Monitor：每個地方一個，管那裡所有的資源

2026-10-26 使用者提出，併進這份提案當作骨架：peer 的 owner、decider、executor 解決的是資料怎麼流、I/O 放在哪裡；誰活著、誰該結束、失去連線怎麼收拾、使用者的操作怎麼傳到每一頭、卡住時誰說出來，是另一組問題，交給 **monitor**。同一天討論後定下它的形狀：**每個地方一個 monitor，它是那裡的頂層，管那裡所有的資源**；第一版「每個行程一個」改掉了。下面第 3 到 5 節的 Scope、Liveness、三種型別，都是 monitor 用來做事的東西。

![Monitor：每個地方一個，client 的機器上一個，每個 host 上一個（就是 launcher）。核心不做 I/O，持有 Scope 樹，只做決定；觀察者（心跳、等子行程、收訊號、各條 Wire）做 I/O，把事件送進核心；核心的動作是取得與結束資源（啟動 worker、結束 Scope）、送「停」、回報、結束。worker 在它底下，只收「停」，只回報做完或失敗。兩個 monitor 經追蹤用的連線配對](../assets/silent-drops-monitor.svg)

**這些現在散在好幾處：**

| 現在在哪裡 | 管什麼 |
|---|---|
| `follow.rs` | client 追蹤兩個 server：`LINGER`、`GIVEN`、誰先結束、control 的沉默（one-session 第 7 步之後交給 stream 層） |
| `interrupt.rs` | 訊號、收拾暫存檔、結束子行程 |
| `peer.rs` 的 `serve` | server 自己的生命週期：`DONE`、等 client 結束 control |
| `launcher.rs` | launcher 的生命週期、它啟動的程式 |
| `transport` 各處 | 等子行程結束的 `ENDING`、`close_within`、`end_within` |

**形狀：**

1. **每個地方一個 monitor**：
   - **client 的 monitor**：在 client 行程裡，管傳輸、每個 host、client 的連線與子行程（`ssh`、本機的 server）。client 行程除了 monitor，還有準備、建立 stream、畫面這些工作：monitor 是它的頂層，不是整個 client。
   - **host 的 monitor**：**就是 launcher 行程**。它管那個 host 上的一切：它的 listener、它啟動的程式。「啟動 server」只是它取得一個子資源的方式，所以它本來就兼任 launcher。第一個 session 照樣 exec 成它，one-session 不受影響。
2. **兩個 monitor 共用同一個核心**，照 owner 的架構（2026-10-23 的原則）：一個不做 I/O 的狀態機，持有 Scope 樹，只做決定。輸入是事件：心跳斷了、子行程被暫停、繼續或結束、使用者的訊號、worker 做完或失敗、對方的 monitor 送來「停」。輸出是動作：取得資源（啟動 worker、開連線）、結束某個 Scope、送「停」、回報、結束。時間由外面給，以合成的事件與時間就能做確定性的測試，不必真的等 30 秒。
3. **觀察者做 I/O**，把事件送進核心：送心跳與聽心跳的 thread、等子行程的 thread（`Child`）、收訊號的 thread、各條 `Wire`。
4. **兩個 monitor 隔著網路配對**，經追蹤用的那條連線：那是**唯一的健康連結**（心跳），也是**唯一的控制連結**（「停」與結果的回報）。host 的 monitor 是 client 的 monitor「啟動」的（經第一個 session），所以在樹裡，它的父節點是 client 的 monitor：連線還在時，client 的 monitor 看得到它、也管得到它；失去連線後，它自己在上限內收拾。
5. **server 是 worker，不是 monitor**：它沒有心跳，也沒有自己的 Scope 樹，只在三種情況結束，它的每個等待也都因這三件事返回：
   1. 被告知停下：control 結束，或收到 SIGTERM；
   2. 它的 monitor 不在了：**生命線**被關掉（下面）；
   3. 自己做完了。

   peer 在 worker 裡，只和這段很薄的「worker 怎麼結束」說兩件事：收到「停」（附原因）；回報「做完了」或「失敗了」。peer 不知道連線、訊號、計時器的存在。這是 2026-10-23「peer 只收 Lost 與 Closed 事件，不持有網路計時器」的完整版。

**client 也要一個 monitor。** 它不是另一個行程，而是 client 裡管生命週期的那一層，而且是兩個地方裡最忙的：

| 它的工作 | 現在在哪裡 |
|---|---|
| 每個 host 的 Liveness：心跳的一頭；host 不在了就讓傳輸失敗、收拾 | `transport/launcher.rs`、`follow.rs` |
| 看著自己的子行程：`ssh`、本機的 server（被暫停、結束、收尾的寬限） | `transport` 各處的 `ENDING`、`end_within` |
| 使用者的操作：第一次、第二次 Ctrl-C，SIGTERM，SIGHUP；把「停」送到每個 host、收它們的結果 | `interrupt.rs` |
| 決定傳輸什麼時候結束：哪個 server 做完了、失敗了、不在了 | `follow.rs`（`LINGER`、`GIVEN`） |
| 把狀態說給使用者：誰被暫停了、為什麼失敗 | 各處 |

和 host 的 monitor 共用核心，好處是心跳的兩頭行為一致（多久沒收到算斷、斷了做什麼，同一段程式），Scope 樹、收尾的寬限、「停」怎麼傳也是同一套，測試只寫一套。它和 host 的不同在於：

- **沒有生命線**：它上面沒有別的 monitor；看著它的是使用者本人，看得到畫面，按得了 Ctrl-C。
- **多了使用者這個輸入**：訊號、兩段式的 Ctrl-C，以及等一個人（密碼、OTP、要不要安裝）時不設上限。
- **管好幾個 host**；host 的 monitor 只管自己那個地方。

**`LINGER` 與原則衝突。** `follow.rs` 現在是：一個 server 做完之後，另一個最多再等 `LINGER`（30 秒），沒做完就結束它。照第 1 節的原則，這段時間連線還在，另一個沒做完是看得到的狀態（進展的問題），應該回報、不逾時，不是自動判定失敗。所以拿掉 `LINGER`（2026-10-26 使用者確認），改成：另一個遲遲沒做完時，用現有的診斷說出它卡在哪裡，交給使用者判斷。`GIVEN`（某一頭失敗後，給另一頭 5 秒結束）沒有衝突：那是 Scope 因失敗而結束之後的收尾寬限，原則允許，併進第 7 節的收尾寬限。

**生命線。** host 的 monitor 自己被 `kill -9` 或當掉時，host 上沒有人看著它；若網路剛好也斷了，client 碰不到 host，worker 的 channel 又不會關，worker 就會留到 sshd 發現為止。所以 monitor 啟動 worker 時多給它一個管道，自己握著寫的那頭；worker 看著讀的那頭。monitor 一結束，不論怎麼結束，系統都會關掉那頭，worker 立刻讀到結束，有秩序地停下。它是純本機的事件，沒有計時器，也沒有網路。Linux 的 `PR_SET_PDEATHSIG` 做得到類似的事，macOS 沒有，所以用管道。經 `sudo` 的 worker：`sudo` 預設會關掉 3 以上的檔案描述符（`closefrom`），要確認保不保得住這個管道，或改用別的傳法，要實測。本機的 server 是 client monitor 底下的 worker，它的 control 管道本身就兼任生命線：client 一不在，管道就關。

**界線：**

- **只管生命週期，不管效能。** `control.rs` 的「是什麼拖住了傳輸」、`--profile` 照舊分開；monitor 只在卡住時把診斷送出去（狀態可知）。
- **只做決定，不搬資料、不碰檔案。** peer 內部怎麼停它的 stage，仍是 peer 自己的事：`stop.rs` 的第一個錯誤、`stage.rs` 的 stage panic 時停下，都留在 peer 裡。
- **不寫兩份。** client 與 host 的 monitor 共用核心；不共用的只有觀察者與 Scope 樹。worker 的那段結束邏輯，本機與 host 上的 server 也共用。

**名字。** launcher 這個名字當初是因為「它只做啟動」；現在它是 host 的 monitor，除了啟動，還有心跳、看著它啟動的程式、失去連線時收拾。2026-10-26 使用者決定改名為 **monitor**：子命令 `ferry launcher` 改成 `ferry monitor`。這份草稿的其餘部分仍寫 launcher，指的就是 host 的 monitor；程式碼與文件在實作時一起改（計畫第 4 步）。

### 3. 每個等待都屬於一個生命週期（Scope）

![Scope 的樹：client 端是傳輸、每個 host、那個 host 上的每條連線與子行程；host 端是 launcher 與它啟動的程式；兩邊以心跳相連](../assets/silent-drops-scopes.svg)

- **client 端**（client 的 monitor）：一次傳輸是根；底下是每個 host；每個 host 底下是它的連線（第一個 session、追蹤 launcher 的那條、每條 control、stream 的一半）與承載它們的 `ssh` 子行程。本機的 server 是 worker，直接掛在傳輸底下：它是 client 的子行程，它的 control 管道就是生命線，不需要 Liveness。
- **host 端**（host 的 monitor，就是 launcher）：它是根，它的父節點是 client 那棵樹上那個 host 的 Scope，中間隔著網路，以心跳與「停」相連；底下是它的 listener（等連線進來）與它啟動的 worker（server、sudo 的檢查、fixture），每個 worker 都拿著一條生命線。`--remote-join` 時 host 上的加入行程不是 launcher 啟動的，但它等的是它加入的那個（launcher 或 server）放開 socket，所以算在那個 Scope 底下（第 1 節）。
- **規則只有三條**：
  1. 每個 Scope 只有一個「結束了」的來源：傳輸是 Ctrl-C、失敗、或任何一個 host 的 Scope 結束；host 是它的 Liveness；launcher 是它暫時的上限或心跳；一條連線是它的父 Scope，或它自己的結尾。
  2. 父的結束了，子的跟著結束；子行程在結束前有一段收尾的寬限。傳輸的 Scope 因使用者的操作結束時（第一次 Ctrl-C、終止、關掉終端機），先在每條追蹤用的連線上送「停」，等 launcher 回報各自的結果，最多等收尾的寬限（第 1 節，操作可控）。
  3. 每一個會阻塞的操作都帶著一個 Scope，只因兩種原因返回：等的事發生了，或 Scope 結束了（附原因）。

### 4. 一個 host 的 Liveness，來源隨階段換手

![一個 host 的 Liveness 在各階段的來源：登入時是 ssh 自己的上限；第一個 session 是 alive 行；launcher 說出位置後是 30 秒內要被追蹤；之後是心跳。launcher 出現之前，host 那頭由 shell 的看門狗看 client 每 5 秒送的指令。它說不在了的時候，client 端那個 host 上的每個等待都返回，launcher 結束它啟動的程式](../assets/silent-drops-liveness.svg)

| 階段 | client 看那個 host 的 Liveness | launcher 自己的 Liveness |
|---|---|---|
| 登入 | ssh 的 `ConnectTimeout`；等 ControlMaster 的 30 秒 | 還沒有 launcher |
| 第一個 session（probe、安裝） | 腳本在背景每 5 秒印一行加框的 `alive`，30 秒沒收到就算斷；在 host 上建 dev build 的那幾分鐘也有。exec 成 launcher 之前停掉它 | 還沒有 launcher；shell 的看門狗：client 每 5 秒送一行 `:`，30 秒沒有動靜就結束 session（第 1 節） |
| launcher 說出位置之後、被追蹤之前 | 等追蹤的那條連線的 start mark，上限 30 秒 | 30 秒內要被追蹤 |
| 被追蹤之後 | 心跳：5 秒送一次，30 秒沒收到就算斷 | 同一個心跳 |

Liveness 說不在了：client 那個 host 上的每個等待都返回並說出原因，傳輸失敗；launcher 先送 SIGTERM 給它啟動的程式，讓它們像收到 Ctrl-C 一樣收拾，寬限到期就砍掉，然後自己結束。

一個 host 只有一條 SSH 連線（2026-10-25 定的），所有 channel 走同一條 TCP，網路靜靜斷掉時一起斷，所以一個心跳就判斷得了那個 host 上的所有連線。

### 5. 等待一律經過 transport 的三種型別

![一個等待：Wire 的讀寫、Listener 的 accept、Child 的 wait，都同時等兩件事：等的事，或 Scope 結束；前者返回結果，後者返回結束的原因。整個程式只有三種計時，定義在同一處](../assets/silent-drops-wait.svg)

| 型別 | 做什麼 | 等什麼 |
|---|---|---|
| `Wire` | 一條連線：讀寫封包，也讀原始位元組（shell 的雜訊、記號、握手、秘密）；正常結尾的標記是一種封包，讀到就是結尾，用來分辨正常結束與被切斷 | 資料、空間、對方的結尾，或 Scope 結束 |
| `Listener` | 等連線進來，並等它表明身分（開頭的位元組、port 的秘密）；還沒表明身分的連線不屬於任何 host，掛在 listener 底下，用暫時的上限 | 連線、身分，或 Scope 結束 |
| `Child` | 承載連線的行程與啟動的程式；每個可能卡住的東西的觀察者（第 1 節規則 2） | 它結束，或 Scope 結束；後者給它收尾的寬限，到期就砍。等待時也說出它被暫停與繼續 |

transport 對外只提供這三種型別，連線的 fd 不外流，所以沒有人能繞過 Scope 直接讀寫。怎麼接上（`ssh -W`、loopback port 加秘密、`--remote-join`、本機的管道或 socket pair）只在建立 `Wire` 時不同，之後都一樣。

### 6. 所有的等待，與它的上限從哪來

| 等待 | 例子 | 上限從哪來 |
|---|---|---|
| 讀 | 第一個 session、start mark、握手、control、stream、sudo 的檢查、fixture | 所屬 host 的 Liveness |
| 寫 | 對方不讀，或連線已死但 TCP 還沒發現 | 同上 |
| 等連線進來 | launcher 等被追蹤；之後的每條連線；server 等 stream | launcher：暫時的上限，之後是心跳；server：它自己的 Scope，隨 launcher 或 client 結束它的 control 而結束 |
| 等對方表明身分 | 連線開頭的位元組、port 的秘密 | listener 底下的暫時上限 |
| 等子行程結束 | `ssh -W`、本機的 server、launcher 啟動的程式 | 父 Scope 結束後的收尾寬限 |
| 登入 | ssh 本身 | ssh 的 `ConnectTimeout`；等 ControlMaster 的 30 秒 |
| 等網路上的第三方 | 下載、cargo 抓 crate | 工具帶進展的下限（第 1 節第 2 種） |
| 等一個人 | 密碼、OTP、要不要安裝 | 不設上限，只有 Ctrl-C（第 1 節第 3 種） |
| 等系統 | NFS、心跳路以外的行程被暫停、PAM、死結、本機的 `hand` | 回報，不逾時（第 1 節第 4 種） |
| host 上 launcher 出現之前 | shell 等下一個指令、等上傳 | 第一個 session 上的雙向心跳，shell 的看門狗（第 1 節第 5 種） |

### 7. 整個程式只剩三種計時

1. **心跳**：5 秒送一次，30 秒沒收到就算斷（2026-10-26 使用者決定）。
2. **暫時的上限**：心跳出現之前，或連線表明身分之前，30 秒，和心跳的沉默是同一個值。
3. **收尾的寬限**：Scope 結束後給子行程的時間，取代現在的 `ENDING`（5 秒）、`GIVEN`（5 秒）、`close_within` 等。`LINGER`（30 秒）不併進來，而是拿掉：它是連線還在時的逾時（第 2 節）。它是原則的後半：失去連線之後「在上限內收拾」的那個上限。例外要老實說：卡在不可中斷等待（D state）的行程連 SIGKILL 都砍不掉，Ferry 只能放棄它、自己照樣結束；它沒有網路可卡，等 I/O 回來就會結束。

都定義在同一處；測試以 `FERRY_TEST_SILENCE` 之類的注入縮短。

### 8. 會拿掉的

- stream 層的 keeper thread、keepalive、沉默判斷；Deframer 裡拆 keepalive 的那段；`Ends.before` 與換手時的緩衝：資料裡不再混著 keepalive，原始位元組與封包是同一串，不必換手。
- `follow.rs` 對 control 的判斷、server 的 control stream、`Carried::let_go`。
- 各處各自的時限：launcher 與 `accept` 的 `Silent`、`Within`，`hand` 的 `TAKING`，以及上面第 5 點列的收尾常數。

### 9. 同一條 SSH 連線上的多條 channel

原本是 redesign 的改進選項（原本的 A3）；2026-10-26 使用者決定和上面一起做。

![多條 channel：K 條 stream 與追蹤 launcher 的那條，都是同一條 SSH 連線、同一條 TCP 的 channel。第 0 條帶 block 以外的一切，block 交給佇列最短的一條；Destination 依序號把 block 放回，緩衝以 data 額度為上限，再交給 decider；第 0 條上依賴 block 的訊息等它。它們一起斷，一個心跳就涵蓋；但心跳會排在它們的資料後面，要量過](../assets/silent-drops-channels.svg)

- **為什麼要**：一條 channel 每個 RTT 最多送 2 MiB（OpenSSH 的 channel window）。redesign 5.2 節的模擬中，RTT 120 ms 約 17 MB/s、250 ms 約 8 MB/s；主機用預設的 TCP buffer 時，多開 channel 最多快 ×1.35，buffer 調大時接近 ×K。fatcat、nano4 這種短路徑上沒有好處。掉包的路徑上也沒有幫助，那要多條 TCP 連線，2026-10-25 決定不做。
- **怎麼做**：client 為一次傳輸建 K 條 stream，每條都是那個（或那兩個）host 的 Scope 底下的一條 `Wire`；兩個 server 的 pump 把 block 交給佇列最短的一條，其他所有封包（manifest、needs、results、額度、控制訊息）固定走第 0 條。短路徑上 K=1，就是現在的樣子。
- **健康**：K 條都走同一個 master、同一條 TCP，一定落在同一台、一起斷，所以由同一個心跳管，不必多做任何事。任何一條讀到沒有結尾標記的盡頭，就算被切斷，傳輸失敗，不需要確認與重送。堵塞（window 滿了、等空間）只在 Scope 結束時返回，沒有另外的計時器，慢但沒斷的鏈路不會被誤判。
- **順序（要先處理）**：現在 Destination 假設 block 照送出的順序到：解壓縮後由一個 thread 依序交給 decider（`src/destination/mod.rs`），協定裡沒有 block 的序號。K 條時 block 會亂序到，也可能和第 0 條上依賴它的訊息錯開，例如分段大檔的雜湊（`file hash`）比它最後的段先到。預定的做法：block 帶序號，Destination 收到後依序號放回再交給 decider，緩衝以 data 額度為上限（在途的 block 本來就不超過額度），decider 不用改；第 0 條上依賴 block 的訊息逐一盤點，各自等到它依賴的 block 放回之後才處理。盤點的結果寫回這裡。
- **K 怎麼定**：握手時量 RTT，例如每 20 ms 一條、最多 8 條；先以一個測試用的注入固定 K，量過再定公式。不給使用者選項：`--ssh-connections` 已經拿掉，這次也不加。
- **和心跳的交集**：心跳走追蹤 launcher 的那條 channel，各 channel 的 window 各自算，但最後都排在同一條 TCP 的送出佇列裡。多條 channel 只在 TCP buffer 調大時有用，那正是佇列最長的時候：例如 64 MiB 的 buffer 跑在 8 MB/s 的鏈路上，排在後面的心跳約要等 8 秒，仍小於 30 秒，但餘裕變小。用 5.2 節的 TUN 模擬（RTT 2、30、120、250 ms，buffer 用預設與 64 MiB）量心跳的延遲，再定 30 秒夠不夠（取捨第 3 點）。

### 取捨

1. **launcher 會結束它啟動的程式。** one-session 原本的原則是「它啟動的東西不依賴它」。2026-10-26 使用者同意改。
2. **行程被暫停（SIGSTOP）不再由沉默發現，改成回報。** 網路沒斷，只是心跳路以外的行程被暫停時，心跳照常。照原則它是看得到的狀態：`Child` 說出被暫停與繼續，launcher 的子行程與 client 的子行程同一段程式，client 顯示出來，不設上限，由使用者判斷。心跳路上的被暫停則照失去連線處理（第 1 節規則 1）。死結兩種設計都抓不到（keepalive 是另一個 thread 送的），靠現有的「60 秒沒有進展就說出每個 thread 在哪裡」。
3. **心跳與資料走同一條 TCP。** 慢的鏈路上，大量的資料可能讓心跳排隊幾秒；多條 channel 時更明顯（[第 9 節](#9-同一條-ssh-連線上的多條-channel)）。30 秒的餘裕應該夠，在 TUN 模擬與 nano4 上量過再定。
4. **改動大。** 所有建立連線、讀寫、accept、等子行程的地方都要改成帶 Scope；換來的是「有沒有上限」由型別保證。2026-10-26 使用者允許大幅重構，目標是程式簡潔、邏輯清楚、容易維護。
5. **client 被 Ctrl-Z 停住超過 30 秒，傳輸失敗，刻意和 rsync 不同。** client 停住時，它送心跳的 thread 也停了；host 分不出「client 被暫停」與「網路斷了」，而網路是要強保證的部分，所以照樣在 30 秒後判定 client 不在，收掉它啟動的一切。rsync 預設沒有逾時（`--timeout` 預設 0），Ctrl-Z 多久都能 `fg` 回來。曾考慮讓 client 攔下 Ctrl-Z（SIGTSTP），先告訴 launcher「暫停判斷」再停下，但暫停中若網路靜靜斷了，host 那頭就控制不了；2026-10-26 使用者對此態度保留，不做，維持現狀（A2 之後就是如此）。

## 其他做法

- **每條連線都自帶 keepalive，帶不了的階段各加時限。** 沿 one-session 第 6、7 步的方向。好處是連線之間互不牽連；壞處是時限散在十幾處，每加一種連線或一個階段都得記得補一個，這次就是這樣漏掉的。不採用。
- **只以 host 的心跳管已建立連線上的讀取**（2026-10-26 的第一版）。等連線進來、等對方表明身分、寫、等子行程結束、心跳出現之前，仍各是特例。使用者指出範圍不足，改成上面的。

## 待決定的問題

- [x] **健康在哪裡判斷。** 2026-10-26 使用者同意：以 host 為單位，見[提案](#提案)。
- [x] **launcher 結束它啟動的程式。** 2026-10-26 使用者同意。
- [x] **範圍。** 2026-10-26 使用者要求：統一所有的等待，不只連線上的讀取。
- [x] **被暫停的行程**（取捨第 2 點）：2026-10-26 使用者決定：心跳路以外的回報、不設上限；由 `Child` 統一說出，launcher 與 client 的子行程都算。心跳路上的照失去連線處理。
- [x] **第二條原則：操作可控**：2026-10-26 與使用者討論定下：使用者在 client 上的每個操作都傳到每一頭，連線還在時明確傳過去並確認結果，不在時由失去連線的規則接手（第 1 節）。
- [x] **本機的 server 要不要分組**：2026-10-26 使用者決定：分組，由 client 的 monitor 以 SIGTERM、收尾的寬限停它，和 host 上的 worker 一致。
- [x] **原則**：2026-10-26 與使用者討論定下：強保證只放在失去連線上，連線還在就看得到、回報；每個可能卡住的東西都有不是它自己的觀察者（第 1 節）。
- [x] **Ctrl-Z**（取捨第 5 點）：維持現狀，client 停住超過 30 秒就失敗；不做暫停判斷。
- [x] **心跳的時間**：2026-10-26 使用者決定：5 秒送一次，30 秒沒收到就算斷；暫時的上限、第一個 session 上的雙向心跳也用 30 秒，整個設計只有一個沉默的值。心跳排在資料後面的延遲，照樣在 TUN 模擬與 nano4 上量，確認 30 秒夠（取捨第 3 點）。
- [x] **安裝的那幾分鐘**：依原則已回答：`alive` 行確認的是連線還在，這就夠；安裝本身的進展是看得到的狀態（cargo 的輸出顯示給使用者），cargo 抓 crate 屬於第 2 種，有它自己的逾時。
- [ ] **收尾的寬限**：一個值，還是依子行程不同，例如 server 收拾暫存檔要多久。
- [x] **host 上 launcher 出現之前怎麼辦**（第 1 節第 5 種）：2026-10-26 使用者決定：第一個 session 上的雙向心跳，host 那頭由 shell 的看門狗實作；調整流程讓 host 少等（原本的做法 B）不採用。
- [ ] **本機的 `hand`**：依原則提議：本機沒有連線可失去，屬於第 4 種，拿掉 `TAKING`、不逾時；待使用者確認（第 1 節）。
- [ ] **D state 要不要說出來**：讀 `/proc/<pid>/stat`，說出「卡在不可中斷的等待」，只有 Linux 做得到；不做的話，只看得到它還在、沒被暫停（第 1 節規則 2）。
- [x] **monitor**：2026-10-26 使用者提出、同意：在 peer 之上加 monitor，併進這份提案當作骨架；同一天改成每個地方一個（client 的機器、每個 host），它是那裡的頂層，管所有的資源，host 的就是 launcher；server 是 worker；兩個 monitor 經追蹤用的連線配對（第 2 節）。
- [x] **client 要不要 monitor**：要，它是 client 裡管生命週期的那一層，和 host 的共用核心；沒有生命線，多了使用者這個輸入（第 2 節）。
- [x] **`LINGER`**：2026-10-26 使用者確認拿掉：一個 server 做完後另一個遲遲沒做完時，回報它卡在哪裡、不逾時（第 2 節）。
- [x] **launcher 改名**：2026-10-26 使用者決定：改名為 monitor，子命令 `ferry monitor`；實作時連同程式碼與文件一起改（第 2 節）。
- [ ] **生命線經 `sudo`**：`sudo` 預設關掉 3 以上的檔案描述符，生命線的管道保不保得住，或改用別的傳法；要實測（第 2 節）。
- [x] **多條 channel 要不要一起做。** 2026-10-26 使用者決定：一起做，整合在同一份計畫。
- [ ] **block 的順序**：預定 block 帶序號、Destination 依序號放回；第 0 條上依賴 block 的訊息有哪些、各自怎麼等，要先盤點（[第 9 節](#9-同一條-ssh-連線上的多條-channel)）。
- [ ] **K 的公式**：每 20 ms 一條、最多 8 條只是起點，量過再定。

## 計畫

每一步結束時，都要能執行、測試過。先做 monitor 與連線的健康，讓 stream 層變薄、每個等待都有 Scope；多條 channel 再長在上面：那時 stream 只剩封包與結尾標記，K 條只是 K 條 `Wire`。2026-10-26 併入 monitor 之後，第 2 到 5 步改成：先寫 monitor 的核心並以合成事件測試，再接上觀察者，然後是 launcher，最後把 client 與 server 換上去。

**2026-10-26 依原則重新審視。** 計畫的骨架不變：Scope、Liveness、三種型別、三種計時，本來就是在保證「失去連線時每個等待都會返回、兩頭都在上限內收拾」。調整的是：

- 目標的寫法：不是「每個等待都有時間上限」，而是失去連線時都會返回；連線還在時的等待不逾時，而是看得到。
- 新增觀察者的要求：`Child` 統一說出被暫停與繼續，client 的子行程也算（第 3 步）。
- 依原則回答了兩個問題：安裝的那幾分鐘（`alive` 行就夠）；本機的 `hand`（不逾時，待使用者確認）。
- 新增一個問題：D state 要不要說出來。
- 收尾的寬限是原則的後半；砍不掉的 D state 行程是例外，寫明。
- 測試照原則分成「失去連線」與「連線還在、事情卡住」兩組（第 5 步）。
- 多條 channel 不受影響：一條 channel 被切斷而心跳還在，是看得到的錯誤，照舊讓傳輸失敗並說出原因；心跳排在資料後面可能被誤判成失去連線，仍要量（第 9 步）。

1. **量測的基準。** 用 redesign 5.2 節的 TUN 模擬，量現在一條 channel 在 RTT 2、30、120、250 ms、buffer 預設與 64 MiB 下的速度，以及一條「心跳」（另一條 channel 上每 5 秒一個小封包）在大量資料下的延遲，當作之後的對照。
2. **monitor 的核心。** client 與 host 的 monitor 共用的、不做 I/O 的狀態機：Scope 樹、三種計時、事件與動作；時間由外面給，以合成的事件與時間測試：失去連線、子行程被暫停又繼續、使用者的訊號、peer 做完或失敗、對方送來「停」，各自得到的動作。還沒接上。同一步把 client 抓 release 的 curl 補上和 host 一樣的逾時與速度下限（第 1 節第 2 種）。
3. **觀察者：`Wire`、`Listener`、`Child`，與收訊號的 thread。** 先包住現有的連線與子行程，行為不變；transport 之外不再拿得到連線的 fd。`Child` 同時說出被暫停與繼續（規則 2）。
4. **host 的 monitor（launcher）換上核心，並改名。** `ferry launcher` 改成 `ferry monitor`，程式碼裡的 launcher 跟著改名。啟動 worker 時給它生命線；worker 那段結束邏輯（control 結束、SIGTERM、生命線關掉、做完）寫成本機與 host 上的 server 共用的一段。心跳：30 秒內要被追蹤，然後是心跳；心跳斷了或收到「停」，對它啟動的程式送 SIGTERM，收集各自怎麼結束，回報 client，寬限到期就砍。第一個 session 上的雙向心跳：client 每 5 秒送 `:`，腳本的看門狗 30 秒沒有動靜就結束 session，在 nano4 與 fatcat 上確認它砍得掉卡在讀取上的子行程。測試生命線：host 的 monitor 被 `kill -9` 時，worker 立刻停下，經 `sudo` 的也一樣。
5. **client 的 monitor 換上核心，server 換成 worker。** `follow.rs`、`interrupt.rs` 的生命週期部分搬進 client 的 monitor，`serve` 的生命週期換成 worker 那段結束邏輯；每個等待改經 Scope；拿掉 stream 層的 keepalive 與沉默判斷、`follow.rs` 對 control 的判斷與 `LINGER`、各處各自的時限與收尾常數。操作可控：經 master 的 `ssh` 與本機的 server 放進自己的 process group（登入用的 `ssh` 不放）；本機的 server 由 client 的 monitor 以 SIGTERM、收尾的寬限停下；第一次 Ctrl-C 送「停」、等結果，最多等收尾的寬限，第二次立刻結束。等一個人（密碼、OTP、要不要安裝）不設上限，其他 host 的心跳照常。測試照原則分三組，每個都先寫出會卡住的：
   - **失去連線**：在每個階段（登入、第一個 session、等被追蹤、被追蹤之後的傳輸、收尾）讓連線「不傳也不關」，以及讓心跳路上的每個行程被暫停：兩頭都在上限內收拾，host 上不留下行程，client 說出原因；
   - **連線還在、事情卡住**：心跳路以外的行程被暫停又繼續、server 停住（`FERRY_TEST_STALL`）、一個 server 做完而另一個遲遲沒做完（`LINGER` 拿掉之後）、等一個人：不逾時，看得到（回報、或 60 秒的診斷），繼續之後照常完成；
   - **操作**：第一次 Ctrl-C，host 上的程式都停下、暫存檔收拾掉，client 說出每一頭的結果；第二次 Ctrl-C，host 不等 30 秒就收拾；client 被 `kill -9`，host 在 30 秒內收拾；launcher 不回覆「停」時，client 在寬限到期後結束。
6. **monitor 與健康的文件與驗收。** architecture 新增 monitor 一節，原則第 10 點、Connections、launcher；CHANGELOG；本機、fatcat、nano4 的 `ferry test`，與第 1 步的心跳延遲再量一次。
7. **block 的順序。** 盤點 Destination 對順序的假設；block 帶序號，Destination 依序號放回；第 0 條上依賴 block 的訊息各自等。這時 K 仍是 1，行為不變；以亂序送 block 的測試確認。
8. **K 條 stream。** client 建 K 條；pump 把 block 交給佇列最短的一條，其他固定走第 0 條；任何一條被切斷就整個失敗。先以測試用的注入固定 K，測試 K=1、2、8，與其中一條被切斷。
9. **K 怎麼定。** 握手時量 RTT；用 TUN 模擬量各 K 的速度與心跳的延遲，定 K 的公式與 30 秒夠不夠。
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
