# Ferry 架構

[English](architecture.md)

狀態：**草稿，討論中。** 每一節都標明它是已決定還是提議。目標以及完整性與中繼資料的規則，在[設計概觀](overview.zh-TW.md)中。資料在一次傳輸中如何流動，在[管線](pipeline.zh-TW.md)中。

**目錄**

- [Client 與 server](#client-與-server)
- [Source 與 Destination 兩個 peer](#source-與-destination-兩個-peer)
- [Client 在 server 之間建立 stream](#client-在-server-之間建立-stream)
- [SSH](#ssh)
- [Transport](#transport)
- [Connection](#connection)
- [在遠端 host 上安裝](#在遠端-host-上安裝)
- [Linux 上的靜態 musl build](#linux-上的靜態-musl-build)
  - [以 `getent` 查詢名稱](#以-getent-查詢名稱)
  - [處處都用 mimalloc 作為記憶體配置器](#處處都用-mimalloc-作為記憶體配置器)
  - [建置與測試 musl build](#建置與測試-musl-build)
- [經由 session 的 packet](#經由-session-的-packet)

![每次傳輸：ferry client 啟動兩個 ferry server，各透過一個位置的 transport 啟動，一個執行 Source peer、另一個執行 Destination peer，並建立兩者之間的 stream，交給它們；每個 server 都在本機或某個 host 上執行](../assets/architecture-peers.svg)

## Client 與 server

**已決定。**

- **client** 是你在終端機中執行的 `ferry` 指令。它剖析引數、進行準備、啟動一次傳輸的兩個 server、建立兩者之間的 stream，並顯示它們回報的內容：進度、警告與最終摘要。它本身絕不是 peer，也不碰一個 peer 送給另一個的任何東西。它不是背景行程：它的生命週期與指令相同。
- **server** 是 client 透過 [transport](#transport) 啟動的 `ferry server` 行程：在 host 上經由 SSH 啟動，就像 rsync 啟動其遠端那一側；或在本機上作為子行程啟動。每個 server 執行一個 peer，以及與另一個 server 之間的 [session](#transport)。它不常駐、不監聽任何埠，也不在傳輸之間保留狀態。它唯一監聽的 socket 是一個私有的 Unix socket，只在一次傳輸期間存在，供它的 [stream](#connection) 加入。它的 peer 與 session 都做完之後，它會說出來，並在 client 關閉它的 [control connection](#transport) 時結束；若 control connection 在那之前就關閉，表示 client 已不在，server 隨即停止。

一次傳輸只在其 client 執行期間進行：stream 由 client 建立，斷了的也由它重建。當 client 結束，或某個 server 的 control connection 中斷時，該 server 也會結束。已重新命名就位的檔案是完整的，下次執行會略過它們；未完成的檔案會從頭重送。

**已決定。** client 或 server 發生 panic，都是 Ferry 的 bug：該行程以 `internal error in <role>` 說明，帶上那條 thread 的角色，還原終端機、移除它的暫存檔，然後立刻以 101 結束，不留下任何等著那條 panic 的 thread 的 thread。server 會先經由 control connection 告訴 client，由 client 顯示，因為 server 自己寫出的內容會被儀表板蓋掉；之後 client 也以 101 結束。

## Source 與 Destination 兩個 peer

**已決定。** 一次傳輸有兩個 **peer**，每一側一個。兩者執行相同的程式碼，各在自己的 server 中執行，由交握決定哪一個是 **Source**、哪一個是 **Destination**：

| 指令 | Source | Destination |
|---|---|---|
| `ferry copy ./a host:b`（push） | 本機上的 server | host 上的 server |
| `ferry copy host:b ./a`（pull） | host 上的 server | 本機上的 server |
| `ferry copy ./a ./b`（本機複製） | 本機上的 server | 本機上的 server |
| `ferry copy hostA:a hostB:b`（遠端到遠端） | hostA 上的 server | hostB 上的 server |

**已決定。** 每種組合都以相同的方式執行。對每一側，client 取得其位置的 transport，透過它啟動 server，並建立兩者之間的 stream，peer 就在上面溝通。server 在哪裡執行，只決定由哪個 transport 啟動它；其他一切都相同，因此只需維護一條程式路徑。push 與 pull 仍表示資料的流向，僅此而已。

**已決定。** 由 Destination 決定要傳送什麼。比較在 Destination 上執行，如同 rsync；見[管線第 2 節](pipeline.zh-TW.md#2-比較)。

## Client 在 server 之間建立 stream

**已決定。** 兩個 server 之間的每條 stream 都由 client 建立，並把一端交給各自的 server：在本機，經由 server 的 socket；在 host 上，則是 client 某條 SSH connection 上一條 channel 的輸入與輸出。兩邊都在 host 上時，client 把兩條 channel 背對背接起來，位元組由 kernel 在兩個 `ssh` 之間搬。在 stream 上，兩個 server 的 [session](#transport) 攜帶一個 peer 送給另一個的內容；client 完全不碰。server 回報的內容，例如進度、dry run 列出的路徑或警告，走它的 [control connection](#transport)，由 client 顯示。兩台機器不需要能互相連線，任何一台上也不會放置憑證。

代價是兩邊都在 host 上時，資料仍會兩度穿過 client 那台機器的網路鏈路，因為兩條 SSH connection 都是 client 的；但資料不再經過 client 的行程，以前則是 client 完整接收每則訊息後再送出一次。在本機量到的：複製 1 GiB 時，client 的執行緒以前要用半秒的 CPU，現在不到 10 ms。

## SSH

**已決定。**

- **Ferry 執行系統的 `ssh`**，如同 rsync 與 File Peeker，而非使用 SSH 函式庫。`~/.ssh/config`、`ProxyJump`、host 別名、ssh-agent、Kerberos、硬體金鑰、OTP 與 `known_hosts` 的運作方式，都與在 `ssh` 本身中相同。函式庫必須逐一重新實作這些功能，而 `ssh` 連得到、Ferry 卻連不到的 host，正是這樣做所要避免的失敗。
- **client 持有連線，並在你的終端機中進行驗證。** 密碼或 OTP 就在你執行 `ferry` 的地方輸入。
- **斷了的 stream 會重建，它的 SSH connection 斷了就重新登入。** client 在同一條 SSH connection 上再建一條。若它的 master 已不在，先在不提示的情況下重新登入（`BatchMode=yes`），使用金鑰驗證時會成功；若 host 拒絕，需要密碼或 OTP，就在你的終端機中要求登入，儀表板這時交出終端機，這是 2026-10-24 的決定。這段期間傳輸在其他 stream 上繼續，斷掉那條上的內容由 session 重送一次：已完成的不會重送。沒有終端機時，例如在腳本或 cron job 中，不會詢問：傳輸在其他 stream 上繼續，一條都不剩時，server 在寬限時間一分鐘之後放棄。
- **第一條 SSH connection 攜帶兩個 server 的 control connection，以及它們自己的 session。** 它斷了，由它攜帶的 server 就會停止，傳輸也跟著失敗，訊息為 `the SSH connection its server ran over dropped`。讓 control connection 也由 session 管、能像 stream 一樣重新加入，在[架構重新設計](../drafts/architecture-redesign.md)中提過，尚未實作。

**提議。**

- **client 會在缺少所需版本的 host 上安裝 `ferry`；** 見[在遠端 host 上安裝](#在遠端-host-上安裝)。

## Transport

![transport：每個 server 的 peer 把 control 訊息與 block 交給它的 session；session 把 control 訊息編號後走一條 stream，把 block 切成 fragment 走要寫的最少的那條，每條 stream 都平等；另一端的 session 把每則 control 訊息依序交出一次、每個 block 交出一次，並說它收到了什麼；某條 stream 斷了，上面的內容改走另一條重送，client 則再建一條取代它](../assets/architecture-transport.svg)

**已決定。** client 與 server 之間、以及兩個 server 之間的每個位元組，都經過同一個模組，即 **transport**。它的原則：

1. **兩種 connection，兩者都是抽象的。** 透過 transport，client 開啟 **control connection**，每一條通往遠端的一個程式，用於準備與追蹤 server；見下文。它也建立兩個 server 之間的 **stream**，交給它們；在 stream 上，每個 server 的 **session** 攜帶它的 peer 送給另一個 peer 的內容：control 訊息以 `control` 送出、依序交付，block 以 `block` 送出、完整交付。SSH 的任何細節都不會外露，transport 也完全不知道程式是什麼：Ferry 安裝在哪裡、token 代表什麼，都由元件來說。
2. **每則 control 訊息恰好抵達一次、依序；每個 block 恰好一次。** 沒有訊息會遺失、多出、重複或損毀；control 訊息依送出的順序抵達，block 則不拘順序。
3. **它唯一的目標是速度。** 在此保證之內，transport 盡可能快地搬運資料。
4. **每條 stream 都平等。** stream 每條 SSH connection 一條，沒有各自的角色：control 訊息一次走一條，編號最小的那條；block 切成 fragment，走要寫的最少的那條。有幾條、每一段走哪一條，由 transport 選擇；外部既看不到也無法選擇。
5. **其他一切都在內部發生。** 編號、確認、保留送出的內容直到對方收到；把 block 切成 64 KiB 的 fragment 再組回；以及把斷掉的 stream 上的內容改走另一條重送，都是 session 的工作。
6. **它不讀取所攜帶的內容。** 對 transport 而言，每則訊息都一樣；它分辨不出錯誤與資料。
7. **只有 stream 一直接不回來時，它才失敗。** 斷了的 stream 由 client 依任一個 server 的通知重建，這段期間傳輸在其他 stream 上繼續。一條都不剩時，session 等一段寬限時間，一分鐘，讓 stream 重新加入；等不到就放棄，並告訴它的 peer 連線已失去，傳輸因此失敗。它絕不會只交付一部分，也絕不交付兩次。
8. **送出的一方不必等它，記憶體有上限。** session 照單全收：保留到對方收到的量由 peer 的協定限住，block 由資料額度，manifest 由在途的項目數。每條 stream 的寫入者手上、還沒寫出的，最多 256 KiB，所以 control 訊息最多只排在這麼多之後。
9. **開啟即準備。** 每條 SSH connection 都在 server 啟動前登入，兩個 server 都被告知可以繼續之後，才建立 stream。之後只有斷掉的 SSH connection 需要再登入。
10. **健康只在兩處判斷。** 每個 session 判斷自己的每條 stream：沉默太久的算斷；client 則建立 stream，並重建斷掉的。Ferry 的其他部分都不在連線上持有計時器，peer 只會得知 session 結束了，或連線已失去。

**已決定。** 在一條 stream 上，control 訊息排在還沒寫出的 block fragment 之前；control 訊息彼此之間保持順序，錯誤也一樣。每一側都只送出它的第一個失敗，只送一次，之後不論其他 stage 如何失敗，都不再做任何事；見 [pipeline.zh-TW.md](pipeline.zh-TW.md#原則)。transport 不讀取它所攜帶的內容來決定誰先走：control 訊息就是 control 訊息。這是[架構重新設計](../drafts/architecture-redesign.md)的 A2。

**已決定。** **control connection** 用於準備、維繫 server 的生命週期，並在傳輸進行時用來追蹤傳輸。它獨立於 stream 之外，而且單純。

- **通往一個程式的位元組串流。** control connection 把元件接到 host 上的一個程式，也就是它的標準輸入與輸出，以位元組的形式，很像管道。遠端的程式可能對 Ferry 一無所知，例如 Ferry 安裝前的 `sh`，因此這條 connection 攜帶的是位元組，而非訊息；需要訊息時，例如交握，由元件自己在位元組上劃分訊息。程式結束時，讀取也隨之結束；若程式以非 0 的狀態失敗結束，讀取就會失敗，而不是當成一切順利地結束。
- **用途。** 準備期間，client 依序開啟 control connection，每一條通往它需要的程式：探測 host 的腳本、安裝程序、`ferry test` 的 fixture，最後是 server，在 server 的 control connection 上進行交握，比對 build，並得知角色、選項、session 的 ID、stream 加入它所用的 token，以及它的根目錄是什麼：根目錄的 device 與 inode，以及它上面每一層目錄的 device 與 inode。接著兩個 server 都會等待，什麼都不改動，讓 client 比對兩個根目錄：一個包含另一個時拒絕，`sync` 可能誤清空的 Destination 則先詢問；見[概觀](overview.zh-TW.md#ferry-sync-的行為)。之後 client 才告訴每個 server 可以繼續，並告訴它絕不能進入或刪除什麼；control connection 先關閉的 server 則直接結束。這一切都發生在建立任何 stream 之前。傳輸進行時，client 透過 server 的 control connection 追蹤傳輸：server 在那裡送出它的 peer 回報的內容、每秒的 [metrics](profiling.zh-TW.md#邊傳輸邊追蹤)，以及 peer 完成後各 thread 的時間花在哪裡，並在被問到它的 thread 停在哪裡時在那裡回答；它也在那裡說哪條 stream 斷了、每條 stream 送收了多少（給儀表板），以及它的 peer 與 session 都做完了。由於與 stream 分開，這些都不會排在傳輸的資料後面。
- **server 啟動後，它改走自己的一條 channel。** 在 host 上，server 的輸出與使用者的 shell 共用，也與 shell 啟動檔留在背景的程式共用，它們隨時可能往那裡寫。因此 server 被告知可以繼續之後，control connection 就改走一條像 stream 一樣連到 server socket 的 channel，走第一條 SSH connection。此後 server 輸出上的任何東西都讀掉丟棄；它的輸入則保持開啟，讓 server 知道 client 還在。
- **client 關閉 control connection 時，server 才結束。** 它的 peer 與 session 都做完之後，它會說出來，並留下來，讓它的 session 回應之後才加入的 stream，以防對方沒收到它的結束；兩個 server 都做完，client 才關閉兩條 control connection。若在那之前就關閉，表示 client 已不在，server 隨即停止。一個 server 沒說做完就不在了，client 立刻讓另一個停止：它的 session 只會等滿寬限時間。
- **server 綁定在第一條 SSH connection 上。** 它的 control connection，以及它執行所在的 session，都走第一條 SSH connection，因此失去那一條就會結束 server，傳輸也跟著失敗。讓 control connection 由 session 管、像 stream 一樣重新加入，在[架構重新設計](../drafts/architecture-redesign.md)中提過，尚未實作。
- **其他任何一條 SSH connection 中斷都不會失敗。** 它上面的 stream 會重建，連線也先重新登入，session 重送上面的內容；見 [SSH](#ssh) 與 [Connection](#connection)。
- **它很單純。** 它的位元組原樣傳送，既不編號也不重送：速度的原則不適用於它，因為它攜帶的內容很少：在傳輸之前，以及傳輸期間每秒幾百位元組。
- **peer 的失敗也走它。** peer 失敗時，server 在這裡說出來，client 以任一個 server 第一個說的為傳輸的原因；另一個 peer 則從它的 session 得知，那是一則依序到達的 control 訊息。
- **它在哪裡執行由 transport 決定。** 與底下的一切一樣，由哪條 SSH connection 攜帶它是隱藏的。它與一條 stream 共用第一條，因此不需要自己登入，也不計入 `--ssh-connections`。
- **stream 連到一個 socket。** 每條 stream，和 control connection 的 channel 一樣，連到 server 的 socket：直接連，即 SSH 轉發到它的 channel；在 `--remote-join` 時，在那裡執行 `ferry server --join <token>`；或在本機，經由它交出去。server 先讀到的東西說明它是什麼：一條 stream 與它的編號（兩個 server 相同），或是 control connection 的 channel。
- **server 的回覆夾在標記之間。** 在 host 上，程式經由使用者的登入 shell 執行，而 shell 的啟動檔可能往標準輸出印東西，例如在那裡發出警告的工具，也可能在背景留下之後才印東西的程式。server 在那裡只送出一則訊息，也就是交握的回覆，並把它夾在開頭標記 `\0ferry: start\0` 與結束標記 `\0ferry: end\0` 之間，以一次幾百 byte 的寫入送出，而管道會讓這樣的寫入保持完整：其他任何輸出只會出現在它之前或之後，不會插進中間。client 會略過開頭標記之前的內容，而不是當成 packet 讀取，並把 shell 印出的內容說一次，好讓人修正啟動檔；缺少結束標記的回覆表示被插入了東西，交握就失敗。輸出在開頭標記之前就結束的程式，就是沒有啟動。直接連到 socket 的 stream 或 channel，server 會以開頭標記回應；`--join` 行程則在啟動時、在 shell 印的東西之後寫出它；另一邊的 server 在本機時，由 client 寫，因為那個 server 在它的 session 之前什麼都不寫。所以每條 stream 每個方向正好一個開頭標記，讀它的 server 先跳到那裡。


**已決定。** transport 不只一種，而 client 不知道自己拿到的是哪一種。對每個位置，它要求該位置的 transport，且只使用這兩種 connection。

- **SSH transport** 連到 host，如本節與 [Connection](#connection) 所述。
- **本機 transport** 連到本機。control connection 以子行程啟動其程式，透過管道相連。它的 stream 是 socket pair，兩端由 client 經由 server 的 socket 交給它們。沒有什麼需要登入、探測或安裝：server 就是 client 自己的執行檔。
- **連到 host 的其他方式**就是另一種 transport。transport 之上的一切都不需要為它改變。

**提議。** 由 transport 選擇每條 SSH connection 攜帶多少條 stream，每條一個 channel。SSH connection 的一條 channel 只能有一定量的未確認資料，即它的 window，在 OpenSSH 中為 2 MiB（`CHAN_SES_WINDOW_DEFAULT`），因此每個往返最多只能攜帶一個 window 的資料：在 100 ms 時約為 20 MB/s。至於 TCP 連線，無論其 channel 有多少，都會受壅塞控制與封包遺失所限。有 C 條 connection、每條 K 個 channel 時，一次傳輸最多只能達到以下三者中的最小值：鏈路的容量、C 條 TCP 連線的吞吐量，以及每個往返 C × K 個 window。來自 `--ssh-connections` 的 SSH connection 處理 TCP 的限制；不需要登入的 channel 處理 window 的限制。transport 會在交握期間量測往返時間，並開啟足夠的 channel，使 window 不成為限制：短路徑一條，跨洲則幾條。待能在往返時間長的路徑上量測時再決定。

## Connection

![通往 host 的 connection：ferry client 為每條 SSH connection 執行一個 ssh master，每個 master 各有自己的 TCP 連線；第一條上走 control connection，它們啟動 ferry server，server 啟動後它自己的 control connection，以及一條 stream；其他各條上各走一條 stream；每條 stream 都是 sshd 轉發到 server 的 Unix socket 的 channel，不經過 shell，另一端由 client 交給另一個 server](../assets/architecture-connections.svg)

**已決定。** 這是 [transport](#transport) 使用 SSH 的方式。每條 SSH connection 一個 `ssh` master，每個 master 一條 TCP 連線。在它們之上執行 [control connection](#transport)，其中一條執行 server，以及每條一個、全都平等、加入 server 的 stream。每條 SSH connection 都在 server 啟動前登入。

- **預設一條 SSH connection，若你有自己的 ControlMaster 就共用它。** client 從 `ssh -G <host>` 讀取套用於該 host 的選項。若它們設定了 ControlMaster（`controlmaster` 不是 `false`，且 `controlpath` 不是 `none`），client 就不加任何 control 選項，第一條 SSH connection 共用你的 master。否則它使用自己的：`ControlMaster=auto`、位於 `/tmp/ferry-<uid>/ssh/` 下的 `ControlPath`，以及 `ControlPersist=600`，加上 `ServerAliveInterval=15` 與 `ServerAliveCountMax=4`，讓 host 約一分鐘沒有回應時，master 連同其上的每個 session 一起結束：準備階段，例如上傳，還沒有 session 看著它，否則會在已經消失的連線上一直等到 TCP 放棄，長達數小時。你自己的 master 保留你給它的選項。無論哪一種，每個 host 只需輸入一次登入資訊或 OTP，而在保留時間內的後續執行完全不需要。control socket 放在 `/tmp` 下，是因為在 macOS 上 Unix socket 路徑限制為 103 個位元組。沒有任何設定能關閉此行為：你自己的 ControlMaster 不需設定就會被找到。Ferry 執行的每個 `ssh` 都另外加上 `RemoteCommand=none`、`ClearAllForwardings=yes`、`PermitLocalCommand=no` 與 `ForwardX11=no`，因為它的 session 執行自己的指令，也不需要轉發；`ssh -G` 沒有顯示你自己的 `ConnectTimeout` 時，再加上 `ConnectTimeout=10`。
- **`--ssh-connections N` 對一個 host 開啟 N 條 SSH connection，每一條都是獨立的 TCP 連線。** 預設為 1。N 是精確值：SSH connection 在準備期間開啟，因此傳輸開始時要嘛 N 條全在，要嘛根本不開始，如下所述。它只計算 SSH connection；每條攜帶多少 channel，以及哪一條攜帶 control connection，由 transport 選擇。名稱中帶有 SSH，是因為 transport 外部的 connection 是另一回事，也因為每一條都可能需要一次登入。一個 master 的所有 session 都走同一條 TCP 連線，因此在它上面開更多 session 並不會更快。在會掉封包的路徑上，單一 TCP 流量會被壅塞控制壓到遠低於鏈路的容量；連到一個遺失率約 0.01% 的 host 時，一條連線約達 31 MB/s，四條合計約 60 MB/s，即鏈路的上限。第一條之後的每條 SSH connection 都有 Ferry 自己的 master，`/tmp/ferry-<uid>/ssh/%C-1`、`%C-2`，依此類推，與第一條一樣在保留時間內保留。
- **先開啟 SSH connection，再啟動 server。** 如[概觀](overview.zh-TW.md#先求可靠再求速度)所要求，transport 先開啟每條 SSH connection：啟動每個 master，並等到它登入。直到那時，client 才透過 control connection 探測 host、視需要安裝 Ferry，並啟動 server，因此登入之後再沒有任何事會要求輸入。
- **SSH connection 開啟時不執行任何工作。** 對每條 SSH connection，transport 執行 `ssh -N`，它會登入並啟動 master，但不在 host 上執行任何東西，並在整次執行期間持有它。開啟 SSH connection 就只是登入；control connection 與 stream 是稍後在已登入的 master 上開啟的 session，不需要各自登入。持有 master 也能讓它保持存活，當它是你自己未設 `ControlPersist` 的 ControlMaster 時，否則它會隨第一個使用它的 `ssh` 一起結束。`ssh -N` 一定隨 Ferry 結束，不論 Ferry 怎麼結束，`SIGTERM` 與 `SIGHUP` 也一樣，Ferry 會在結束前先結束它；只有 `SIGKILL` 會留下它，因為那時什麼都不會執行。
- **SSH connection 一次登入一條。** 第一條之後的每一條都先嘗試 `BatchMode=yes`，這在使用金鑰驗證或 master 仍存活時會成功。若失敗，client 會說明哪一條正在登入，例如 `logging in SSH connection 2 of 4 to
  host`，並讓 `ssh` 在終端機中詢問，一條 SSH connection 接著一條，使提示絕不交錯，進度列也絕不會插進提示之中。只接受同一組 TOTP 碼一次的 host，會讓每次登入都等待下一組碼。
- **若有任何 SSH connection 無法開啟，準備就會失敗。** 登入失敗、需要終端機卻沒有終端機，或在登入期間按下 Ctrl-C，都會在傳輸開始前停止執行，沒有需要清理的東西，並說明是哪條 SSH connection 失敗以及原因，例如 `ferry: fatcat: SSH
  connection 3 of 4 needs a login, and there is no terminal to type it in`。要求 N 條 SSH connection 時，Ferry 絕不會默默以較少的條數傳輸。傳輸開始之後，斷掉的那條會在重建它上面的 stream 時重新登入；見 [SSH](#ssh)。
- **每條 SSH connection 一條 stream。** 一次傳輸的 stream 數，是兩邊 SSH connection 較多的那一邊的數目，兩邊都在本機時是一條。stream 由 client 編號，兩個 server 相同：第 `n` 條在每一邊都走第 `n` 除以條數的餘數那條 SSH connection。
- **每條 stream 都以相同的方式連到它的 server，不經過 shell。** server 執行之後，它在 host 上私有目錄中的一個 Unix socket 等待，socket 以隨機的 token 命名，兩者都經由其 control connection 得知。在 host 上，stream 的一端是 SSH 轉發到該 socket 的 channel，就像 `ssh -W <socket>` 所開啟的：它不執行任何 shell，因此 shell 或它留在背景的程式印出的任何東西，都無法插進來。不會開啟任何埠。client 把每條 stream 建成一對 socket：一端是 channel 的輸入與輸出，另一端交給本機的 server，或成為另一台 host 那條 channel 的輸入與輸出，讓兩個 `ssh` 背對背接在一起。
- **每個 server 先讀到的東西由 client 寫。** 在交出任何一端之前寫進去：直接連過去的 channel 寫一個表示直連的位元組；標頭，說明它是 stream 以及它的編號；另一邊的 server 在本機時，再寫開頭標記。host 上的 server 回應直接連來的 channel 時寫開頭標記；所以每條 stream 每個方向正好一個開頭標記，讀它的 server 先跳到那裡。
- **SSH 不肯轉發到 socket 時，`--remote-join` 讓 stream 改為加入。** host 的 `sshd` 可以用 `AllowStreamLocalForwarding no` 禁止這件事；這時最先開啟的 control connection 的 channel 就連不上，準備階段失敗，並說明可改用 `--remote-join`。加上它，每條 stream 的 channel，以及 control connection 的，都改在 host 上執行 `ferry server --join <token>`，它寫出開頭標記，並透過 socket 把自己的標準輸入與輸出交給 server。這樣的 channel 經由使用者的 shell 執行，只有 shell 在開頭標記之前印出的東西會被擋掉：shell 留在背景的程式仍可能寫進來。因此不會在未要求時自動這麼做。
- **每個 session 判斷自己的每條 stream。** 結束、失敗、或 20 秒沒有任何東西到達的 stream 算斷：session 從兩個方向關掉它，讓另一端也看到它斷了，把上面的內容改走另一條重送，並告訴 client，由 client 再建一條取代它。5 秒沒有寫出任何東西的 stream，會被告知對方還在。session 永遠把每條 stream 讀乾，到達的量由 peer 的額度限住，所以慢鏈路上資料還在進來時，絕不會被當成沉默。一條都不剩時，session 等一分鐘的寬限時間讓 stream 重新加入，等不到就放棄。這三個時間是連線上僅有的計時器，此外只有 client 登入時與每個 server 結束時自己的。
- **stream 在同樣的 SSH connection 上重建。** 用下一個沒用過、而且在兩邊都走斷掉那條所走的 SSH connection 的編號，只為兩個 server 都拿到過的 stream 重建，不論幾個 server 說它斷了，都只建一次；建不成的 2 秒後再試。斷掉的 SSH connection 先重新登入；見 [SSH](#ssh)。
- **每條 SSH connection 攜帶了多少，會告訴儀表板。** 每個 server 每秒說一次它的每條 stream 雙向送收了多少、多久前收到東西；client 把每條 SSH connection 上的 stream 加起來，作為[儀表板](dashboard.zh-TW.md#顯示內容)上的速率與「heard」。

**未採用。**

- **在一個 master 上開更多 session 或轉送埠。** SSH 把一個 master 的每個 session 與每個轉送埠，都作為 channel 走在它唯一的 TCP 連線上，因此它們共用一條流量及其壅塞控制。在一個 master 上開四個 session，並不比一個快。
- **Ferry 自己的 TCP 埠。** 直接連到 server 開啟的埠，可以在不增加登入的情況下得到多條流量。但這需要 Ferry 自己的加密，以及經由 SSH 傳遞的金鑰、host 上開放的埠，還有在防火牆阻擋時退回 SSH 的機制。額外的 SSH connection 能以 SSH 本身的加密提供同樣的流量，且沒有其他需要維護的東西。

## 在遠端 host 上安裝

**已決定。** 本機上的 server 就是 client 自己的執行檔，因此不需要為它探測或安裝任何東西。在 host 上：

- **client 與 server 執行完全相同的 build。** 協定不需要與舊版本相容，各版本之間可以自由變更。交握仍會比對 build，遇到不符時以清楚的錯誤拒絕，而不是默默執行錯誤的那一個。build 在 hello 的其他內容之前就讀取，所以這是每個 build 都保持不變的唯一布局：hello 是第一種 control 訊息，build 是它的第一個欄位，另外還有拒絕訊息的布局。
- **build 並存安裝。** 每個 build 在 host 上都有自己的目錄，client 執行與自己相符的那一個。兩個不同版本的 client，例如在筆電與桌機上，可以連到同一個 host 而不會互相覆蓋。由於 server 不常駐，永遠不會有正在執行的 server 需要替換。
- **只有 release 工作流程會建置 release build。** 它在編譯時設定 `FERRY_RELEASE`。其他 build 都是 dev build，即使是以 `cargo build --release` 最佳化的也一樣，因為它的原始碼可能與同版本的 release 不同。
- **release build 安裝執行檔。** client 以 `curl` 從 GitHub Releases 下載與自己版本相同、符合 host 作業系統與架構的 release，以 `SHA256SUMS` 檢查，在本機快取，再經由 SSH connection 上傳。host 不需要對外的網路連線，而叢集的計算節點常常沒有。Linux 執行檔是[靜態 musl build](#linux-上的靜態-musl-build)，因此能在任何發行版的 C 函式庫上執行。host 先檢查上傳的檔案，才以改名把它放到定位：檢查大小，host 有 `sha256sum` 或 `shasum` 時也檢查 SHA-256；接著執行檔必須能在那裡執行，並以 `ferry build-id` 表明自己就是要安裝的 build。因此被 Ctrl-C 或斷線截斷的上傳不會安裝任何東西，無法在那裡執行的執行檔，例如家目錄以 `noexec` 掛載時，在安裝時就會發現。每次安裝都寫入自己的檔案，因此兩個同時安裝的 client 不會混到彼此的上傳。
- **dev build 從原始碼安裝**，如同 File Peeker。dev build 沒有可下載的 release，因此 client 上傳編譯它所用的原始碼，由 host 建置。build 以該原始碼的雜湊識別。host 需要 Rust 工具鏈。建出的執行檔要表明自己就是那個 build 才會安裝：沒有完整送達的原始碼會建出別的 build。同一個 host 上一次只有一個 dev 安裝在建置，因為它們共用原始碼與 target 目錄；其他的會等待。
- **client 在 host 上首次安裝前會先詢問。** 之後在同一個 host 上安裝較新的版本時不會詢問。沒有終端機時，client 只在指定 `--yes` 時安裝。
- **Ferry 絕不碰 host 的 `PATH`。** 它執行自己安裝的 build，即使 `PATH` 上有另一個 `ferry`。
- **舊的 build 會被移除。** host 保留最近使用的三個 build。每次使用一個 build，以及每次安裝，都會以修改時間把它的目錄標記為已使用，並移除超過三個的其餘 build；最近一小時內用過的除外，因為另一個 client 可能剛探測到它，正要啟動它。使用時間由 `find -mmin` 判斷；host 上沒有它時（例如某些容器映像），無從得知使用時間，就只保留三個，其餘都移除，舊的 build 才不會一直累積。
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
- **不可能是帳號的名稱也不做對應：** 空的、超過 256 bytes、含 `:`、`/`、NUL 或空白，或以 `-` 開頭的名稱。`getent` 不論這種名稱出現在哪裡，都會當成選項，例如 `-s` 會讓這一批其他所有名稱改從另一個來源查詢。
- **macOS 完全不需要這些。** 它的 build 使用系統的 C 函式庫，該函式庫本身就看得到目錄服務。

除了 `ssh`，Ferry 只會執行 `getent`（而且只在 Linux 上），以及在 client 上為了下載 release 而執行的 `curl`。

### 處處都用 mimalloc 作為記憶體配置器

每個 build 都以 mimalloc 作為全域記憶體配置器，無論是 Linux 上的 musl 與 glibc，還是 macOS。[管線](pipeline.zh-TW.md)的每個 stage 都會從許多執行緒同時配置與釋放緩衝區。musl 自己的配置器偏重強化安全性與低記憶體用量，而非這一點：它的執行緒大多在等待同一把共用鎖。mimalloc 為每個執行緒保留各自的可用記憶體，因此執行緒很少互相競爭。處處都使用它，能讓每個平台有相同的效能特性，且只有一條程式路徑。它的 C 程式碼與 zstd 的一樣編譯進執行檔，因此 Ferry 仍是單一檔案。

**提議。** 等 Ferry 能傳輸檔案之後，以 mimalloc 對比 jemalloc 與系統配置器進行基準測試，若數據顯示有必要，再重新檢討這個選擇。

### 建置與測試 musl build

- **C 程式碼需要 musl C 編譯器。** zstd 與 mimalloc 會把 C 程式碼編譯進執行檔，因此為 musl target 建置需要 musl 的 C 工具鏈，CI 以 `musl-tools` 安裝它。
- **CI 測試實際發布的 build。** 測試在 x86_64 與 aarch64 的 musl target 上執行，而不只在 host 的 glibc 上，使用 `make test TARGET=<target>`。

## 經由 session 的 packet

**已決定。** peer 讀寫一連串有類型的 **packet**：`[type][length][payload]`。它的 server 經由 [session](#transport) 攜帶它們：檔案的 block，由它的 data packet 組回完整的一個，作為 session 的 block，恰好交付一次、不拘順序；其他每個 packet 作為一則 control 訊息，恰好交付一次、依序。每種訊息都有自己的類型：

| 類型 | 方向 | 內容 |
|---|---|---|
| control | 雙向 | 交握（版本、能力、選項）、錯誤、警告 |
| report | server → client，走它的 control connection | 進度計數、管線佇列有多滿、dry run 列出的路徑、警告、無法讀取或複製的項目、哪條 stream 斷了與每條 stream 送收了多少，供 client 顯示 |
| manifest | Source → Destination | Source 的檔案：路徑與中繼資料 |
| needs | Destination → Source | Destination 需要的檔案 |
| data | Source → Destination | 檔案的 block |
| file hash | Source → Destination | 以 segment 傳送的檔案，由各段合併出的雜湊 |
| credit | Destination → Source | 資料額度：一開始給，寫完 block 後歸還 |
| results | Destination → Source | 每個檔案的成功或失敗 |
| blocks' end | Source 的 server → Destination 的 server | Source 送了幾個 block，取代原本跟在最後一個之後的結束：Destination 的 server 等那麼多個都到了，才把結束交給它的 peer |

不同類型的 packet 交錯傳送，讓同時流動的各種訊息不必互相等待。資料流動期間，Source 會隨著掃描找到檔案而持續送出 manifest packet，Destination 則持續送出 needs 與 results。每個 peer 有一個 pump 執行緒，從三個佇列取 packet，每個佇列依交出的順序送出。訊息最先：needs、results 與控制訊息，例如錯誤，以及必須排在 results 之後的 Destination `Done`。接著在 manifest 與資料之間，資料佇列至少四分之一滿時資料優先，以免 sender 的檔案被掃描產生得比 stream 送得還快的 manifest 擋住；低於這個量時 manifest 優先，讓 Destination 在 sender 送完手上的檔案之前，找出更多需要的檔案。資料佇列有上限，Source 送出的資料也受額度所限，因此比 stream 快的 Source 會等待，而不是在記憶體中堆積資料。每個 peer 由輸入結束得知對方做完了：session 說對方做完、它送的都已交到時，server 才結束 peer 的輸入。
