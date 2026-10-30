# Ferry 傳輸協定

[English](protocol.md)

一次傳輸用到的每一條連線：誰和誰之間、怎麼建立、攜帶什麼、怎麼結束，到每個位元組。為什麼這樣設計，寫在[架構](architecture.zh-TW.md)的 [Transport](architecture.zh-TW.md#transport)、[Connection](architecture.zh-TW.md#connection) 與 [Monitor](architecture.zh-TW.md#monitor)；peer 怎麼處理讀到的東西，寫在[管線](pipeline.zh-TW.md)。Ferry 說話的每條連線，兩端都是同一個 build，交握時會檢查，所以這裡的東西都沒有版本：除了 `Hello` 與 `Refused` 的第一個欄位，別的 build 的 client 也要讀得懂，其餘的每個 build 都可能改。

**目錄**

- [每一條連線](#每一條連線)
- [兩類連線](#兩類連線)
- [原樣傳送的東西](#原樣傳送的東西)
- [每條連線的流程](#每條連線的流程)
  - [SSH connection](#ssh-connection)
  - [第一個 session](#第一個-session)
  - [連結](#連結)
  - [sudo 的檢查](#sudo-的檢查)
  - [control connection](#control-connection)
  - [生命線](#生命線)
  - [stream](#stream)
  - [交接](#交接)
  - [fixture](#fixture)
  - [release 與其他](#release-與其他)
  - [`--remote-join`](#--remote-join)
- [連線怎麼攜帶 pipe](#連線怎麼攜帶-pipe)
  - [分層](#分層)
  - [連線的狀態](#連線的狀態)
  - [準備時帶的設定](#準備時帶的設定)
  - [每種連線的 pipe](#每種連線的-pipe)
  - [frame](#frame)
  - [壓縮](#壓縮)
  - [窗口](#窗口)
  - [結束與失敗](#結束與失敗)
- [訊息](#訊息)
- [pipe 攜帶什麼](#pipe-攜帶什麼)

## 每一條連線

![一次傳輸的每一條連線，以這台機器上的 client 與 Source 的 server、host 上的 monitor 與 Destination 的 server 為例；實線攜帶 pipe，虛線是原樣的位元組，編號是建立的順序。1，唯一的 SSH connection，TCP，跨機器的每條連線都是它的 channel。2，第一個 session，經過登入 shell，原樣：探測、安裝，然後成為 host 的 monitor；安裝時，client 以 curl 從 GitHub 取 SHA256SUMS 與要上傳的 binary，host 以 curl 或 wget 下載 release。3，連結，client 的 monitor 與 host 的 monitor 之間，一條 control pipe。4，sudo 的檢查，只有 --remote-sudo，原樣；fixture，只有 ferry test，原樣。5，control connection，client 與每個 server 之間，reports 與 control，本機的不壓縮；server 的標準錯誤是它的生命線，接到它的 monitor，原樣，從不跨網路。6，stream，兩個 server 之間，manifest、data、answers 與 control；本機那一端經 server 的 socket 交接給它。](../assets/protocol-connections.svg)

**已決定。** 一次傳輸用到的每一條連線，以 Destination 在 host 上、client 與 Source 在本機為例，照建立的順序，編號同圖：生命線與它的 server 的 control connection 同號，交接與它交出的 stream 同號：

| # | 連線 | 兩端 | 攜帶 | 方式 |
|---|---|---|---|---|
| 1 | [SSH connection](#ssh-connection) | client 的 `ssh` master 與 host 的 `sshd` | SSH 自己的協定；跨機器的每條連線都是它的 channel | 不是 Ferry 的 |
| 2 | [第一個 session](#第一個-session) | client 與 host 上的 `sh`，之後成為 monitor | 加了標記的行；成為 monitor 之後什麼都不傳，只是開著 | 原樣 |
| 3 | [連結](#連結) | client 的 monitor 與 host 的 monitor | 它的 control pipe | pipe |
| 4 | [sudo 的檢查](#sudo-的檢查)，只有 `--remote-sudo` | client 與 monitor 啟動的 `sudo ferry build-id` | 密碼行；回一行 build | 原樣 |
| 5 | [server 的 control connection](#control-connection) | client 與 server，由 monitor 啟動，在本機由 client 啟動 | control 與 reports | pipe，本機的不壓縮 |
| 5 | [生命線](#生命線) | monitor 啟動的每個程式的標準錯誤，與那個 monitor | 給人看的文字；每秒一行加了標記的行，說它的 stream 多久前聽到對方 | 原樣 |
| 6 | [stream](#stream)，1 到 8 條 | Source 與 Destination 的 server | control、manifest、data、answers | pipe |
| 6 | [交接](#交接) | client 或加入的行程，與 server 或 monitor 的 socket | 一個位元組與兩個描述元 | 原樣 |
| — | [fixture](#fixture)，只有 `ferry test` | client 與 `ferry fixture` | 它的輸出，讀到結束 | 原樣 |
| — | 安裝用的 [release](#release-與其他) | client，或 host 的第一個 session，與 GitHub | HTTPS，以 `curl` 或 `wget` | 不是 Ferry 的 |

依序：

1. client 開啟 host 的 SSH connection：只登入，不執行任何東西；要密碼或 OTP 時，在終端機問。
2. 第一個 session 探測 host；需要時安裝這個 build，由 host 自己下載，或由 client 上傳；最後成為 monitor，說出它的 build、socket、port 與秘密。
3. client 的 monitor 在連結上跟著 host 的 monitor，心跳開始。從此 host 上的每個程式都由 monitor 啟動，各一條 channel，標準錯誤各是一條生命線。
4. `--remote-sudo` 時，sudo 的檢查：要不要密碼，給的對不對。
5. 每個 server 一條 control connection：交握之後兩邊都等，client 比對兩個 root，再叫它們繼續，並說有幾條 stream。
6. K 條 stream；每條在本機的那一端，交接給它的 server。
7. 傳輸做完：control connection 與 stream 結束；連結在 host 上的東西都結束之後才關，第一個 session 最後。

兩邊都在本機時，沒有 1 到 4：client 經由管道啟動兩個 server，它們的生命線就是 client 自己的，每條 stream 的兩端都以交接給出。

## 兩類連線

- **攜帶 pipe 的**：連結、control connection、stream。兩端都是 Ferry。先**準備**，原樣的位元組：連到它的程式、說明用途、交接、告知設定；連線交到使用它的程式手上之後，就 **ready**，成為這種連線的一組 pipe。從此程式只在 pipe 上讀寫，transport 負責 frame、窗口與壓縮；見[連線怎麼攜帶 pipe](#連線怎麼攜帶-pipe)。
- **原樣的**，從頭到尾都是原樣的位元組：第一個 session、生命線、sudo 的檢查、fixture、交接。對面不是 Ferry，或誰都可能寫進去，或只傳描述元。SSH connection 與 GitHub 也屬於這一類，但它們不是 Ferry 的協定。

## 原樣傳送的東西

**已決定。** ready 之前，或在從不 ready 的連線上，原樣傳送的東西：

| 什麼 | 位元組 | 誰寫，誰讀 |
|---|---|---|
| 直連 | 一個位元組，1 | client；socket 的 listener，它回開頭標記 |
| port 的秘密 | 64 個十六進位數字 | client；port 的 listener，交出別的就放掉這條連線 |
| 開頭標記 | `\0ferry: start\0`，14 個位元組 | 對面的程式，在它之後寫的就是使用這條連線的程式；讀的一方跳到它 |
| 結束標記 | `\0ferry: end\0`，12 個位元組 | server，寫在交握的回覆之後；client 以它發現被插入東西的回覆 |
| 加了標記的行 | `\0ferry:<tag>:<kind>:<length>:<content>\n` | host 在第一個 session 上寫，tag 是 client 的，16 個十六進位數字，每次執行重新產生；生命線上的，tag 是 `lifeline` |
| packet | `[type][length][payload]`，類型一個位元組，長度四個位元組，little-endian，payload 是 postcard | client 的 `Follow`、`Launch` 與 `Hello`；server 的回覆 |
| 密碼行 | 密碼加換行 | client；`sudo -S`，只在 `--remote-sudo` 要密碼時 |
| stream 的標頭 | `[number][level]`，兩個位元組，不壓縮時等級是 0 | client；server |
| 交接 | 一個位元組 0，以 `SCM_RIGHTS` 附上兩個描述元；回一個位元組 | client 或加入的行程；server 或 monitor，以此說它收下了 |

連到 socket 或 port 的連線，30 秒內什麼都沒來，第一個位元組、第一個 packet 都沒有，就放掉；連到 port 的，同時最多等 16 條交出秘密。

## 每條連線的流程

照一次傳輸裡建立的順序。時序圖裡，箭頭的終點是讀走那一段的程式；綠色的箭頭是開頭標記。底下一列綠色的是 ready，之後攜帶 pipe；灰色的表示從頭到尾原樣。

### SSH connection

不是 Ferry 的協定。

- 每個 host 一條 TCP 連線，經 `ssh` 的 ControlMaster：你自己設了就共用，否則用 Ferry 自己的，由一個 `ssh -N` 握著。登入時要密碼或 OTP，在終端機問。
- `ssh -O check` 經 master 在本機的 control socket，問它還在不在。
- 跨機器的每條連線都是它的一條 channel；它斷了，傳輸就失敗，不重連。

它的選項見 [Connection](architecture.zh-TW.md#connection)。

### 第一個 session

![第一個 session，原樣：client 與 host 上的 sh，之後成為 monitor。ssh 執行 sh -c <探測的 script>，經過登入 shell；加了標記的行：probe … done，shell 印的東西可能夾在中間；curl：SHA256SUMS；下載的 script，URL 與 SHA-256；curl 或 wget：release；加了標記的行：install … done；或者，host 下載不了時：上傳的 script，接著原樣的 binary 或 tar；ferry_become <ferry> monitor --tag <tag>；exec，成為 monitor；加了標記的行：monitor 的 build、socket、port 與秘密，done。之後不再傳東西：client 握著，放手就是離開。每一行加了標記的行是 \0ferry:<tag>:<kind>:<length>:<content>\n，一次 write；client 只讀自己 tag 的行。成為 monitor 之前，client 每 5 秒送一個 :，host 每 5 秒印一行 alive。原樣，不攜帶 pipe：對面一開始是 sh，shell 留下的程式隨時可能寫進來。](../assets/protocol-flow-first-session.svg)

- **兩端**：client 的 `ssh`，與 host 的登入 shell，它執行 `sh -c '<script>'`：host 唯一的 session。script 寫成一行 `printf` 的八進位跳脫字元，任何登入 shell 都原樣交給 `sh`（`src/remote/shell.rs`）。
- **host → client：加了標記的行**，一行一次 write。長度是內容的位元組數，用來發現被插入東西的行。一段的最後一行是 `done`，內容是這段的行數，掉了一行就看得出來。client 只讀自己 tag 的行，其餘當成 shell 印的，最後說一次。
  - `probe`：探測的結果，一行一個 `key=value`：machine ID、平台、Ferry 在 host 上的目錄、裝了哪些 build、有沒有 cargo、curl 或 wget、能不能算 SHA-256，以及每個 root 是什麼。
  - `install`：安裝的結果。
  - `alive`：成為 monitor 之前，每 5 秒一行，不算進任何一段。
  - `monitor`：monitor 說出 `build=`、`socket=`、`port=`、`secret=`，以 `done` 結束。client 在這裡比對 build，不同就拒絕。
- **client → host：一行一個指令。** script 讀一行執行一行：安裝的 script，最後是 `ferry_become <ferry> monitor --tag <tag>`。沒有別的要送時，client 每 5 秒送一個 `:`；host 的 watchdog 30 秒沒聽到 client，就結束這個 session，上傳中的檔案變大也算聽到 client。
- **上傳。** host 下載不了 release，或驗不了 SHA-256 時，client 送出上傳的 script，接著原樣送出 binary，開發版則是原始碼的 tar，由 host 的 `cat` 讀到結尾。驗過才裝：binary 照 SHA-256，host 沒有工具時照大小；開發版建好之後，要能執行，並說出自己是那個 build。上傳是這個 session 讀的最後一樣東西：script 讀完，就成為 monitor。
- **標準錯誤**直接接到 client 的終端機，monitor 的、以及它轉來的 server 的也是。
- **結束。** 成為 monitor 之後不再傳東西，client 一直握著；client 放手，monitor 讀到輸入的結尾，就當 client 走了。它在連結關掉之後才結束。
- **為什麼原樣**：對面一開始是 `sh`；shell 的啟動檔，和它們留下的程式，共用它的標準輸出，隨時可能寫進來。

### 連結

![連結，攜帶 pipe：client 的 monitor 與 host 的 monitor。01，或 port 的秘密；開頭標記：到了；Follow { 壓縮 }：說明用途，帶連結的設定。ready：連結的 control pipe，雙向。listener 讀到 01 就回開頭標記；從 port 來的，要先給對 64 個十六進位數字的秘密。沒等到開頭標記，client 改試 port。](../assets/protocol-flow-link.svg)

1. client → listener：`01`，或 port 的秘密。
2. listener → client：開頭標記。沒等到，表示 SSH 不轉發到 socket，client 改試 port。
3. client → monitor：`Follow { compress }`。它說明這條連線是連結：monitor 讀到它之前，還不知道自己會不會一直握著這條連線。
4. 兩端 ready：一條 control pipe，雙向。

client 也在它上面以 `Ping` 與 `Pong` 量往返時間，決定傳輸要幾條 stream。

### sudo 的檢查

![sudo 的檢查，原樣：只有 --remote-sudo，每個 host 一次。01 或秘密；開頭標記；Launch { id, SudoCheck { Nothing } }；交出連線：啟動 sudo -n -- ferry build-id；一行 build，原樣讀到結尾；經連結：Ended { id, 結束碼, 標準錯誤 }。不行時：client 問密碼，再開一條，Launch { SudoCheck { Password } } 加密碼行，啟動 sudo -k -S -p "" -- ferry build-id。最多問 3 次。fixture 同形，只有 ferry test：Launch { Fixture }，啟動 ferry fixture --until-stdin-closes，輸出原樣讀完。原樣，不攜帶 pipe：對面是 sudo，只跑一次。](../assets/protocol-flow-sudo.svg)

- 只有 `--remote-sudo`，每個 host 一次，在啟動任何 server 之前。
- client 在 monitor 的 socket 開一條 channel，前兩步同連結，再送 `Launch { id, SudoCheck { Nothing } }`：monitor 啟動 `sudo -n -- ferry build-id`，先試不給密碼。
- 不行時，client 在終端機問密碼，再開一條，送 `Launch { id, SudoCheck { Password } }`，原樣寫出密碼行：monitor 啟動 `sudo -k -S -p "" -- ferry build-id`。最多問 3 次。
- 對面印一行 build，client 讀到結尾；它怎麼結束、標準錯誤說了什麼，monitor 在連結上以 `Ended` 告訴 client。
- 之後每個 server 都經 sudo 啟動，密碼行照樣在 `Hello` 之前送。
- **為什麼原樣**：對面是 sudo，只跑一次，輸出原樣讀完。

### control connection

![control connection，攜帶 pipe：client 與 host 上的 server。01 或秘密；開頭標記；Launch { id, Server }；交出連線：經 sudo 啟動 ferry server，標準錯誤是生命線；密碼行，要密碼時；Hello { build、角色、root、壓縮、… }；開頭標記、Welcome { socket、port、root 的身分 }、結束標記。ready：reports（server → client）、control（雙向）。client 依序寫出，不必等；每一段由箭頭終點的程式讀走，讀完才輪到下一個。本機沒有 listener、monitor 與 sudo：client 經由管道啟動 server，只有 Hello 與回覆。](../assets/protocol-flow-control.svg)

在 host 上：

1. client → listener：`01` 或秘密；listener → client：開頭標記。
2. client → monitor：`Launch { id, Server { sudo, port } }`。monitor 啟動 server，`--remote-sudo` 時經 sudo，把連線交給它當標準輸入與輸出，自己不再讀；server 的標準錯誤是它的[生命線](#生命線)。
3. client → sudo：密碼行，只有要密碼時。
4. client → server：`Hello`；見[訊息](#訊息)。
5. server → client：開頭標記，接著 `Welcome`，或 `Refused`、`Error`，再加結束標記，一次寫完。
6. server 寫完回覆就 ready；client 讀到結束標記就 ready：reports 與 control。

client 依序寫出第 2 到 4 步，不必等：每一段由該讀的程式讀走，讀完才輪到下一個。

在本機：client 經由管道啟動 `ferry server --lifeline`，只有第 4 到 6 步。

ready 之後，server 什麼都不改，等 client 比對完兩個 root，送來 `Proceed`，說它絕不能進入或刪除什麼，以及有幾條 stream；control connection 反而關掉的話，server 就結束。

### 生命線

![生命線，原樣：worker 的標準錯誤與它的 monitor，從不跨網路。啟動 worker：socket pair 的一端當它的標準錯誤；給人看的文字：sudo 的訊息、panic；\0ferry:lifeline:heard:<length>:<毫秒>\n，每秒一行；什麼都不寫；它這端一關，worker 醒來、停下。從頭到尾原樣，直到 worker 結束或 monitor 不在。heard 是 worker 的 stream 多久之前收到對面的東西：那是經 host 的 SSH connection、從 client 那邊來的，monitor 當成聽到 client。給人看的文字：在 host 上，轉到第一個 session 的標準錯誤；跑完就結束的 worker，留最多 64 KiB，隨 Ended 送回。](../assets/protocol-flow-lifeline.svg)

- **兩端**：monitor 啟動的每個 worker，即 server、sudo 的檢查、fixture，它的標準錯誤；與那個 monitor，在 host 上是 host 的 monitor，在本機是 client。兩者是一個 socket pair 的兩端，從不跨網路。
- **worker → monitor**：標準錯誤上給人看的文字，例如 sudo 的訊息、panic；其中夾著加了標記的行 `\0ferry:lifeline:heard:<length>:<毫秒>\n`，每秒一行，說 worker 的 stream 多久之前收到過對面的東西。那是經過 host 的 SSH connection、從 client 那邊來的，monitor 把它當成聽到了 client。除了 worker 沒有人寫這裡，給人看的文字也沒有 NUL，所以 tag 固定。
- **monitor → worker**：什麼都不寫。worker 等它可讀，而它只在 monitor 那一端關閉時可讀：monitor 不管怎麼結束，被 kill 或當掉，worker 都看得到，於是停下。沒有計時器，也不經網路。
- **給人看的文字去哪**：host 上 server 的，由 monitor 轉到自己的標準錯誤，也就是第一個 session 的，到 client 的終端機；本機 server 的，由 client 轉到終端機。跑完就結束的 worker，即 sudo 的檢查與 fixture，monitor 留最多 64 KiB，在連結上隨 `Ended` 告訴 client。
- **sudo 記錄輸出時**：sudo 自己的管道夾在中間，worker 發現標準錯誤不是 socket，就說生命線靠不住。
- **為什麼原樣**：標準錯誤誰都可以寫，sudo 會，panic 也會；它也不跨網路。

### stream

![stream，攜帶 pipe：兩個 server 之間，由 client 建立。先寫進去：01 或秘密（host）、編號、等級；另一端同樣先寫進去。交出：本機經交接；host 上接到 ssh -W。開頭標記，Destination 那邊寫的；開頭標記，Source 那邊寫的。ready：manifest 與 data（S → D）、answers（D → S）、control（雙向）。host 上的 server，由 listener 讀 01 並回開頭標記；另一邊的 server 在本機時，它不先寫東西，由 client 代它寫。server 讀完編號與等級，跳到對面來的開頭標記，就 ready。K 條 stream 各做一次，編號 0 到 K−1。](../assets/protocol-flow-stream.svg)

1. client 建立一個 socket pair，在兩端各先寫進：host 上的 server，`01` 或秘密；標頭 `[number][level]`；另一邊的 server 在本機時，再寫一個開頭標記，因為本機的 server 不先寫東西。
2. client 交出兩端：本機的 server，經[交接](#交接)；host 上的 server，當 `ssh -W` 到它的 socket 的標準輸入與輸出。兩台 host 之間，兩條 channel 就這樣背對背接起來，由 kernel 搬位元組，client 不經手。
3. host 上的 server，由 listener 讀 `01` 或秘密，回開頭標記，它經 socket pair 到另一邊的 server。
4. 每個 server 讀標頭，跳到對面來的開頭標記，就 ready：manifest、data、answers 與 control。

K 條 stream 各做一次，編號 0 到 K−1，K = min(8, max(1, ⌈RTT ÷ 15 ms⌉))，RTT 是連結量到的、到最遠的 host 的往返時間，本機是一條；`Proceed` 告訴每個 server 有幾條。第一條攜帶每條 pipe；其他的只攜帶 data 的塊。

### 交接

![交接，原樣：把一條連線交給 server。connect；00，附上兩個描述元（SCM_RIGHTS）；1 個位元組：收下了。之後 client 放掉它那一份；加入的行程則一直等到 socket 關閉。本機的 stream：socket pair 的一端，一份讀、一份寫；收下之前 client 一直握著，因為 macOS 會清掉沒人握著的。加入的行程（--remote-join）：它自己的標準輸入與輸出。原樣，不攜帶 pipe：只傳描述元。](../assets/protocol-flow-handover.svg)

- **本機的 stream**：client 連到本機 server 的 socket，送一個位元組 `00`，附上 socket pair 一端的描述元兩份，一份讀、一份寫（`SCM_RIGHTS`）；server 收下，回一個位元組。在那之前，client 一直握著這一端：macOS 上，送往還沒 accept 的連線、沒有行程握著的 socket，會被當成垃圾清掉。
- **`--remote-join`**：加入的行程連到 server 或 monitor 的 socket，送 `00`，附上自己的標準輸入與輸出；之後一直等到對面關掉 socket，因為 host 的 session 會跟著它結束。
- socket 在只有這個使用者能開的目錄裡，以隨機的 token 命名。
- **為什麼原樣**：只傳描述元，沒有資料。

### fixture

- 只有 `ferry test`：在 host 上產生、改動、檢查測試用的樹。流程和 sudo 的檢查同形。
- client 在 monitor 的 socket 開一條 channel，送 `Launch { id, Fixture { arguments } }`：monitor 啟動 `ferry fixture --until-stdin-closes <動作>`，動作是產生、改動、算 digest、建立、標記、刪除或檢查。
- 它印的結果，例如 digest，原樣寫回，client 讀到結尾。輸入只用來叫它停：client 關掉，或連線斷了，它就停。
- 怎麼結束、標準錯誤說了什麼，同 sudo 的檢查，由 `Ended` 告訴 client。
- **為什麼原樣**：只跑一次，輸出原樣讀完，而且只在測試。

### release 與其他

- **GitHub release**，不是 Ferry 的協定：client 以 curl 取這個版本的 `SHA256SUMS`，要上傳時，也取那個平台的 binary；host 在第一個 session 裡以 curl 或 wget 下載 release，照 `SHA256SUMS` 驗過才裝。
- **子行程的管道**：server 查 owner 的名稱時啟動 `getent`，以管道讀回它的輸出。它不是連線，列在這裡只為完整。
- **只在測試**：debug build 的 `ferry relay`，在沒有 `ssh` 的測試裡，代替 `ssh -W` 把標準輸入與輸出接到 socket 或 port。

### `--remote-join`

給 SSH 既不轉發到 socket、也不轉發到 port 的 host 用，由使用者以 `--remote-join` 指定。連結、control connection 與 stream 的 channel，都經過 host 的 shell，執行 `ferry server --join <socket>`：加入的行程先在標準輸出寫開頭標記，再把自己的標準輸入與輸出交給 socket（[交接](#交接)）。之後同上，只是沒有 `01`。shell 印的東西在開頭標記之前，讀的一方跳過；shell 留下的程式在它之後才寫進來的，只會寫進協定裡，傳輸失敗。

## 連線怎麼攜帶 pipe

這一節只關於連結、control connection 與 stream，ready 之後的部分。

### 分層

![分層：寫的一方與讀的一方，各有四層。程式，即 peer、client 或 monitor，有什麼寫什麼，寫不下就等，不知道一次寫得下多少；讀的一方有什麼讀什麼，自己切出記錄。pipe：每種連線一組，用途與編號固定，單向、依序、可靠，讀到 End 才完整。transport：送出 buffer、壓縮、pump 的排程與切成 frame、守窗口；接收的 thread 只分流，再解壓，讀走多少還多少窗口。連線，即 wire：準備時是原樣的位元組，ready 之後是 frame，經 SSH channel、socket pair 或管道。準備的位元組不走這條路；本機不壓縮，跨網路的每條 pipe 都壓。](../assets/protocol-layers.svg)

- **程式**：peer、client、monitor。寫：`write(&[u8])`，全部收下才回來，寫不下就等，不知道一次寫得下多少。讀：有什麼讀什麼，自己切出一筆筆記錄。程式看不到 frame、壓縮與窗口。
- **pipe**：一種內容一條，單向、依序、可靠。寫的一方寫完就關閉，transport 送出它的 `End`；讀的一方讀到 `End`，這條 pipe 才完整。每種連線一組 pipe，用途與編號固定；見[每種連線的 pipe](#每種連線的-pipe)。不同 pipe 之間沒有先後；寫的一方不知道對面讀了沒。寫的一方可以 `sync`，返回時對面的程式已經讀完之前寫的；Ferry 裡目前沒有東西 sync。
- **transport**：送出端有每條 pipe 的送出 buffer、壓縮與 pump，pump 排程、切成 frame、守窗口；接收端的 thread 只把 frame 分進各條 pipe，從不等程式。
- **連線**，即 wire：一條 SSH channel、socket pair 或管道。準備時原樣讀寫位元組；ready 之後，上面只有 frame。

### 連線的狀態

![連線的狀態，兩端各有自己的一份，只往前走。準備：原樣的位元組，連到、說明用途、交接、告知設定，沒有 pipe、窗口與壓縮。兩端都是使用它的程式、設定也告知了，經 Wire::stream() 進入 ready：有 pipe、窗口與壓縮，程式只在這裡讀寫，frame 是 pipe、kind、長度與內容。兩個方向的每條 pipe 都讀到 End，連線就結束。準備時斷了，或 ready 之後在 End 之前斷了，就是失敗：傳輸失敗，不重連。每一端只轉換一次。](../assets/protocol-states.svg)

- **準備。** 連線連到使用它的程式，並告知 ready 要用的設定。原樣的位元組，沒有 pipe、窗口與壓縮，照上面每條連線的流程。
- **ready。** 兩端都是使用它的程式、設定也告知了，transport 把連線變成這種連線的一組 pipe。每一端只轉換一次，在 `Wire::stream()`（`src/transport/wire.rs`）。
- **結束。** 兩個方向的每條 pipe 都讀到 `End`；見[結束與失敗](#結束與失敗)。
- **失敗。** 準備時斷了，或 ready 之後在 `End` 之前斷了：傳輸失敗，不重連。唯一的例外在準備的第一步：直連 socket 的 channel 沒等到開頭標記，表示 SSH 不轉發到 socket，client 改試 port。
- 兩端各有自己的狀態，不必同時轉換：例如 control connection 上，server 寫完回覆就 ready，client 讀到結束標記才 ready。

### 準備時帶的設定

- **每種連線的準備，帶它自己那組 pipe 的設定**：連結在 `Follow`，control connection 在 `Hello`，stream 在 client 寫進去的標頭。有哪些 pipe、編號多少，由連線的種類決定，兩端又是同一個 build，所以不逐條宣告。
- **壓縮由 client 為每條連線決定：**
  - `--no-compress`：都不壓；
  - `--compress-level N`，N 是 1 到 19：每條都以 N 壓，本機的也壓；
  - 否則：連到 host 的連線以 zstd 等級 1 壓，本機的不壓。

### 每種連線的 pipe

pipe 上的每筆記錄是長度（varint）加 postcard 編碼；一筆最多是一段加 64 KiB，現在是 2 MiB + 64 KiB，超過就是協定錯誤，不認得的 pipe 編號或 kind 也是。每條 pipe 攜帶什麼，見 [pipe 攜帶什麼](#pipe-攜帶什麼)；control 與 reports 上的訊息是 [packet](#訊息)。

**連結**

| 編號 | pipe | 方向 | 攜帶 | 窗口 |
|---|---|---|---|---|
| 0 | control | 雙向 | `Beat`、`Ping` 與 `Pong`、`Stop`、`Look` 與 `Looked`、`Ended`、`NotStarted`、`Paused`、`Resumed`、`Awaiting` | 無 |

**control connection**

| 編號 | pipe | 方向 | 攜帶 | 窗口 |
|---|---|---|---|---|
| 0 | control | 雙向 | client 的 `Proceed` 與 `Snapshot`；server 每秒的 metrics 與 stream 送收了多少、它的 snapshot、它的失敗，以及它的 peer 做完了 | 無 |
| 1 | reports | server → client | 計數、dry run 列出的路徑、警告、佇列、正在傳的檔案、thread 在忙什麼、等級；最後是 profile | 1 MiB |

**stream**

| 編號 | pipe | 方向 | 攜帶 | 窗口 |
|---|---|---|---|---|
| 0 | control | 雙向 | 錯誤 | 無 |
| 1 | manifest | Source → Destination | `Root`、`In`、`Entry`、`Listed`、`Unlistable`、`User`、`Group`、`Problem`、`End` | 2 MiB |
| 2 | data | Source → Destination | `File`、`Segment`、`Hash`、`Unopened`、`End` | 64 MiB；它的塊分到每條 stream |
| 3 | answers | Destination → Source | `File`、`Attributes`、`Compared`、`Failed`、`NotDeleted`、`NeedsEnd`、`Done` | 1 MiB |

### frame

![ready 之後的 frame 與壓縮。a，frame：pipe 一個位元組、kind 一個位元組、長度四個位元組，接著內容，最多 64 KiB；kind 0 是 Bytes，這條 pipe 接下來的內容；1 是 Window，還回去的窗口；2 是 End，這條 pipe 寫完了，data 的帶塊數；3 是 Sync，sync，帶之前寫的位元組數；4 是 Synced，回覆對面的程式已經讀到那裡。pipe 的編號依連線種類固定。b，以 stream 壓縮的 pipe，以 manifest 為例：記錄一筆接一筆，壓成一條 zstd stream，切成好幾個 Bytes frame；記錄、zstd 的 block 與 frame 的邊界互不相干。c，data：每塊是編號、原樣或 zstd、長度與內容；一塊可以切成好幾個 frame，都在同一條 stream 上，中間可以夾別的 pipe 的 frame；塊分到每條 stream，讀的一方照編號排回。d，第一條 stream 的兩個方向：Source 往 Destination 有 manifest、data 與 control；反方向有 answers、還給 manifest 與 data 的窗口，以及 control。](../assets/protocol-frames.svg)

- **格式**：`[pipe：1 個位元組][kind：1 個位元組][長度：4 個位元組，little-endian][內容]`，內容最多 64 KiB。ready 之後，連線上只有這一種 packet。長度用 4 個位元組，雖然 64 KiB 用 2 個位元組也差一點就放得下，以後要調整 frame 的大小也不必改格式；kind 與 pipe 分開兩個欄位，比把 transport 自己的 frame 編成保留的 pipe 編號清楚。
- **kind**：
  - `0 Bytes`：這條 pipe 接下來的內容；
  - `1 Window`：還這條 pipe 的窗口，內容是還的位元組數，u32；
  - `2 End`：這條 pipe 寫完了，沒有內容；data 的帶塊數，u64；
  - `3 Sync`：sync，一個編號，u32，以及 sync 之前寫進這條 pipe 的位元組數，u64；
  - `4 Synced`：讀的一方的 transport 回的，它的程式已經讀到那裡，同一個編號，u32。
- **以 stream 壓縮的 pipe**：`Bytes` 的內容是這條 pipe 的 zstd stream 的下一段；不壓時就是 pipe 的位元組。記錄、zstd 的 block 與 frame 的邊界互不相干。
- **data 的塊**：`[編號：u64][0 原樣、1 zstd：1 個位元組][長度：u32][內容]`，little-endian，標頭 13 個位元組，每塊最多是 pipe 的 2 MiB。一塊走一條 stream，切成需要的幾個 `Bytes` frame，中間可以夾別的 pipe 的 frame；一條 stream 一次只送一塊。讀的一方的 transport 照編號排回順序。
- **塊的編號從線路上來，不能信任**：重複的、超出 data 的 `End` 所說塊數的、比還在等的那一塊超前得多過 data 的窗口裝得下的（窗口的位元組數除以一塊最少占的位元組數），都是協定錯誤；算下一個位置時也不能溢位。以前 block 的編號就出過這個問題：編號 `u64::MAX` 讓 nightly fuzz 崩潰（2026-10-27，`fb6e6ec` 修好）。
- **走哪條 stream**：data 的塊分到每條；其他 pipe 的 frame，以及所有的 `Window`、`Sync`、`Synced`，都走第一條。`Window` 與 `Synced` 走它那條 pipe 的反方向。
- **sync**：寫的一方呼叫 `sync()`，transport 送 `Sync`；讀的一方的程式讀到那個位元組數時，它的 transport 回 `Synced`，和還窗口一樣在讀走時做；收到 `Synced`，`sync()` 返回。
- **順序**：第一條 stream 的 pump 最先送 control、`Window` 與 `Synced`；其次是 answers 或 reports；再來在 data 與 manifest 之間，data 的送出 buffer 至少四分之一滿時 data 先，否則 manifest 先。

### 壓縮

- 以連線為單位設定，見[準備時帶的設定](#準備時帶的設定)：跨網路的每條 pipe 都壓，沒有例外；本機的不壓，除非指定 `--compress-level`。
- **以 stream 壓縮的 pipe**：manifest、answers、reports、control，每條一個 zstd stream，寫的一方一個壓縮狀態，讀的一方一個解壓狀態。
- **data**：切成 2 MiB 的塊，由 8 條 thread 各自單獨壓縮，解壓也一樣；壓了不會變小的塊原樣送；不滿一塊的，就是輪到 data 時有多少算多少。
- **flush**：不等。壓縮由 pump 往回拉：輪到一條 pipe、下游有空間時，才把它送出 buffer 裡的全部拿去壓，最多一個 frame 的量，壓完就 flush。下游慢時自然攢得多、壓得好；不慢時有多少送多少。慢速鏈路上的中型傳輸會多送一些；見[慢速鏈路上，pipe 攢一下再送](future.zh-TW.md#慢速鏈路上pipe-攢一下再送)。
- 完整性的檢查不在這裡：檔案的 BLAKE3 雜湊、分段的 chaining value，由 peer 計算與驗證。

### 窗口

- 有窗口的是 manifest、data、answers、reports；control 沒有：寫的一方從不等讀的一方，讀的一方照收。control 上還沒讀走的超過 16 MiB，就是協定錯誤，正常時碰不到。
- 窗口由讀的一方給，單位是線路上的位元組，也就是 frame 的內容，壓縮之後的。
- 寫的一方用完窗口就等；讀的一方的程式讀走多少，transport 就還多少，攢到窗口的四分之一再送 `Window`。`sync()` 也一樣：對面不讀，它就等。
- 接收的 thread 從不等程式：對方不會超過窗口，接收 buffer 一定放得下。
- 窗口：data 64 MiB、manifest 2 MiB、answers 1 MiB、reports 1 MiB。送出 buffer 和窗口一樣大，但 data 的是 16 MiB。

### 結束與失敗

- 寫的一方寫完一條 pipe 就關閉，transport 送出它的 `End`；讀的一方讀到 `End`，這條 pipe 才完整。
- 一條連線上，兩個方向的每條 pipe 都結束了，transport 才關掉連線。
- 連線讀到結尾時，對方寫的 pipe 還有沒結束的，就是斷了：傳輸失敗，不重連。K 大於 1 時，第一條之後的 stream 只帶 data 的塊：斷在一塊的中間，或塊數少於 data 的 `End` 所說的，就是斷了。
- 對方是否還在，由 monitor 的心跳判斷；連線上來的任何東西都算聽到對方；見 [Monitor](architecture.zh-TW.md#monitor)。
- 各連線什麼時候結束：server 的 peer 做完，就說出來，等 client 結束它的 control connection；client 的 monitor 在兩個 server 都做完時結束兩者；連結在 host 上的東西都結束之後才關。
- **peer 做完，只說它自己做完了。** 收到的一方照自己的狀態判斷自己做完沒有：monitor 收到只關掉 client 這邊；worker 要等它的行程結束、而且它的連線上每條 pipe 都讀到 `End`，才算結束。所以 peer 做完走 control，和最後幾筆報告誰先到都無妨。

## 訊息

packet 是 `[type：1 個位元組][長度：4 個位元組，little-endian][payload]`，payload 是訊息的 postcard 編碼。它的類型：

| 類型 | 訊息 | 在哪裡 |
|---|---|---|
| 0 | `Control` | control connection，ready 之前與它的 control pipe 上；stream 的 control pipe |
| 5 | `Report` | control connection 的 control 與 reports pipe |
| 11 | `Monitor` | 連到 monitor 的 channel，ready 之前；連結的 control pipe |

類型 1、2、3，即 manifest、answer 與 data 的記錄，只在測試與 fuzzer 以一條 packet 串流跑 peer 時是 packet；在 pipe 上，它們是記錄。4、6 到 10、12 在 pipe 之前用過，不再使用。

**`Control`**

| 訊息 | 從、到 | 說什麼 |
|---|---|---|
| `Hello` | client → server | 交握；見下 |
| `Welcome { socket, port, root }` | server → client | 接受 `Hello`：stream 要連的 socket，或改用的 port 與它的秘密；以及它的 root 是什麼，即 kernel 的開機識別、root 的正規路徑、在不在、它與上面每個目錄的 device 與 inode，以及它代為操作的使用者的家目錄 |
| `Refused { reason }` | server → client | 拒絕 `Hello`，然後結束；每個 build 都是第三個 variant，只有原因 |
| `Proceed { guard, streams }` | client → server | 可以繼續：它絕不能進入或刪除什麼，以及有幾條 stream |
| `Error { message }` | server → client，作為回覆；peer → peer，在 stream 上 | 送的一方失敗了，停下 |
| `Snapshot` | client → server | 它的 thread 停在哪裡 |

`Control` 還有 `Done`，沒有東西送它：Destination 做完走 answers，server 做完以 report 走它的 control pipe。

`Hello` 的欄位，每個 build 都是 build 第一個：

| 欄位 | 什麼 |
|---|---|
| `build` | client 的 build，server 的必須相同 |
| `role` | `Source` 或 `Destination` |
| `root` | server 的 root，路徑的位元組 |
| `dry_run` | 只比較，什麼都不寫 |
| `modify_window` | 兩個 mtime 相差多少以內算相同 |
| `numeric_ids` | 保留 owner 的數字 ID，不依名稱對應 |
| `delete` | 刪除 Source 沒有的：`ferry sync` |
| `one_file_system` | 不進入別的檔案系統上的目錄：`-x` |
| `compress`、`compress_level` | 這條 control connection 的 pipe 壓不壓，以什麼等級 |
| `profile` | 剖析這次執行，說出時間花在哪裡 |
| `batch_entries`、`backlog_files`、`paced_bytes`、`data_credit` | 測試與量測用；0 表示預設值 |
| `listers` | 每一邊同時列出、比較幾個目錄；0 表示一個 |
| `cpus`、`place` | server 用 host 的幾個核心，0 表示全部，以及從哪裡取 |

**`Monitor`**

| 訊息 | 從、到 | 說什麼 |
|---|---|---|
| `Follow { compress }` | client → monitor，第一個 | 這條連線是連結，它的 pipe 以 `compress` 壓縮，若有 |
| `Launch { id, what }` | client → monitor，第一個 | 啟動 `what`，以這條連線當它的輸入與輸出：`Server { sudo, port }`、`SudoCheck { asks }` 或 `Fixture { arguments }` |
| `Ended { id, code, signal, said }` | monitor → client | `id` 怎麼結束，以及跑完就結束的，在標準錯誤說了什麼 |
| `NotStarted { id, reason }` | monitor → client | `id` 啟動不了 |
| `Beat` | 雙向 | 對面還在，每 5 秒 |
| `Ping { id }`、`Pong { id }` | client → monitor，回覆 | 往返時間 |
| `Stop` | client → monitor | 停下它啟動的 |
| `Paused { id }`、`Resumed { id }` | monitor → client | `id` 被暫停，或繼續了 |
| `Awaiting { ids }` | monitor → client | 被叫停之後，它還在等什麼 |
| `Look`、`Looked { id, outside }` | client → monitor，回覆 | 它啟動的從外面看起來怎樣 |

**`Report`**，server 給 client：在它的 control pipe 上，每秒的 `Metrics` 與 `Carried`，以及 `Snapshot`、`Failed` 與 `Done`；在它的 reports pipe 上，`Counts`、`Listed`、`Said`、`Queues`、`Transferring`、`Busy`、`Levels`，最後是 `Profile`。

## pipe 攜帶什麼

**已決定。** 程式寫進 pipe 的，有兩種形式：

- **packet**，在訊息的 pipe 上，即 control 與 reports，和 pipe 之前一樣：`[type][length][payload]`，類型一個位元組，長度四個位元組，little-endian。control connection 上的交握、連到 host 的 monitor 的連線要做什麼，都在 ready 之前，也是 packet。
- **記錄**，在 manifest、data 與 answers 上：一件事一筆，先是長度，最多五個位元組的 varint，再是它的 postcard 編碼，最多 2 MiB 加 64 KiB，即檔案的一段加上跟著它的。程式一次寫入有多少就寫多少，讀的一方讀到什麼就是什麼，不完整的一筆留到下一次讀。更長的、或解不開的，就是協定錯誤，傳輸當場失敗。

| pipe | 連線 | 方向 | 攜帶的內容 |
|---|---|---|---|
| control | stream | 雙向 | 錯誤，一個 packet |
| manifest | stream | Source → Destination | 記錄：根；每個目錄是 `In`，接著是它的項目，依名稱排序，ID 依序，再來是 `Listed`，沒有列的目錄則是 `Unlistable`；owner 的名稱在第一次用到之前；讀不到的路徑；結束 |
| data | stream | Source → Destination | 記錄：2 MiB 以內的整個檔案，連同它的 BLAKE3；較大檔案的一段 2 MiB，連同它的 chaining value；分段檔案的雜湊，在它的最後一段之後；需要卻打不開的檔案；data 的結束 |
| answers | stream | Destination → Source | 記錄：需要的檔案；需要的屬性；比較過的項目，從第一個到最後一個；失敗；`sync` 刪不掉的；needs 的結束；做完 |
| control | server 的 control connection | 雙向 | packet：client 說可以繼續、問 thread 停在哪裡；server 每秒的 metrics 與 stream 送收了多少、它的 thread 停在哪裡、它的失敗，以及它的 peer 做完了 |
| reports | server 的 control connection | server → client | packet：peer 回報的內容，進度計數、它的佇列多滿、正在傳的檔案、dry run 列出的路徑、警告、讀不到或複製不了的；最後是各 thread 的時間花在哪裡 |
| control | 連結 | 雙向 | packet：心跳，兩個方向每 5 秒各一次，量往返時間的 ping，停下，查看；monitor 啟動的每個程式怎麼結束、何時被暫停或繼續、它還在等什麼，以及它從外面看起來怎樣 |

每條 pipe 照它的內容流動，除了它的窗口，誰都不等誰。資料流動期間，Source 一邊掃描、一邊繼續寫 manifest，Destination 也繼續寫它的 answers。要保持順序的，走同一條 pipe：分段檔案的雜湊在它的最後一段之後、打不開的檔案，都在 data 上；Destination 的 `Done` 之前的失敗，在 answers 上。程式從對方的 pipe 結束得知對方做完了：每條 pipe 的寫的一方寫完全部之後關掉它，它的結束排在全部之後。data 以自己的記錄 `End` 結束，在 pipe 的結束之前，所以沒帶它就結束的 pipe，例如失敗的 Source 的，算是被切斷，而不是資料到齊了。manifest 也是這樣結束。
