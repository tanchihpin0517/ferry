# Ferry 架構

[English](architecture.md)

狀態：**草稿，討論中。** 每一節都標明它是已決定還是提議。目標以及完整性與中繼資料的規則，在[設計概觀](overview.zh-TW.md)中。資料在一次傳輸中如何流動，在[管線](pipeline.zh-TW.md)中。

**目錄**

- [Client 與 server](#client-與-server)
- [Source 與 Destination 兩個 peer](#source-與-destination-兩個-peer)
- [Client 在 server 之間轉送](#client-在-server-之間轉送)
- [SSH](#ssh)
- [Transport](#transport)
- [Connection](#connection)
- [在遠端 host 上安裝](#在遠端-host-上安裝)
- [Linux 上的靜態 musl build](#linux-上的靜態-musl-build)
  - [以 `getent` 查詢名稱](#以-getent-查詢名稱)
  - [處處都用 mimalloc 作為記憶體配置器](#處處都用-mimalloc-作為記憶體配置器)
  - [建置與測試 musl build](#建置與測試-musl-build)
- [一條 connection 上的 packet](#一條-connection-上的-packet)

![每次傳輸：位於中間的 ferry client 啟動兩個 ferry server，各透過一個位置的 transport 啟動，一個執行 Source peer、另一個執行 Destination peer，並在兩者之間轉送；每個 server 都在本機或某個 host 上執行](../assets/architecture-peers.svg)

## Client 與 server

**已決定。**

- **client** 是你在終端機中執行的 `ferry` 指令。它剖析引數、進行準備、啟動一次傳輸的兩個 server、在兩者之間轉送，並顯示它們回報的內容：進度、警告與最終摘要。它本身絕不是 peer。它不是背景行程：它的生命週期與指令相同。
- **server** 是 client 透過 [transport](#transport) 啟動的 `ferry server` 行程：在 host 上經由 SSH 啟動，就像 rsync 啟動其遠端那一側；或在本機上作為子行程啟動。每個 server 執行一個 peer。它不常駐、不監聽任何埠，也不在傳輸之間保留狀態。它唯一監聽的 socket 是一個私有的 Unix socket，只在一次傳輸期間存在，供它的 [path](#connection) 加入。它的生命週期與它的 [control connection](#transport) 相同。

一次傳輸只在其 client 執行期間進行，因為 client 永遠位於兩端之間。當 client 結束，或某個 server 的 control connection 中斷時，該 server 也會結束。已重新命名就位的檔案是完整的，下次執行會略過它們；未完成的檔案會從頭重送。

## Source 與 Destination 兩個 peer

**已決定。** 一次傳輸有兩個 **peer**，每一側一個。兩者執行相同的程式碼，各在自己的 server 中執行，由交握決定哪一個是 **Source**、哪一個是 **Destination**：

| 指令 | Source | Destination |
|---|---|---|
| `ferry copy ./a host:b`（push） | 本機上的 server | host 上的 server |
| `ferry copy host:b ./a`（pull） | host 上的 server | 本機上的 server |
| `ferry copy ./a ./b`（本機複製） | 本機上的 server | 本機上的 server |
| `ferry copy hostA:a hostB:b`（遠端到遠端） | hostA 上的 server | hostB 上的 server |

**已決定。** 每種組合都以相同的方式執行。對每一側，client 取得其位置的 transport，透過它啟動 server，peer 之間則經由 client 溝通。server 在哪裡執行，只決定由哪個 transport 啟動它；其他一切都相同，因此只需維護一條程式路徑。push 與 pull 仍表示資料的流向，僅此而已。

**已決定。** 由 Destination 決定要傳送什麼。比較在 Destination 上執行，如同 rsync；見[管線第 2 節](pipeline.zh-TW.md#2-比較)。

## Client 在 server 之間轉送

**已決定。** client 對每個 server 開啟一條 [connection](#transport)，把一方送來的內容轉給另一方。它只讀取每則訊息的類型：report，例如進度、dry run 列出的路徑或警告，是給 client 的，由它顯示；其他一切則原封不動地轉送，不加解碼。兩台機器不需要能互相連線，任何一台上也不會放置憑證。

代價是每則訊息都要由 client 完整接收後再送出一次。在本機的兩個 server 之間，或在本機的一個與某個 host 上的一個之間，這只是多經過一次本機管道，遠比網路或磁碟快。從遠端到遠端時，資料還會兩度穿過本機的網路鏈路。

## SSH

**已決定。**

- **Ferry 執行系統的 `ssh`**，如同 rsync 與 File Peeker，而非使用 SSH 函式庫。`~/.ssh/config`、`ProxyJump`、host 別名、ssh-agent、Kerberos、硬體金鑰、OTP 與 `known_hosts` 的運作方式，都與在 `ssh` 本身中相同。函式庫必須逐一重新實作這些功能，而 `ssh` 連得到、Ferry 卻連不到的 host，正是這樣做所要避免的失敗。
- **client 持有連線，並在你的終端機中進行驗證。** 密碼或 OTP 就在你執行 `ferry` 的地方輸入。
- **中斷的連線會先重試，再等待登入。** client 先在不提示的情況下重新連線（`BatchMode=yes`），使用金鑰驗證時會成功。若 host 需要密碼或 OTP，傳輸會暫停，client 會在你的終端機中要求登入。登入之後，client 啟動新的 server，傳輸繼續進行。未完成的檔案會從頭重送。
**提議。**

- **沒有終端機時，連線中斷會使傳輸失敗**，而不是等待一個沒有人能輸入的登入，例如在腳本或 cron job 中。重新執行會略過已完成的檔案。
- **client 會在缺少所需版本的 host 上安裝 `ferry`；** 見[在遠端 host 上安裝](#在遠端-host-上安裝)。

## Transport

![transport：client 與 server 看到的是一條 connection，一方送出訊息，另一方完整且依序收到；在其下，傳送端的 transport 把訊息切成編號的片段，把每個片段交給佇列最短的 path，每條 path 都平等，接收端的 transport 再依序把它們組回去](../assets/architecture-transport.svg)

**已決定。** client 與 server 之間的每個位元組，都經過同一個模組，即 **transport**。它的原則：

1. **對外有兩種 connection，兩者都是抽象的。** 透過 transport，元件可以開啟一條 **connection**，再以 `send` 送出訊息、以 `recv` 接收訊息；以下的原則談的就是這些。或者開啟一條 **control connection**，用於準備；見下文。無論哪一種，元件都指定遠端的程式，也就是 host 上的一個指令，並得到一條通往它的 connection。SSH 的任何細節都不會外露，transport 也完全不知道該程式是什麼：Ferry 安裝在哪裡、token 代表什麼，都由元件來說。
2. **訊息完整且依序抵達。** 沒有訊息會遺失、多出、重複或損毀，訊息依送出的順序抵達。
3. **它唯一的目標是速度。** 在此保證之內，transport 盡可能快地搬運資料。
4. **底下的每條 path 都平等。** transport 使用的 SSH connection 與 channel 全都相同，沒有各自的角色：沒有哪一條專供控制，也沒有哪一條專供資料。有幾條、每個片段走哪一條，由 transport 選擇；外部既看不到也無法選擇。除了它的 path，每條 SSH connection 還有一條 heartbeat channel，它不是 path：它不攜帶訊息，只攜帶 heartbeat；見 [Connection](#connection)。
5. **其他一切都在內部發生。** 把訊息切成片段、透過 path 送出片段，以及在遠端把它們組回去並排好順序，都是 transport 的工作。
6. **它不讀取所攜帶的內容。** 對 transport 而言，每則訊息都一樣；它分辨不出錯誤與資料。
7. **它寧可失敗，也不交付錯誤的內容。** 若無法維持保證，例如某條 path 中斷且無法恢復，`send` 與 `recv` 會失敗。訊息絕不會只交付一部分，也絕不會亂序交付。path 以結尾標記作結，使得被截斷的 path，即使斷在兩個片段之間，也能與正常關閉的 path 區分開來。
8. **傳送端會等待，記憶體有上限。** 當遠端跟不上時，`send` 會等待，而不是在記憶體中堆積訊息。
9. **開啟即準備。** `open` 回傳時，每條 path 都已就緒：已登入，server 也在執行。傳輸開始之後，transport 絕不需要使用者。
10. **外部的 connection 與內部的 path 彼此獨立。** 元件可以開啟所需數量的 connection；每一條都是普通的 connection，唯一的保證是順序與完整性。transport 在它們底下開啟多少條 path，以及如何把它們的訊息分散到這些 path 上，除了速度之外不受任何限制。通往同一個 host 的 connection 可以共用 transport 在那裡的 path，讓稍後開啟的 connection 不需要自己登入：transport 可以讓多條 connection 走同一條 SSH connection，也可以讓一條 connection 走多條。

**已決定。** 沒有訊息會優先送出。錯誤與警告和其他訊息一樣，依送出的順序抵達。在控制訊息中，只有錯誤曾因優先送出而受益：交握發生在任何資料之前，Destination 的 `Done` 無論如何都必須排在其 results 之後，而警告是在傳輸結束時才來。排在已入列資料之後的錯誤，那些資料不過幾 MiB，只會晚一瞬間抵達，而且不會改變任何事：送出它的一方反正都會停止。既然沒有優先送出的訊息，transport 便不必讀取它所攜帶的任何東西。

**已決定。** **control connection** 用於準備、維繫 server 的生命週期，並在傳輸進行時用來追蹤傳輸。它獨立於 connection 之外，而且單純。

- **通往一個程式的位元組串流。** control connection 把元件接到 host 上的一個程式，也就是它的標準輸入與輸出，以位元組的形式，很像管道。遠端的程式可能對 Ferry 一無所知，例如 Ferry 安裝前的 `sh`，因此這條 connection 攜帶的是位元組，而非訊息；需要訊息時，例如交握，由元件自己在位元組上劃分訊息。程式結束時，讀取也隨之結束；若程式以非 0 的狀態失敗結束，讀取就會失敗，如原則 7 所要求。
- **用途。** 準備期間，client 依序開啟 control connection，每一條通往它需要的程式：探測 host 的腳本、安裝程序、`ferry test` 的 fixture，最後是 server，在 server 的 control connection 上進行交握，比對 build，並得知角色、選項、path 加入它所用的 token，以及它的根目錄是什麼：根目錄的 device 與 inode，以及它上面每一層目錄的 device 與 inode。接著兩個 server 都會等待，什麼都不改動，讓 client 比對兩個根目錄：一個包含另一個時拒絕，`sync` 可能誤清空的 Destination 則先詢問；見[概觀](overview.zh-TW.md#ferry-sync-的行為)。之後 client 才告訴每個 server 可以繼續，並告訴它絕不能進入或刪除什麼；control connection 先關閉的 server 則直接結束。這一切都發生在開啟任何 connection 之前。傳輸進行時，client 透過 server 的 control connection 追蹤傳輸：server 每秒在那裡送出它的 [metrics](profiling.zh-TW.md#邊傳輸邊追蹤)，並在被問到它的 thread 停在哪裡時在那裡回答。由於與 path 分開，這些都不會排在傳輸的資料後面。
- **server 啟動後，它改走自己的一條 channel。** 在 host 上，server 的輸出與使用者的 shell 共用，也與 shell 啟動檔留在背景的程式共用，它們隨時可能往那裡寫。因此 server 回應交握之後，control connection 就改走一條像 path 一樣連到 server socket 的 channel，它與 path 一起開啟，走第一條 SSH connection。此後 server 輸出上的任何東西都讀掉丟棄；它的輸入則保持開啟，讓 server 知道 client 還在。
- **server 的生命週期與其 control connection 相同。** 它關閉時，server 就結束。
- **server 綁定在第一條 SSH connection 上。** 它的 control connection 走第一條 SSH connection，因此失去那一條就會結束 server。曾考慮讓 server 比其 control connection 活得更久，改由其 path 決定何時結束，但未採用：若 host 的 systemd-logind 設定了 `KillUserProcesses=yes`，無論如何都會隨 session 結束該 session 的行程，而且單靠這一點也救不了傳輸；見下一點。
- **任何一條 SSH connection 中斷都會使傳輸失敗**，無論是第一條還是其他任何一條。其 path 上已寫出但未抵達的片段會遺失，於是遠端發現少了一個，原則 7 便使該 connection 失敗。heartbeat 能發現其他機制都發現不了的中斷，而沒有結尾標記就結束的 path 也會使 connection 失敗；見 [Connection](#connection)。重新執行會略過已完成的檔案。曾考慮讓 path 確認收到片段，使傳送端能經由其他 path 重送中斷的 path 所遺失的內容，但為了讓 transport 保持簡單而未採用。
- **它很單純。** 它的位元組原樣傳送，既不切分，也不分散到各條 path：速度的原則不適用於它，因為它攜帶的內容很少：在傳輸之前，以及傳輸期間每秒幾百位元組。
- **傳輸中的錯誤不使用它。** 傳輸期間的錯誤與警告走 connection，與其他內容一起依序傳送。
- **它在哪裡執行由 transport 決定。** 與底下的一切一樣，由哪條 SSH connection 攜帶它是隱藏的。它可能與 path 共用一條，因此不需要自己登入，也不計入 `--ssh-connections`。
- **connection 連到一個 socket。** 開啟 connection 時，元件提供 server 等待它的地方，也就是 host 上的一個 Unix socket，以及在 `--remote-join` 時、path 改為執行以加入 server 的指令，例如 `ferry server --join <token>`；transport 開啟每條 path，server 那一端再把各條 path 組合回來。
- **server 的回覆夾在標記之間。** 在 host 上，程式經由使用者的登入 shell 執行，而 shell 的啟動檔可能往標準輸出印東西，例如在那裡發出警告的工具，也可能在背景留下之後才印東西的程式。server 在那裡只送出一則訊息，也就是交握的回覆，並把它夾在開頭標記 `\0ferry: start\0` 與結束標記 `\0ferry: end\0` 之間，以一次幾百 byte 的寫入送出，而管道會讓這樣的寫入保持完整：其他任何輸出只會出現在它之前或之後，不會插進中間。client 會略過開頭標記之前的內容，而不是當成 packet 讀取，並把 shell 印出的內容說一次，好讓人修正啟動檔；缺少結束標記的回覆表示被插入了東西，交握就失敗。輸出在開頭標記之前就結束的程式，就是沒有啟動。直接連到 socket 的 path，server 會以開頭標記回應，讓 client 知道它連上了；`--join` 行程則在啟動時寫出它。


**已決定。** transport 不只一種，而 client 不知道自己拿到的是哪一種。對每個位置，它要求該位置的 transport，且只使用這兩種 connection。

- **SSH transport** 連到 host，如本節與 [Connection](#connection) 所述。
- **本機 transport** 連到本機。control connection 以子行程啟動其程式，透過管道相連。connection 的 path 是連到 server 的 Unix socket 的連線。沒有什麼需要登入、探測或安裝：server 就是 client 自己的執行檔。
- **連到 host 的其他方式**就是另一種 transport。transport 之上的一切都不需要為它改變。

**提議。** 由 transport 選擇每條 connection 攜帶多少 channel。SSH connection 的一條 channel 只能有一定量的未確認資料，即它的 window，在 OpenSSH 中為 2 MiB（`CHAN_SES_WINDOW_DEFAULT`），因此每個往返最多只能攜帶一個 window 的資料：在 100 ms 時約為 20 MB/s。至於 TCP 連線，無論其 channel 有多少，都會受壅塞控制與封包遺失所限。有 C 條 connection、每條 K 個 channel 時，一次傳輸最多只能達到以下三者中的最小值：鏈路的容量、C 條 TCP 連線的吞吐量，以及每個往返 C × K 個 window。來自 `--ssh-connections` 的 SSH connection 處理 TCP 的限制；不需要登入的 channel 處理 window 的限制。transport 會在交握期間量測往返時間，並開啟足夠的 channel，使 window 不成為限制：短路徑一條，跨洲則幾條。待能在往返時間長的路徑上量測時再決定。

## Connection

![通往 host 的 connection：ferry client 為每條 SSH connection 執行一個 ssh master，每個 master 各有自己的 TCP 連線；第一條上走 control connection，它啟動 ferry server，以及一條 path；其他各條上各走一條 path；每條 path 都是 sshd 轉發到 server 的 Unix socket 的 channel，不經過 shell](../assets/architecture-connections.svg)

**已決定。** 這是 [transport](#transport) 使用 SSH 的方式。每條 SSH connection 一個 `ssh` master，每個 master 一條 TCP 連線。在它們之上執行 [control connection](#transport)，其中一條執行 server，以及全都平等、加入 server 的 path。每條 SSH connection 都在 server 啟動前登入。

- **預設一條 SSH connection，若你有自己的 ControlMaster 就共用它。** client 從 `ssh -G <host>` 讀取套用於該 host 的選項。若它們設定了 ControlMaster（`controlmaster` 不是 `false`，且 `controlpath` 不是 `none`），client 就不加任何 control 選項，第一條 SSH connection 共用你的 master。否則它使用自己的：`ControlMaster=auto`、位於 `/tmp/ferry-<uid>/ssh/` 下的 `ControlPath`，以及 `ControlPersist=600`。無論哪一種，每個 host 只需輸入一次登入資訊或 OTP，而在保留時間內的後續執行完全不需要。control socket 放在 `/tmp` 下，是因為在 macOS 上 Unix socket 路徑限制為 103 個位元組。沒有任何設定能關閉此行為：你自己的 ControlMaster 不需設定就會被找到。
- **`--ssh-connections N` 對一個 host 開啟 N 條 SSH connection，每一條都是獨立的 TCP 連線。** 預設為 1。N 是精確值：SSH connection 在準備期間開啟，因此傳輸開始時要嘛 N 條全在，要嘛根本不開始，如下所述。它只計算 SSH connection；每條攜帶多少 channel，以及哪一條攜帶 control connection，由 transport 選擇。名稱中帶有 SSH，是因為 transport 外部的 connection 是另一回事，也因為每一條都可能需要一次登入。一個 master 的所有 session 都走同一條 TCP 連線，因此在它上面開更多 session 並不會更快。在會掉封包的路徑上，單一 TCP 流量會被壅塞控制壓到遠低於鏈路的容量；連到一個遺失率約 0.01% 的 host 時，一條連線約達 31 MB/s，四條合計約 60 MB/s，即鏈路的上限。第一條之後的每條 SSH connection 都有 Ferry 自己的 master，`/tmp/ferry-<uid>/ssh/%C-1`、`%C-2`，依此類推，與第一條一樣在保留時間內保留。
- **先開啟 SSH connection，再啟動 server。** 如[概觀](overview.zh-TW.md#先求可靠再求速度)所要求，transport 先開啟每條 SSH connection：啟動每個 master，並等到它登入。直到那時，client 才透過 control connection 探測 host、視需要安裝 Ferry，並啟動 server，因此登入之後再沒有任何事會要求輸入。
- **SSH connection 開啟時不執行任何工作。** 對每條 SSH connection，transport 執行 `ssh -N`，它會登入並啟動 master，但不在 host 上執行任何東西，並在整次執行期間持有它。開啟 SSH connection 就只是登入；control connection 與 path 是稍後在已登入的 master 上開啟的 session，不需要各自登入。持有 master 也能讓它保持存活，當它是你自己未設 `ControlPersist` 的 ControlMaster 時，否則它會隨第一個使用它的 `ssh` 一起結束。
- **SSH connection 一次登入一條。** 第一條之後的每一條都先嘗試 `BatchMode=yes`，這在使用金鑰驗證或 master 仍存活時會成功。若失敗，client 會說明哪一條正在登入，例如 `logging in SSH connection 2 of 4 to
  host`，並讓 `ssh` 在終端機中詢問，一條 SSH connection 接著一條，使提示絕不交錯，進度列也絕不會插進提示之中。只接受同一組 TOTP 碼一次的 host，會讓每次登入都等待下一組碼。
- **若有任何 SSH connection 無法開啟，準備就會失敗。** 登入失敗、需要終端機卻沒有終端機，或在登入期間按下 Ctrl-C，都會在傳輸開始前停止執行，沒有需要清理的東西，並說明是哪條 SSH connection 失敗以及原因，例如 `ferry: fatcat: SSH
  connection 3 of 4 needs a login, and there is no terminal to type it in`。要求 N 條 SSH connection 時，Ferry 絕不會默默以較少的條數傳輸。
- **每條 path 都以相同的方式連到 server，不經過 shell。** server 執行之後，它在 host 上私有目錄中的一個 Unix socket 等待，socket 以隨機的 token 命名，兩者都經由其 control connection 得知。每條 path，遍及每條 SSH connection，包括第一條，都是 SSH 轉發到該 socket 的 channel，就像 `ssh -W <socket>` 所開啟的：它不執行任何 shell，因此 shell 或它留在背景的程式印出的任何東西，都無法插進來。不會開啟任何埠。
- **SSH 不肯轉發到 socket 時，`--remote-join` 讓 path 改為加入。** host 的 `sshd` 可以用 `AllowStreamLocalForwarding no` 禁止這件事；這時連不上的 path 會讓傳輸失敗，並說明可改用 `--remote-join`。加上它，每條 path 以及 control connection 的 channel，都改在 host 上執行 `ferry server --join <token>`，透過 socket 把自己的標準輸入與輸出交給 server。這樣的 path 經由使用者的 shell 執行，只有 shell 在它之前印出的東西會被開頭標記擋掉：shell 留在背景的程式仍可能寫進來。因此不會在未要求時自動這麼做。
- **訊息分散到各條 path 上，再依序組回。** transport 把訊息切成編號的片段，把每個片段交給佇列最短的 path，並在遠端依序把片段組回，不論它們走了哪一條 path。
- **path 以結尾標記作結。** 關閉 connection 時，傳送端在每條 path 的最後一個片段之後，寫入一個表示該 path 到此結束的標記，共八個位元組。對接收端而言，正常關閉的 path 與被截斷的 path，例如其 SSH connection 中斷時，都只是結束了；而斷在兩個片段之間的 path，在等待它遺失的片段時，看起來不會有任何異狀。沒有標記就結束的 path 會使 connection 失敗。
- **每條 SSH connection 在其 path 旁攜帶一條 heartbeat channel。** 它像 path 一樣連到 server，並表明自己是用於 heartbeat。雙方每 5 秒在每條 heartbeat channel 上送出一個 heartbeat，從 connection 開啟時就開始，另一側仍在準備時也送。若某條 heartbeat channel 30 秒內沒有收到任何東西，或在 connection 仍開啟時結束，就會使 connection 失敗，並說明是哪條 SSH connection，例如 `ferry: the Destination on fatcat: SSH connection 2 of 4: no
  heartbeat for 30 s` 或 `...: dropped`。server 則改為等待 40 秒，讓會告知你的 client 先察覺；server 自己的逾時是為了 client 已不在的情況，並會使它停止，就像其 control connection 關閉時一樣。這些時間是固定的。失敗之後，client 給每個 server 幾秒鐘結束，然後便放手不管：已經消失的 SSH connection，永遠不會告知其 session 已結束。若沒有 heartbeat，一條無聲無息中斷的 SSH connection，例如網路斷了或筆電進入睡眠時，會讓雙方一直等待，長達數小時，直到 TCP 察覺為止。heartbeat 有自己的 channel，因此絕不會排在資料後面：path 的佇列，或遠端讀取緩慢時的 SSH window，都不會擋住 heartbeat，而沉默代表 SSH connection 已消失，而不是資料在等待。它們仍與 SSH connection 共用其 TCP 連線，而那正是它們所監看的對象。第一條 SSH connection 也有一條，而不是使用保持單純的 control connection。本機 transport 也有 heartbeat，讓兩種 transport 執行相同的程式碼。

**未採用。**

- **在一個 master 上開更多 session 或轉送埠。** SSH 把一個 master 的每個 session 與每個轉送埠，都作為 channel 走在它唯一的 TCP 連線上，因此它們共用一條流量及其壅塞控制。在一個 master 上開四個 session，並不比一個快。
- **Ferry 自己的 TCP 埠。** 直接連到 server 開啟的埠，可以在不增加登入的情況下得到多條流量。但這需要 Ferry 自己的加密，以及經由 SSH 傳遞的金鑰、host 上開放的埠，還有在防火牆阻擋時退回 SSH 的機制。額外的 SSH connection 能以 SSH 本身的加密提供同樣的流量，且沒有其他需要維護的東西。

## 在遠端 host 上安裝

**已決定。** 本機上的 server 就是 client 自己的執行檔，因此不需要為它探測或安裝任何東西。在 host 上：

- **client 與 server 執行完全相同的 build。** 協定不需要與舊版本相容，各版本之間可以自由變更。交握仍會比對 build，遇到不符時以清楚的錯誤拒絕，而不是默默執行錯誤的那一個。
- **build 並存安裝。** 每個 build 在 host 上都有自己的目錄，client 執行與自己相符的那一個。兩個不同版本的 client，例如在筆電與桌機上，可以連到同一個 host 而不會互相覆蓋。由於 server 不常駐，永遠不會有正在執行的 server 需要替換。
- **只有 release 工作流程會建置 release build。** 它在編譯時設定 `FERRY_RELEASE`。其他 build 都是 dev build，即使是以 `cargo build --release` 最佳化的也一樣，因為它的原始碼可能與同版本的 release 不同。
- **release build 安裝執行檔。** client 以 `curl` 從 GitHub Releases 下載與自己版本相同、符合 host 作業系統與架構的 release，以 `SHA256SUMS` 檢查，在本機快取，再經由 SSH connection 上傳。host 不需要對外的網路連線，而叢集的計算節點常常沒有。Linux 執行檔是[靜態 musl build](#linux-上的靜態-musl-build)，因此能在任何發行版的 C 函式庫上執行。host 先檢查上傳的檔案，才以改名把它放到定位：檢查大小，host 有 `sha256sum` 或 `shasum` 時也檢查 SHA-256；接著執行檔必須能在那裡執行，並以 `ferry build-id` 表明自己就是要安裝的 build。因此被 Ctrl-C 或斷線截斷的上傳不會安裝任何東西，無法在那裡執行的執行檔，例如家目錄以 `noexec` 掛載時，在安裝時就會發現。每次安裝都寫入自己的檔案，因此兩個同時安裝的 client 不會混到彼此的上傳。
- **dev build 從原始碼安裝**，如同 File Peeker。dev build 沒有可下載的 release，因此 client 上傳編譯它所用的原始碼，由 host 建置。build 以該原始碼的雜湊識別。host 需要 Rust 工具鏈。建出的執行檔要表明自己就是那個 build 才會安裝：沒有完整送達的原始碼會建出別的 build。同一個 host 上一次只有一個 dev 安裝在建置，因為它們共用原始碼與 target 目錄；其他的會等待。
- **client 在 host 上首次安裝前會先詢問。** 之後在同一個 host 上安裝較新的版本時不會詢問。沒有終端機時，client 只在指定 `--yes` 時安裝。
- **Ferry 絕不碰 host 的 `PATH`。** 它執行自己安裝的 build，即使 `PATH` 上有另一個 `ferry`。
- **舊的 build 會被移除。** host 保留最近使用的三個 build。每次使用一個 build，以及每次安裝，都會以修改時間把它的目錄標記為已使用，並移除超過三個的其餘 build。
- **release 由 CI 建置。** release 工作流程在 tag `v<version>` 上執行，該 tag 必須是 `Cargo.toml` 中的版本；它在與各平台架構相同的 runner 上建置並測試每個平台：Linux（靜態 musl）與 macOS，各有 x86_64 與 aarch64。它把每個執行檔以未壓縮的 `ferry-<version>-<target>` 發布，並附上它們的 `SHA256SUMS`，以及一個寫入了版本號的 `install.sh`，它會下載、檢查並安裝符合所在機器的執行檔。`curl -fsSL <repository>/releases/latest/download/install.sh | sh` 執行的就是最新 release 的這個腳本。

**提議。**

- **build 放在哪裡。** build 安裝於 `~/.ferry/hosts/<machine-id>/bin/<build>/ferry`。release build 的 `<build>` 是其版本，dev build 的則是 `dev-<source hash>`。machine ID 用來區隔共用同一個家目錄、但架構或 C 函式庫可能不同的叢集節點。dev build 改用 `~/.ferry-dev/`，因此永遠不會碰到 release 的安裝，如同 File Peeker。
- **dev build 如何上傳與建置。** 它上傳 `git ls-files --cached --others --exclude-standard` 列出的檔案，因此未提交的修改也會一併傳過去。host 以 `cargo install --locked` 建置到一個保留的 target 目錄，讓重新建置可以增量進行。
- **`--remote-ferry <path>`** 改為執行指定的執行檔，類似 rsync 的 `--rsync-path`。它是給家目錄不可寫入或沒有空間的 host 使用的。

## Linux 上的靜態 musl build

**已決定。** Linux 的 release 是靜態 musl build，因此一個執行檔就能在任何發行版的 C 函式庫上執行，包括 RHEL 衍生版的舊 glibc。musl 與 glibc 有些差異會影響設計，彙整於此。

### 以 `getent` 查詢名稱

musl 只從 `/etc/passwd` 與 `/etc/group` 讀取使用者與群組。它不支援 NSS，因此看不到 LDAP 或 SSSD 中的帳號，而大多數叢集正是把帳號放在那裡。為了在那裡依名稱對應擁有者與群組，Linux build 透過執行系統的 `getent` 來查詢名稱與 id，它使用系統自己的 C 函式庫，因此看得到 NSS 看得到的一切。

- **查詢會批次進行並快取。** `getent passwd` 與 `getent group` 一次呼叫可接受多個鍵，而每個 id 或名稱每次執行只查詢一次。一側只有在一批項目帶來它尚未見過的 id 或名稱時，才會執行 `getent`，因此行程數量隨不同的擁有者與群組數增長，而非隨檔案數增長：一棵有數百萬個檔案的目錄樹，只需少數幾個行程，而非數百萬個。
- **沒有 `getent` 時，Ferry 會退回**透過 musl 讀取 `/etc/passwd` 與 `/etc/group`，再退回數字 id，並警告一次。
- **全由數字組成的名稱不做對應。** `getent` 會把這樣的鍵當成 id，因此找到的是具有該 id 的帳號，或者找不到，而不是具有該名稱的帳號。該項目保留其數字 id，就像名稱未知時一樣。
- **macOS 完全不需要這些。** 它的 build 使用系統的 C 函式庫，該函式庫本身就看得到目錄服務。

除了 `ssh`，Ferry 只會執行 `getent`（而且只在 Linux 上），以及在 client 上為了下載 release 而執行的 `curl`。

### 處處都用 mimalloc 作為記憶體配置器

每個 build 都以 mimalloc 作為全域記憶體配置器，無論是 Linux 上的 musl 與 glibc，還是 macOS。[管線](pipeline.zh-TW.md)的每個 stage 都會從許多執行緒同時配置與釋放緩衝區。musl 自己的配置器偏重強化安全性與低記憶體用量，而非這一點：它的執行緒大多在等待同一把共用鎖。mimalloc 為每個執行緒保留各自的可用記憶體，因此執行緒很少互相競爭。處處都使用它，能讓每個平台有相同的效能特性，且只有一條程式路徑。它的 C 程式碼與 zstd 的一樣編譯進執行檔，因此 Ferry 仍是單一檔案。

**提議。** 等 Ferry 能傳輸檔案之後，以 mimalloc 對比 jemalloc 與系統配置器進行基準測試，若數據顯示有必要，再重新檢討這個選擇。

### 建置與測試 musl build

- **C 程式碼需要 musl C 編譯器。** zstd 與 mimalloc 會把 C 程式碼編譯進執行檔，因此為 musl target 建置需要 musl 的 C 工具鏈，CI 以 `musl-tools` 安裝它。
- **CI 測試實際發布的 build。** 測試在 x86_64 與 aarch64 的 musl target 上執行，而不只在 host 的 glibc 上，使用 `make test TARGET=<target>`。

## 一條 connection 上的 packet

**已決定。** connection 是一條雙向的位元組串流，攜帶一連串有類型的 **packet**：`[type][length][payload]`。每個 packet 不論類型，都是 [transport](#transport) 送上它所選 path 的一個 piece。每種訊息都有自己的類型：

| 類型 | 方向 | 內容 |
|---|---|---|
| control | 雙向 | 交握（版本、能力、選項）、錯誤、警告 |
| report | server → client | 進度計數、管線佇列有多滿、dry run 列出的路徑、警告、無法讀取或複製的項目，供 client 顯示 |
| manifest | Source → Destination | Source 的檔案：路徑與中繼資料 |
| needs | Destination → Source | Destination 需要的檔案 |
| data | Source → Destination | 檔案 record |
| results | Destination → Source | 每個檔案的成功或失敗 |

不同類型的 packet 交錯傳送，讓同時流動的各種訊息不必互相等待。資料流動期間，Source 會隨著掃描找到檔案而持續送出 manifest packet，Destination 則持續送出 needs 與 results。檔案資料被切成最多 64 KiB 的 data packet，每個攜帶一個壓縮後 frame 的一段。每個 peer 有一個 pump 執行緒，擁有 connection 的寫入端，並從三個佇列取 packet，每個佇列依交出的順序送出。訊息最先：needs、results、report 與控制訊息，例如錯誤，以及必須排在 results 之後的 Destination `Done`。接著在 manifest 與資料之間，資料佇列至少四分之一滿時資料優先，以免 sender 的檔案被掃描產生得比 connection 送得還快的 manifest 擋住；低於這個量時 manifest 優先，讓 Destination 在 sender 送完手上的檔案之前，找出更多需要的檔案。control packet 沒有自己的佇列，也不優先送出；見 [Transport](#transport)。資料佇列有上限，因此比 connection 快的 Source 會等待，而不是在記憶體中堆積串流。位於兩個 server 之間的 client 只讀取每個 packet 的類型：report 由它顯示，其他一切則原樣轉送。
