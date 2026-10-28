# 傳輸協定：一次傳輸的每一條連線

- 狀態：草稿
- 開始：2026-10-28；最後修改：2026-10-28
- 寫什麼：總覽。一次傳輸用到的每一條連線：誰和誰之間、怎麼建、上面傳什麼、怎麼結束，到每個 byte。連線分兩類：**走 pipe 的**，準備好之後轉成 pipe；**原樣的**，從頭到尾都是原樣的 bytes。描述的是 [pipes](pipes.md) 提案之後的樣子，pipes.md 寫為什麼這樣設計；和現在不同的地方，列在[第 5 節](#5-和現在的差別)。
- 未定的：標「待決定」，答案在 [pipes 的待決定](pipes.md#待決定的問題)。[4.5](#45-封包草案) 的封包格式整節是草案。
- 會改動：採用後，併入 `docs/design/architecture.md` 的 Transport、Connections、Monitors 與 Packets over the stream，以及它的 `.zh-TW.md`。

## 1. 全部的連線

![一次傳輸的全部連線，以這台機器上的 client 與 Source server、主機上的 monitor 與 Destination server 為例；實線走 pipe，虛線是原樣的 bytes，編號是建立的順序。1，一條 SSH 連線，TCP，跨機器的都是它的 channel。2，first session，經過 login shell，原樣：探測、安裝，exec 成主機的 monitor；安裝時，client 用 curl 從 GitHub 取 SHA256SUMS 與要上傳的 binary，主機用 curl 或 wget 下載 release。3，link，client 的 monitor 與主機的 monitor 之間，一條 control pipe。4，sudo 檢查，只有 --remote-sudo，原樣。fixture 只有 ferry test，原樣。5，control connection，client 與每個 server 之間，reports 與 control；本機那條不壓縮；server 的 stderr 是 lifeline，接到它的 monitor，原樣，不跨網路。6，stream，兩個 server 之間，manifest、data、answers 與 control；本機那一端由 client 經 server 的 socket 交接 fd。](assets/protocol-connections.svg)

以一台主機為例：client 與 Source server 在這台機器，Destination server 在主機上。編號是一次傳輸裡建立的順序。

**走 pipe 的**

| # | 連線 | 兩端 | 經過 | pipe | 流程 |
|---|---|---|---|---|---|
| 3 | link | client 的 monitor 與主機的 monitor | channel，`ssh -W` 到 monitor 的 socket | control | [3.3](#33-link走-pipe) |
| 5 | control connection | client 與每個 server | 主機：channel 到 monitor，由它交出；本機：pipe | control、reports | [3.5](#35-control-connection走-pipe) |
| 6 | stream，1 到 8 條 | Source 與 Destination 的 server | client 做的 socket pair；主機那一端接上 `ssh -W` | control、manifest、data、answers | [3.7](#37-stream走-pipe) |

**原樣的**

| # | 連線 | 兩端 | 經過 | 內容 | 流程 |
|---|---|---|---|---|---|
| 1 | SSH 連線 | client 與主機的 sshd | TCP；你的 ControlMaster，或 Ferry 的 `ssh -N` | SSH 自己的協定；跨機器的連線都是它的 channel | [3.1](#31-ssh-連線不是-ferry-的協定) |
| 2 | first session | client 與主機上的 `sh`，之後 exec 成 monitor | SSH session，經過 login shell | 帶 tag 的標記行 | [3.2](#32-first-session原樣) |
| 4 | sudo 檢查，只有 `--remote-sudo` | client 與 `sudo ferry build-id` | channel 到 monitor，由它交出 | 密碼行，一行 build | [3.4](#34-sudo-檢查原樣) |
| 5 | lifeline | monitor 起的每個 worker 的 stderr，與那個 monitor | socket pair，不跨網路 | stderr 的文字，夾著標記行 | [3.6](#36-lifeline原樣) |
| 6 | 交接 fd | client 到本機 server 的 socket；`--remote-join` 時，join 程序到 server 或 monitor 的 socket | Unix socket，SCM_RIGHTS | 1 byte 加兩個 fd | [3.8](#38-交接-fd原樣) |
| — | fixture，只有 `ferry test` | client 與 `ferry fixture` | channel 到 monitor，由它交出 | 原樣的輸出 | [3.9](#39-fixture原樣) |
| — | GitHub releases | client，或主機的 first session，與 GitHub | HTTPS，curl 或 wget | 安裝用的 release | [3.10](#310-github-releases-與其他) |

依序：

1. client 打開主機的 SSH 連線：只登入，不執行任何東西；要密碼或 OTP 時，在終端機問。
2. first session：探測主機；需要時安裝，由主機自己下載，或由 client 上傳；最後 exec 成 monitor，說出 build、socket、port 與 secret。
3. link：client 的 monitor 跟上主機的 monitor，heartbeat 開始。從此主機上的每個程式都由 monitor 起，各一條 channel，stderr 各是一條 lifeline。
4. `--remote-sudo` 時，sudo 檢查：要不要密碼，給的對不對。
5. 每個 server 一條 control connection：handshake 之後兩邊都等，client 比對兩個根目錄，再叫它們繼續。
6. stream，K 條；本機的那一端，經交接 fd 給 server。
7. 傳輸做完：control connection 與 stream 結束；link 在主機上的東西都結束之後才關，first session 最後。

兩邊都在本機時，沒有 1 到 4：client 以 pipe 起兩個 server，lifeline 接到 client，stream 的兩端都經交接 fd 給出。

## 2. 兩類連線

- **走 pipe 的**：link、control connection、stream。兩端都是 ferry 的程式。先**準備**，原樣的 bytes：到達、說明用途、交接、交換設定；連線交到兩端最終的程式手上之後，**ready**，轉成這種連線的一組 pipe，從此程式只經 pipe 讀寫，transport 負責封包、窗口與壓縮（[第 4 節](#4-走-pipe-的連線怎麼送)）。
- **原樣的**：first session、lifeline、sudo 檢查、fixture、交接 fd，從頭到尾都是原樣的 bytes：對面不是 ferry，或誰都可能寫進去，或只傳 fd。照現在的做法，不因 pipe 而改。SSH 連線與 GitHub 也在這一類，但它們不是 ferry 的協定。

兩類共用的元素，都是原樣的 bytes：

| 元素 | bytes | 誰寫，誰讀 |
|---|---|---|
| `STRAIGHT` | 1 byte，`01` | client 寫；socket 的 listener 讀：這條是直連來的，回 start mark |
| port 的 secret | 64 個十六進位字元 | client 寫；port 的 listener 讀，不對就斷線 |
| start mark | `\0ferry: start\0`，14 bytes | 對面寫：「從下一個 byte 起，寫的是最終的程式」；讀的一方跳到它，丟掉之前的 |
| end mark | `\0ferry: end\0`，12 bytes | server 寫在 handshake 的答案之後；client 確認答案沒有被插話 |
| 標記行 | `\0ferry:<tag>:<kind>:<長度>:<內容>\n` | first session 上主機寫的每一行，tag 是 client 每次執行新產生的；lifeline 上的 tag 固定是 `lifeline` |
| 訊息 | 5 bytes 的 header，類型 1 byte、長度 4 bytes little-endian，加 postcard | `Follow`、`Launch`、`Hello`、`Welcome` |
| 密碼行 | 密碼加換行 | client 寫；`sudo -S` 讀，只有 `--remote-sudo` 要密碼時 |
| stream 的 header | `[編號][壓縮]`，2 bytes | client 寫；server 讀。現在只有編號 |
| 交接 | 1 byte `00`，加兩個 fd（SCM_RIGHTS） | client 或 join 程序寫；收下的一方回 1 byte |

## 3. 每條連線的流程

照一次傳輸裡建立的順序。時序圖裡，箭頭的終點是讀走那一段的程式；綠色的箭頭是 start mark。底下一列綠色的是 ready，之後走 pipe；灰色的表示從頭到尾原樣。

### 3.1 SSH 連線（不是 ferry 的協定）

- 每台主機一條 TCP 連線，經 `ssh` 的 ControlMaster：你自己設了就共用，否則 Ferry 自己起一個，由一個 `ssh -N` 握著。登入時要密碼或 OTP，在終端機問。
- `ssh -O check` 經 master 在本機的 control socket，問它還在不在。
- 跨機器的連線都是這條連線裡的 channel；它斷了，傳輸就失敗，不重連。

### 3.2 first session（原樣）

![first session（原樣）：client 與主機上的 sh，之後 exec 成 monitor。ssh：sh -c <探測的 script>，經過 login shell；標記行：probe … done；shell 印的字可能夾在中間；curl：SHA256SUMS；下載的 script：URL 與 SHA-256；curl 或 wget：下載 release；標記行：install … done；或，主機下載不了時：上傳的 script，接著原樣的 binary 或 tar；ferry_become <ferry> monitor --tag <tag>；exec 成 monitor；標記行：monitor 的 build、socket、port、secret，done。之後不再傳東西：client 握著，放手就是走了。每一行標記行是 \0ferry:<tag>:<kind>:<長度>:<內容>\n，一次 write；client 只讀帶自己 tag 的行。成為 monitor 之前，client 每 5 秒送一個 :，主機每 5 秒印一行 alive。原樣，不走 pipe：對面一開始是 sh，shell 留下的程式隨時可能寫進來。](assets/protocol-flow-first-session.svg)

- **兩端**：client 的 `ssh`，與主機上的 login shell，它執行 `sh -c '<script>'`。主機上唯一的 session。script 寫成一行 `printf` 的八進位跳脫字元，任何 login shell 都原樣轉給 `sh`（`src/remote/shell.rs`）。
- **主機 → client：標記行**，一次 write 一行。長度是內容的 bytes 數，用來發現被插話的行。一段的最後一行是 `done`，內容是這段的行數，掉了一行就看得出來。client 只讀帶自己 tag 的行，其餘當成 shell 印的，最後說一次。
  - `probe`：探測的結果，一行一個 `key=value`：machine ID、平台、Ferry 在主機上的目錄、裝了哪些 build、有沒有 cargo、curl 或 wget、能不能算 SHA-256、每個根目錄是什麼。
  - `install`：安裝的結果。
  - `alive`：成為 monitor 之前，每 5 秒一行，不算進任何一段。
  - `monitor`：monitor 說出 `build=`、`socket=`、`port=`、`secret=`，以 `done` 結束。client 在這裡比對 build，不同就拒絕。
- **client → 主機：一行一個指令。** script 讀一行執行一行：安裝的 script，最後是 `ferry_become <ferry> monitor --tag <tag>`。沒有別的要送時，client 每 5 秒送一個 `:`；主機的 watchdog 30 秒沒聽到 client，就結束這個 session。
- **上傳。** 主機下載不了 release，或驗不了 SHA-256 時，client 送出上傳的 script，接著原樣送出 binary，開發版則是原始碼的 tar，由主機的 `cat` 讀到結尾，驗過才裝：binary 照 SHA-256，主機沒有工具時照大小；開發版建好之後，要能執行，並說出自己是那個 build。上傳是這個 session 讀的最後一樣東西：讀完的 script 自己成為 monitor。
- **stderr**：直接接到 client 的終端機。monitor 的 stderr，以及它轉來的 server 的 stderr，也從這裡出來。
- **結束**：成為 monitor 之後不再傳東西，client 一直握著；client 放手，monitor 讀到輸入結束，就當 client 走了。它在 link 關掉之後才結束。
- **為什麼原樣**：對面一開始是 `sh`；shell 的 startup files，和它們留下的程式，都握著同一個 stdout，隨時可能寫進來。

### 3.3 link（走 pipe）

![link（走 pipe）：client 的 monitor 與主機的 monitor。01，或 port 的 secret；start mark：到了；Follow { 壓縮 }：說明用途，帶 link 的設定。ready：link 的 control pipe，雙向。listener 讀到 01 就回 start mark；從 port 來的，要先給對 64 個十六進位字元的 secret。沒等到 start mark，改試 port。](assets/protocol-flow-link.svg)

1. client → listener：`01`，或 port 的 secret。
2. listener → client：start mark。沒等到，表示 SSH 沒有 forward 到 socket，client 改試 port。
3. client → monitor：`Follow { 壓縮 }`。它說明這條連線是 link：monitor 讀到它之前，還不知道自己會不會一直握著這條連線。
4. 兩端 ready：一條 control pipe，雙向。

### 3.4 sudo 檢查（原樣）

![sudo 檢查（原樣）：只有 --remote-sudo，每台主機一次。01 或 secret；start mark；Launch { id, SudoCheck { Nothing } }；交出連線：起 sudo -n -- ferry build-id；一行 build，原樣讀到結尾；經 link：Ended { id, 結束碼, stderr }。不行時：client 問密碼，再開一條，Launch { SudoCheck { Password } } 加密碼行，起 sudo -k -S -p "" -- ferry build-id。最多問 3 次。fixture 同形，只有 ferry test：Launch { Fixture }，起 ferry fixture --until-stdin-closes，輸出原樣讀完。原樣，不走 pipe：對面是 sudo，只跑一次。](assets/protocol-flow-sudo.svg)

- 只有 `--remote-sudo`，每台主機一次，在起 server 之前。
- client 在 monitor 的 socket 開一條 channel，前兩步同 link，再送 `Launch { id, SudoCheck { Nothing } }`：monitor 起 `sudo -n -- ferry build-id`，先試不給密碼。
- 不行時，client 在終端機問密碼，再開一條，送 `Launch { id, SudoCheck { Password } }`，原樣寫出密碼行：monitor 起 `sudo -k -S -p "" -- ferry build-id`。最多問 3 次。
- 對面印一行 build，client 讀到結尾；它怎麼結束、stderr 說了什麼，monitor 在 link 上以 `Ended` 告訴 client。
- 之後每個 server 都經 sudo 起，密碼行照樣在 `Hello` 之前送。
- **為什麼原樣**：對面是 sudo，只跑一次，輸出原樣讀完。

### 3.5 control connection（走 pipe）

![control connection（走 pipe）：client 與主機上的 server。01 或 secret；start mark；Launch { id, Server }；交出連線：經 sudo 起 ferry server，stderr 是 lifeline；密碼行（要密碼時）；Hello { build、角色、根目錄、壓縮、… }；start mark、Welcome { socket、port、根目錄的身分 }、end mark。ready：reports（server → client），control（雙向）。client 依序寫出，不必等；每一段由箭頭終點的程式讀走，讀完才輪到下一個。本機沒有 listener、monitor 與 sudo：client 以 pipe 起 server，只有 Hello 與答案。](assets/protocol-flow-control.svg)

主機上：

1. client → listener：`01` 或 secret；listener → client：start mark。
2. client → monitor：`Launch { id, Server { sudo, port } }`。monitor 起 server，`--remote-sudo` 時經 sudo，把連線交給它當 stdin 與 stdout，自己不再讀；server 的 stderr 是它的 lifeline（[3.6](#36-lifeline原樣)）。
3. client → sudo：密碼行，只有要密碼時。
4. client → server：`Hello`。
5. server → client：start mark、`Welcome`，或 `Refused`、`Error`，再加 end mark，一次寫完。
6. server 寫完答案就 ready；client 讀到 end mark 就 ready：reports 與 control 兩條 pipe。

client 依序寫出第 2 到 4 步，不必等：每一段由該讀的程式讀走，讀完才輪到下一個。

本機：client 以 pipe 起 `ferry server --lifeline`，只有第 4 到 6 步。

### 3.6 lifeline（原樣）

![lifeline（原樣）：worker 的 stderr 與它的 monitor，不跨網路。起 worker：socket pair 的一端當它的 stderr；給人看的文字：sudo 的訊息、panic；\0ferry:lifeline:heard:<長度>:<毫秒>\n，每秒一行；什麼都不寫；它這端一關，worker 醒來、停下。從頭到尾原樣，直到 worker 結束或 monitor 不在。heard 是 worker 的 stream 多久之前收到對面的東西：那是經主機的 SSH 連線、從 client 那邊來的，monitor 當成聽到 client。給人看的文字：主機上轉到 first session 的 stderr；跑完就結束的 worker，留最多 64 KiB，隨 Ended 送回。](assets/protocol-flow-lifeline.svg)

- **兩端**：monitor 起的每個 worker，server、sudo 檢查、fixture，它的 stderr；與那個 monitor，主機上是主機的 monitor，本機是 client。是一個 socket pair 的兩端，不跨網路。
- **worker → monitor**：給人看的 stderr 文字，例如 sudo 的訊息、panic；其中夾著標記行 `\0ferry:lifeline:heard:<長度>:<毫秒>\n`，每秒一行，說 worker 的 stream 多久之前收到過對面的東西。那是經過主機的 SSH 連線、從 client 那邊來的，monitor 把它當成聽到了 client。除了 worker 沒有人寫這裡，給人看的文字也沒有 NUL，所以 tag 固定。
- **monitor → worker**：什麼都不寫。worker 等它可讀，只在 monitor 那一端關閉時醒來：monitor 不管怎麼結束，被 kill 或當掉，worker 都看得到，於是停下。沒有計時器，也不靠網路。
- **給人看的文字去哪**：主機上 server 的，由 monitor 轉到自己的 stderr，也就是 first session 的 stderr，到 client 的終端機；本機 server 的，由 client 直接轉到終端機。跑完就結束的 worker，sudo 檢查與 fixture，monitor 留最多 64 KiB，在 link 上隨 `Ended` 告訴 client。
- **sudo 記錄輸出時**：sudo 自己的 pipe 夾在中間，worker 發現 stderr 不是 socket，就說 lifeline 靠不住。
- **為什麼原樣**：stderr 誰都可以寫，sudo 會，panic 也會；它也不跨網路。

### 3.7 stream（走 pipe）

![stream（走 pipe）：兩個 server 之間，由 client 做。先寫進去：01 或 secret（主機）、編號、壓縮；先寫進去：01 或 secret（主機）、編號、壓縮；交出：本機經交接 fd；主機接上 ssh -W；交出：同左；start mark：Destination 那頭寫的；start mark：Source 那頭寫的。ready：manifest、data（S → D），answers（D → S），control（雙向）。主機上的 server，由 listener 讀 01 並回 start mark；另一頭在本機時，那個 server 不先寫東西，由 client 代它寫。server 讀完 編號、壓縮，跳到對面來的 start mark，就 ready。K 條 stream 各做一次，編號 0 到 K−1。](assets/protocol-flow-stream.svg)

1. client 做一個 socket pair，在兩端各先寫進：主機上的 server，`01` 或 secret；`[編號][壓縮]`；另一頭的 server 在本機時，再寫一個 start mark，因為本機的 server 不先寫東西。
2. client 交出兩端：本機的 server，經交接 fd（[3.8](#38-交接-fd原樣)）；主機上的 server，由 `ssh -W` 連到它的 socket，socket pair 的一端當 `ssh` 的 stdin 與 stdout。兩台主機之間，兩條 channel 就這樣背對背接起來，由 kernel 搬 bytes，client 不經手。
3. 主機上的 server，由 listener 讀 `01` 或 secret，回 start mark；它經 socket pair 到對面的 server。
4. 每個 server 讀 `[編號][壓縮]`，跳到對面來的 start mark，就 ready：manifest、data、answers 與 control。

K 條 stream 各做一次，編號 0 到 K−1；data 以外的 pipe 都走第 0 條。

### 3.8 交接 fd（原樣）

![交接 fd（原樣）：把一條連線交給 server。connect；00，附上兩個 fd（SCM_RIGHTS）；1 byte：收下了。之後 client 放掉它那一份；join 程序則一直等到 socket 關閉。本機的 stream：交出 socket pair 的一端，一份讀、一份寫；收下之前 client 一直握著，因為 macOS 會清掉沒人握著的。join 程序（--remote-join）：交出自己的 stdin 與 stdout。原樣，不走 pipe：只傳 fd。](assets/protocol-flow-handover.svg)

- **本機的 stream**：client 連到本機 server 的 socket，送 1 byte `00`，附上 socket pair 一端的 fd 兩份，一份讀、一份寫（SCM_RIGHTS）；server 收下，回 1 byte。在那之前，client 一直握著這一端：macOS 上，送出中、還沒被收下的 socket，沒有 process 握著，會被當成垃圾清掉。
- **`--remote-join`**：join 程序連到 server 或 monitor 的 socket，送 `00`，附上自己的 stdin 與 stdout；之後一直等到對面關掉 socket，因為主機的 session 會跟著它結束。
- socket 在只有這個使用者能開的目錄裡，名稱是隨機的 token。
- **為什麼原樣**：只傳 fd，沒有資料。

### 3.9 fixture（原樣）

- 只有 `ferry test`：在主機上產生、改動、驗證測試用的樹。流程和 sudo 檢查同形。
- client 在 monitor 的 socket 開一條 channel，送 `Launch { id, Fixture { arguments } }`：monitor 起 `ferry fixture --until-stdin-closes <動作>`，動作是產生、改動、算 digest、建立、標記、刪除或驗證。
- 它印的結果原樣寫回，例如 digest，client 讀到結尾。輸入只用來叫它停：client 關掉，或連線斷了，它就停。
- 怎麼結束、stderr 說了什麼，同 sudo 檢查，由 `Ended` 告訴 client。
- **為什麼原樣**：只跑一次，輸出原樣讀完，而且只在測試。

### 3.10 GitHub releases 與其他

- **GitHub releases**（不是 ferry 的協定）：client 用 curl 取這個版本的 `SHA256SUMS`，要上傳時，也取那個平台的 binary；主機在 first session 裡用 curl 或 wget 下載 release，照 `SHA256SUMS` 驗過才裝。
- **子程序的 pipe**：server 查 owner 的名稱時起 `getent`，用 pipe 接回它的輸出。它不是連線，列在這裡只為完整。
- **只在測試**：debug build 的 `ferry relay`，在沒有 `ssh` 的測試裡，代替 `ssh -W` 把 stdin 與 stdout 接到 socket 或 port。

### 3.11 `--remote-join`

給 SSH 既不 forward 到 socket、也不 forward 到 port 的主機用，由使用者以 `--remote-join` 指定。link、control connection 與 stream 的 channel 都經過主機的 shell，執行 `ferry server --join <socket>`：join 程序先在 stdout 寫 start mark，再把自己的 stdin 與 stdout 交給 socket（[3.8](#38-交接-fd原樣)）。之後同上，只是沒有 `01`。shell 印的東西在 start mark 之前，讀的一方跳過；shell 留下的程式在 start mark 之後才寫進來的，變成協定錯誤，傳輸失敗。

## 4. 走 pipe 的連線怎麼送

這一節只關於 link、control connection 與 stream，ready 之後的部分。

### 4.1 分層

![分層：寫的一方與讀的一方，各有四層。程式，peer、client 或 monitor，有什麼寫什麼，寫不下就等，不知道一次寫得下多少；讀的一方有什麼讀什麼，自己解出記錄。pipe：每種連線一組，用途與編號固定，單向、有序、可靠，讀到 End 才完整。transport：送出 buffer、壓縮、pump 排程與切成封包、守窗口；接收 thread 只分流，再解壓，讀走多少還多少窗口。連線，即 wire：準備時是原樣的 bytes，ready 之後是封包，經 SSH channel、socket pair 或 pipe。準備的 bytes 不走這條路；本機不壓縮，跨網路的每條 pipe 都壓。](assets/protocol-layers.svg)

- **程式**：peer、client、monitor。寫：`write(&[u8])`，全部收下才回來，寫不下就等，不知道一次寫得下多少。讀：有什麼讀什麼，自己解出一筆筆記錄。程式看不到封包、壓縮與窗口。
- **pipe**：一種內容一條，單向、有序、可靠。寫的一方寫完就關閉，transport 送出它的 End；讀的一方讀到 End，這條 pipe 才完整。每種連線一組 pipe，用途與編號固定（[4.4](#44-每種連線的-pipe)）。不同 pipe 之間沒有先後；寫的一方不知道對面讀了沒。介面還有 `sync()`：返回時，之前寫的都已到達對面，像檔案的 `fsync`，不保證對面的程式讀過（2026-10-28 使用者決定加入，[pipes](pipes.md#pipe)）。
- **transport**：送出端有每條 pipe 的送出 buffer、壓縮（[4.6](#46-壓縮)）與 pump，pump 排程、切成封包、守窗口（[4.7](#47-窗口)）；接收端的 thread 只把封包分進各 pipe，從不等程式。
- **連線**，即 wire：一條 SSH channel、socket pair 或 pipe。準備時原樣讀寫 bytes；ready 之後，上面只有封包。

### 4.2 連線的狀態

2026-10-28 使用者定的：狀態要明確，ready 之後才走壓縮的路；ready 之前的準備，用簡潔的方法就好。

![連線的狀態，兩端各有自己的一份，只往前走。準備：原樣的 bytes，到達、說明用途、交接、交換設定，沒有 pipe、窗口與壓縮。兩端都是最終的程式、設定交換過，經 Wire::stream() 進入 ready：有 pipe、窗口與壓縮，程式只在這裡讀寫，封包是 pipe、kind、長度與內容。每條 pipe 都讀到 End，兩個方向的 pipe 都結束，連線才結束。準備時斷了，或 ready 時在 End 之前斷了，就是失敗：傳輸失敗，不重連。每一端只有一個轉換點。](assets/protocol-states.svg)

- **準備。** 連線交到最終的程式手上，並交換 ready 要用的設定。原樣的 bytes，沒有 pipe、窗口與壓縮，照現在的做法（第 3 節的流程）。
- **ready。** 兩端都是最終的程式、設定也交換過了，transport 把連線轉成這種連線的一組 pipe。每一端只有一個轉換點，就是現在的 `Wire::stream()`（`src/transport/wire.rs`）。
- **結束。** 兩個方向的 pipe 都讀到 End，連線才結束（[4.8](#48-結束與失敗)）。
- **失敗。** 準備時斷了，或 ready 時在 End 之前斷了：傳輸失敗，不重連。唯一的例外在準備的第一步：直連 socket 的 channel 沒等到 start mark，表示 SSH 沒有 forward 到 socket，client 改試 port。
- 兩端各有自己的狀態，不必同時轉換：例如 control connection 上，server 寫完答案就 ready，client 讀到 end mark 才 ready。

### 4.3 準備帶的設定

- **規則：每種連線的準備，帶它自己那組 pipe 的設定**（2026-10-28 使用者同意）。link 在 `Follow`，control connection 在 `Hello`，stream 在 client 寫進去的 header。pipe 的用途與編號由連線的種類決定，兩端又是同一個 build，所以不逐條宣告。
- **壓縮由 client 為每條連線決定：**
  - `--no-compress`：都不壓；
  - `--compress-level N`，N 是 1 到 19：每條都以 N 壓，本機的也壓；
  - 否則：連到主機的連線以等級 1 壓，本機的不壓。

  現在的規則是對整個傳輸判斷：兩邊都在本機才不壓。改成每條連線各自判斷，stream 的結果和現在一樣。
- **`Hello` 其他的欄位照舊**：build、角色、根目錄、選項，以及測試用的覆寫。它的壓縮欄位，現在指 stream 上的 block，改成指這條 control connection 自己。

### 4.4 每種連線的 pipe

每筆記錄是長度（varint）加 postcard；一筆的上限待決定，建議 1 MiB。manifest、data、answers 的記錄見 [pipes 的記錄](pipes.md#記錄)；control connection 與 link 的記錄，就是現在的訊息。編號是這份草案定的。

**link**

| 編號 | pipe | 方向 | 記錄 | 窗口 |
|---|---|---|---|---|
| 0 | control | 雙向 | `Beat`、`Ping` 與 `Pong`、`Stop`、`Look` 與 `Looked`、`Ended`、`NotStarted`、`Paused`、`Resumed`、`Awaiting` | 無 |

**control connection**

| 編號 | pipe | 方向 | 記錄 | 窗口 |
|---|---|---|---|---|
| 0 | control | 雙向 | client 寫 `Proceed`、`Snapshot`；server 寫每秒的 metrics 與 carried、`Snapshot`、`Failed`、`Error`、`Done` | 無 |
| 1 | reports | server → client | 計數、dry run 列的路徑、警告、佇列、在傳的檔案、忙什麼、等級；最後是 `Profile` | 有 |

**stream**

| 編號 | pipe | 方向 | 記錄 | 窗口 |
|---|---|---|---|---|
| 0 | control | 雙向 | 錯誤 | 無 |
| 1 | manifest | S → D | `Root`、`In`、`Entry`、`Listed`、`Unlistable`、`User`、`Group`、`Problem`、`End` | 有 |
| 2 | data | S → D | `File`、`Segment`、`Hash`、`Unopened` | 有；塊分到所有 stream |
| 3 | answers | D → S | `File`、`Attributes`、`Compared`、`Failed`、`NotDeleted`、`NeedsEnd`、`Done` | 有 |

### 4.5 封包（草案）

![ready 之後的封包與壓縮，草案。a，封包：pipe 一個 byte、kind 一個 byte、長度四個 bytes，接著內容，最多 64 KiB；kind 0 是 Bytes，這條 pipe 的下一段內容；kind 1 是 Window，還這條 pipe 的窗口；kind 2 是 End，這條 pipe 寫完了，data 的帶塊數；kind 3 是 Sync，sync 的記號；kind 4 是 Synced，回覆記號之前的都到了。pipe 的編號依連線種類固定。b，一條串流壓縮的 pipe，以 manifest 為例：記錄一筆接一筆，壓成一條 zstd 串流，切成好幾個 Bytes 封包；記錄、壓縮的 block 與封包的邊界互不相干。c，data：每塊是序號、原樣或 zstd、長度與內容；一塊可以切成好幾個封包，都在同一條 stream 上，中間可以夾別的 pipe 的封包；塊分到各條 stream，接收端照序號排回。d，第一條 stream 的兩個方向：Source 往 Destination 有 manifest、data 與 control；反方向有 answers、還給 manifest 與 data 的窗口，以及 control。](assets/protocol-frames.svg)

- **格式**：`[pipe：1 byte][kind：1 byte][長度：4 bytes，little-endian][內容]`，內容最多 64 KiB。ready 之後，連線上只有這一種封包。
- **kind**：
  - `0 Bytes`：這條 pipe 的下一段內容；
  - `1 Window`：還這條 pipe 的窗口，內容是還的 bytes 數；
  - `2 End`：這條 pipe 寫完了；data 的帶塊數；
  - `3 Sync`：sync 的記號，帶一個編號；data 的還帶它之前送出的塊數；
  - `4 Synced`：接收端的 transport 回的，記號之前的都已到達，帶同一個編號。
- **串流壓縮的 pipe**：`Bytes` 的內容是這條 pipe 的 zstd 串流的下一段；不壓時就是 pipe 的 bytes。記錄、壓縮的 block 與封包的邊界互不相干。
- **data 的塊**：`[序號：8 bytes][0 原樣、1 zstd：1 byte][長度：4 bytes][內容]`。一塊在一條 stream 上，可以切成好幾個 `Bytes` 封包，中間可以夾別的 pipe 的封包；一條 stream 一次只送一塊。接收端照序號排回順序。
- **走哪條 stream**：data 的塊分到各條；其他 pipe 的封包，以及所有的 `Window`、`Sync`、`Synced`，都走第 0 條。`Window` 與 `Synced` 走它那條 pipe 的反方向。
- **sync**：寫的一方呼叫 `sync()`，transport 先送出這條 pipe 還沒送的，再送 `Sync`；接收端分流到它時回 `Synced`，data 的要等它之前的塊都到了；收到 `Synced`，`sync()` 返回。
- **排程**：control、`Window` 與 `Synced` 最先；answers 與 reports 其次；data 與 manifest 之間照現在的規則，data 的 buffer 至少四分之一滿時 data 先，否則 manifest 先。

### 4.6 壓縮

- 以連線為單位設定（[4.3](#43-準備帶的設定)）。跨網路的每條 pipe 都壓，沒有例外；本機不壓（2026-10-28 使用者決定）。
- **串流的 pipe**：manifest、answers、reports、control，每條一個 zstd 串流，寫的一方一個壓縮狀態，讀的一方一個解壓狀態。
- **data**：切成塊，建議 2 MiB，由一組 thread 平行壓縮與解壓，建議 8 條；壓了不會變小的塊原樣送；不滿一塊的，輪到 data 時拿到多少算一塊。塊多大、幾條 thread 待決定。
- **flush**（2026-10-28 使用者決定）：不等。壓縮由 pump 往回拉：輪到一條 pipe、下游有空間時，才把它送出 buffer 裡的全部拿去壓，最多一個封包的量，壓完就 flush。下游慢時自然攢得多、壓得好；不慢時拿到多少送多少，多出的 bytes 在線路的空檔裡送完。慢速連線上的中型傳輸會多送一些；為什麼不等、模擬的數字，見 [pipes 的待決定](pipes.md#待決定的問題)。
- 完整性檢查不在這裡：檔案的 BLAKE3 hash、分段的 chaining value，由 peer 算與驗證。

### 4.7 窗口

- 有窗口的是 manifest、data、answers、reports；control 沒有。
- 窗口由讀的一方給，單位是連線上的 bytes，也就是封包的內容，壓縮之後的。
- 寫的一方用完窗口就等；讀的一方的程式讀走多少，transport 就還多少，攢到窗口的四分之一再送 `Window`。`sync()` 也一樣：之前寫的送不出去，它就等。
- 接收 thread 從不等程式：對方不會超過窗口，接收 buffer 一定放得下。
- 初始值：data 用現在的資料額度 64 MiB；manifest、answers、reports 待決定。

### 4.8 結束與失敗

- 寫的一方寫完一條 pipe 就關閉，transport 送出它的 `End`；讀的一方讀到 `End`，這條 pipe 才完整。
- 一條連線上，兩個方向的 pipe 都 `End` 了，transport 才關掉連線。
- 連線讀到結尾時，對方寫的 pipe 還有沒 `End` 的，就是斷了：傳輸失敗，不重連。K 大於 1 時，第 1 條以後的 stream 只帶 data 的塊：斷在一塊的中間，或 data 的 `End` 說的塊數沒有到齊，就是斷了。
- 對方是否還在，照舊由 monitor 的 heartbeat 判斷（architecture 原則 10）；連線上任何 bytes 進來都算聽到。
- 各連線什麼時候結束，照現在：server 的 peer 做完，送 `Done`，等 client 結束 control connection；client 的 monitor 在兩個 server 都做完時結束它們；link 在主機上的東西都結束之後才關。
- **`Done` 只說送的一方自己做完了**，收到的一方照自己的狀態判斷自己做完沒有：monitor 收到 `Done` 只關掉 client 這邊；worker 要等 process 結束、而且這條連線上每條 pipe 都讀到 `End`，才算結束（2026-10-28 使用者定的）。所以 `Done` 走 control，和最後幾筆報告誰先到都無妨。

## 5. 和現在的差別

原樣的連線都不變。走 pipe 的：

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

## 6. 待決定

都在 [pipes 的待決定](pipes.md#待決定的問題)：

- 封包的格式與 pipe 的編號：[4.4](#44-每種連線的-pipe) 與 [4.5](#45-封包草案) 是草案；
- needs 與 results 合成一條 answers；
- 窗口的初始值；
- data 的塊多大、壓縮與解壓各幾條 thread；
- pack 整個拿掉；
- 一筆記錄的上限。
