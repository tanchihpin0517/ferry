# Ferry 架構

[English](architecture.md)

狀態：**草稿，討論中。** 每一節都標明它是已決定還是提議。目標以及完整性與中繼資料的規則，在[設計概觀](overview.zh-TW.md)中。資料在一次傳輸中如何流動，在[管線](pipeline.zh-TW.md)中；每條連線上傳什麼，到每個位元組，在[傳輸協定](protocol.zh-TW.md)中。

**目錄**

- [Client 與 server](#client-與-server)
- [Source 與 Destination 兩個 peer](#source-與-destination-兩個-peer)
- [Client 在 server 之間建立 stream](#client-在-server-之間建立-stream)
- [SSH](#ssh)
- [Transport](#transport)
- [Connection](#connection)
- [Monitor](#monitor)
  - [使用者的操作](#使用者的操作)
  - [使用者看到的](#使用者看到的)
- [在遠端 host 上安裝](#在遠端-host-上安裝)
- [Linux 上的靜態 musl build](#linux-上的靜態-musl-build)
  - [以 `getent` 查詢名稱](#以-getent-查詢名稱)
  - [處處都用 mimalloc 作為記憶體配置器](#處處都用-mimalloc-作為記憶體配置器)
  - [建置與測試 musl build](#建置與測試-musl-build)

![每次傳輸：ferry client 啟動兩個 ferry server，各透過一個位置的 transport 啟動，一個執行 Source peer、另一個執行 Destination peer，並建立兩者之間的 stream，交給它們；每個 server 都在本機或某個 host 上執行](../assets/architecture-peers.svg)

## Client 與 server

**已決定。**

- **client** 是你在終端機中執行的 `ferry` 指令。它剖析引數、進行準備、啟動一次傳輸的兩個 server、建立兩者之間的 stream，並顯示它們回報的內容：進度、警告與最終摘要。它本身絕不是 peer，也不碰一個 peer 送給另一個的任何東西。它不是背景行程：它的生命週期與指令相同。
- **server** 是 client 透過 [transport](#transport) 啟動的 `ferry server` 行程：在 host 上經由 SSH 啟動，就像 rsync 啟動其遠端那一側，在那裡由 Ferry 的 [monitor](#monitor) 啟動；或在本機上作為子行程啟動。每個 server 執行一個 peer，經由 [stream](#transport) 與另一個 server 相連。它不常駐，也不在傳輸之間保留狀態。它唯一監聽的 socket 是一個私有的 Unix socket，只在一次傳輸期間存在，供 [stream](#connection) 連到它；SSH 不肯轉發到 socket 時，改為 loopback 上的一個 port，stream 要交出秘密才連得到。它的 peer 做完之後，它會說出來，並在 client 結束它的 [control connection](#transport) 時結束。它是它的 [monitor](#monitor)（host 的或 client 的）的 worker：control connection 在那之前就結束、收到 `SIGTERM`，或它的 monitor 不在了（由它的生命線得知），它就停止。client 在不在，由它的 monitor 判斷，不是它自己。

一次傳輸只在其 client 執行期間、而且 stream 還在時進行：stream 斷了，或某條 SSH connection 中斷，傳輸就失敗，如同 rsync，不會重建。client 結束時，不論怎麼結束，server 也會停止：在本機，因為它們的生命線斷了；在 host 上，因為那裡的 monitor 發現 client 不在了。已重新命名就位的檔案是完整的，下次執行會略過它們；未完成的檔案會從頭重送。

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

**未採用**（2026-10-23）。

- **Destination 依目錄分 shard**，每個目錄的名稱由一條執行緒寫，避免爭用它的鎖。寫成沒有名稱的檔案，只有在改名時才碰到目錄的名稱；一個決策者加上執行者就能得到同樣的平行，狀態也只有一個擁有者。
- **Source 拆成各管一部分的執行緒**：建 manifest、收 need、打包、送出各一條，每條都簡單好讀。但它們要共用的，已知的項目、stream 上持有的、大檔各段的雜湊，在它們之間就是鎖或訊息，Source 那一半也就不能成為和 Destination 那一半對接、以種子重現的狀態機。

## Client 在 server 之間建立 stream

**已決定。** 兩個 server 之間的 stream 由 client 建立，並把一端交給各自的 server：在本機，經由 server 的 socket；在 host 上，則是該 host 那條 SSH connection 上一條 channel 的輸入與輸出。兩邊都在 host 上時，client 把兩條 channel 背對背接起來，位元組由 kernel 在兩個 `ssh` 之間搬。stream 攜帶兩個 peer 之間的 [pipe](#transport)；client 完全不碰。server 回報的內容，例如進度、dry run 列出的路徑或警告，走它的 [control connection](#transport)，由 client 顯示。兩台機器不需要能互相連線，任何一台上也不會放置憑證。

代價是兩邊都在 host 上時，資料仍會兩度穿過 client 那台機器的網路鏈路，因為兩條 SSH connection 都是 client 的；但資料不再經過 client 的行程，以前則是 client 完整接收每則訊息後再送出一次。在本機量到的：複製 1 GiB 時，client 的執行緒以前要用半秒的 CPU，現在不到 10 ms。

## SSH

**已決定。**

- **Ferry 執行系統的 `ssh`**，如同 rsync 與 File Peeker，而非使用 SSH 函式庫。`~/.ssh/config`、`ProxyJump`、host 別名、ssh-agent、Kerberos、硬體金鑰、OTP 與 `known_hosts` 的運作方式，都與在 `ssh` 本身中相同。函式庫必須逐一重新實作這些功能，而 `ssh` 連得到、Ferry 卻連不到的 host，正是這樣做所要避免的失敗。
- **client 持有連線，並在你的終端機中進行驗證。** 密碼或 OTP 就在你執行 `ferry` 的地方輸入。
- **一個 host 一條 SSH connection，斷線就讓傳輸失敗。** Ferry 在一個 host 上執行的一切都走同一條 SSH connection：它唯一的一個 session，探測 host、在那裡安裝 Ferry，然後成為 Ferry 在那裡的 monitor；monitor 啟動的 server，與它們的 control connection；以及 stream。一個 host 名稱可能通往好幾台機器，例如叢集在同一個名稱後面的幾個 login node；只用一條連線，一切都會到同一台。連線中斷或 stream 斷了，傳輸就失敗，如同 rsync，訊息為 `the SSH connection its server ran over dropped`，或 stream 斷掉的原因。不會重新登入，也不會重建 stream，這是架構重新設計草稿 2026-10-25 的決定，為了排除 SSH 的不確定性；那份草稿已併入設計文件後刪除，最後一版在 `f8085a6`。已重新命名就位的檔案會留著，下次執行會略過它們。

**提議。**

- **client 會在缺少所需版本的 host 上安裝 `ferry`；** 見[在遠端 host 上安裝](#在遠端-host-上安裝)。

## Transport

![transport：連線 ready 之後攜帶 pipe，一種內容一條，單向、依序；在 stream 上，Source 寫 manifest 與 data，Destination 寫它的 answers，control 雙向；程式像寫檔一樣寫它的 pipe，pipe 的窗口用完就等，另一端讀到什麼就是什麼，讀走多少就還多少窗口；transport 把寫進來的切成 frame、排程、壓縮，data 切成塊走每條 stream，其餘走第一條；pipe 在它的結束之前就結束，或連線斷了，傳輸就失敗；對方在不在，不由 transport 判斷，而由 monitor 以心跳判斷](../assets/architecture-transport.svg)

**已決定。** client 與 server 之間、以及兩個 server 之間的每個位元組，都經過同一個模組，即 **transport**。它的原則：

1. **兩種 connection，兩者都是抽象的。** 透過 transport，client 開啟 **control connection**，每一條通往遠端的一個程式，用於準備與追蹤 server；見下文。它也建立兩個 server 之間的 **stream**，交給它們。SSH 的任何細節都不會外露，transport 也不知道程式是什麼：Ferry 安裝在哪裡、token 代表什麼，由元件自己說明。
2. **連線 ready 之後攜帶 pipe，一種內容一條。** **pipe** 單向、依序、可靠：程式有什麼就寫什麼，像寫檔一樣，另一端讀到什麼就是什麼，自己切出一筆筆記錄。一端寫出的，另一端依寫出的順序讀到：不會遺失、多出、重複或損毀。否則就是連線斷了，傳輸失敗。ready 之前，對面的程式與設定都還不確定，連線原樣攜帶它的位元組；見[開啟即準備](#transport)。一條連線有哪些 pipe，由它的種類決定，兩端又是同一個 build：stream 有 control（雙向）、manifest 與 data（Source 寫）、answers（Destination 寫）；control connection 有 control（雙向）與 reports（server 寫）；到 host 的 monitor 的連結只有 control。見 [pipe 攜帶什麼](protocol.zh-TW.md#pipe-攜帶什麼)。不攜帶 pipe 的連線，是對面不是 Ferry、還不是、或誰都能寫的：host 的第一個 session（經過登入 shell）、sudo 的檢查、`ferry test` 的 fixture，以及 server 的生命線，即它的標準錯誤。
3. **它唯一的目標是速度。** 在此保證之內，transport 盡可能快地搬運資料：途中不確認、不保留。一個 frame 放多少、什麼時候送、和什麼交錯、壓不壓縮、對方還收不收得下，都是 transport 的事，程式看不到。
4. **stream 的條數照往返時間需要。** 一個 host 只有一條 SSH connection，每條 stream 都是它上面的一條 channel；兩邊都在 host 上時，是兩邊各一條 channel 接起來；兩個 server 都在本機時，是一對 socket。一條 channel 每個往返最多送它的 window，2 MiB，所以一次傳輸對最遠的 host 的往返時間（由連到那裡 Ferry 的 monitor 的連結量）每 15 ms 一條，一到八條：K = min(8, max(1, ⌈RTT ÷ 15 ms⌉))，本機一條。第一條帶每一條 pipe；data 的 bytes 切成最多 2 MiB 的塊，每塊整個走任一條 stream，帶著編號，由讀的一方的 transport 照編號排回。塊的編號從線路上來，不信任。這些 stream 都走同一條 TCP 連線，所以一起斷，任何一條斷了，傳輸就失敗。
5. **它加的東西都在內部。** 每條 pipe 的結束（寫的一方關掉它時送出）、還回去的窗口與 sync，都是 transport 自己的 frame，由另一端的 transport 拿掉，程式看不到。寫的一方可以 `sync` 它的 pipe：對面的程式把之前寫進這條 pipe 的都讀出來之後才返回，由對面的 transport 數它讀了多少，所以代價是一個往返加上等對面讀；它和寫入一樣在 Scope 裡等。沒有人 sync 的地方什麼都不多加，Ferry 現在也還沒有地方 sync。
6. **它不讀取所攜帶的內容。** 對 transport 而言，pipe 的 bytes 不論裝什麼都一樣；它分辨不出錯誤與資料。誰先走，看的是它在哪條 pipe 上。
7. **連線斷了，它就失敗。** pipe 在它的結束到達之前就結束（因為連線結束了），或連線讀取失敗，這條 pipe 就算斷了；讀它的程式因此失敗，傳輸也跟著失敗。pipe 上的每個等待，包括寫的一方等窗口，都在一個 [Scope](#monitor) 裡，Scope 一結束就返回：連線被當成斷了、它的 host 失去連線，或傳輸被停下。沉默不會讓它斷：程式可以一直等、什麼都不說，看它的工作要多久。不會重建連線：中斷的傳輸再執行一次，做過的會略過。
8. **寫的一方只等它那條 pipe 的窗口，記憶體有上限。** control 以外的每條 pipe 都有一個**窗口**，單位是線路上的 bytes，由讀的一方給，它的程式讀走多少就還多少，每次還窗口的四分之一：寫的一方的 bytes 在 pipe 的送出 buffer 裡等，送出的速度不超過窗口。所以送來的一定放得下，讀連線的 thread 從不等任何程式。窗口：data 64 MiB，manifest 2 MiB，answers 與 reports 各 1 MiB；送出 buffer 和窗口一樣大，只有 data 是 16 MiB，少到留得在 cache 裡。control 沒有窗口：寫的一方從不等讀的一方，而且最先送；上面還沒讀走的超過 16 MiB，就是對方的錯，因為沒有東西會一次在那裡寫很多。
9. **壓縮是 transport 的事。** client 為每條連線決定它的 pipe 壓不壓、以什麼等級壓：連到 host 的以 zstd 等級 1 壓，本機的不壓，除非 `--compress-level` 給每條連線一個等級，或 `--no-compress` 都不壓；由連線的準備帶過去，即 control connection 上的交握、stream 的標頭，以及 client 在連結上先說的。data 以外的每條 pipe 是一條 zstd 串流：它的 pump 拿走等著的，最多一個 frame 的量，壓縮、flush，所以線路忙時 bytes 攢起來，不忙時立刻送出。data 的塊各自壓縮，由 8 條 thread 做；見[壓縮](pipeline.zh-TW.md#壓縮)。窗口的單位是壓縮後的 bytes。程式什麼都不壓。
10. **開啟即準備。** 每條 SSH connection 都在 server 啟動前登入，兩個 server 都被告知可以繼續之後，才建立 stream。之後不再登入。連線 ready 之後才攜帶 pipe，也就是兩端都是使用它的程式、它的設定也都知道了：在那之前的，shell 的輸出、start mark、交握、port 的秘密、連到 monitor 的 channel 要做什麼，都原樣傳送，因為讀它們的程式夾在中間，listener、monitor、`sudo`，壓縮器的狀態不能跨兩個程式。兩端各有自己的狀態，不必同時轉換：control connection 上，server 寫完回覆就 ready，client 讀到結束標記才 ready。
11. **健康只由 monitor 判斷。** 連線的另一端在不在，只在一個地方判斷，即 [monitor](#monitor)：client 的 monitor 與 host 的 monitor 之間的連結上，兩個方向每 5 秒各送一次心跳，30 秒完全沒有從對方來的任何東西，host 就算失去連線；在這條連結之前，由 host 的第一個 session 兩個方向一樣頻繁地說它還在。從對方來的任何東西都和心跳一樣算數：連結上的、server 的 control connection 上的，或 stream 上的（由讀它的 server 說），所以在慢的連線上排在資料後面的心跳，不會讓任何東西被當成斷了。host 失去連線，傳輸就失敗，那裡的 monitor 發現 client 不在了，也會停下它啟動的東西。stream 與 control connection 都沒有 keepalive，沒有人以它們的沉默判斷什麼。Ferry 的其他部分都不在連線上持有計時器。

**已決定。** 一條連線的 pipe 之中，它第一條 stream 的 pump 最先送 control，連同還回去的窗口與 sync；其次是 answers 或 reports；再來在 data 與 manifest 之間，data 的送出 buffer 至少四分之一滿時 data 先，否則 manifest 先。錯誤走 control，所以不排在任何資料後面；要保持順序的，走同一條 pipe；見 [pipe 攜帶什麼](protocol.zh-TW.md#pipe-攜帶什麼)。

**已決定。** 兩個方向都會阻塞的 pipe，可能互相等到永遠，就像兩個行程各自寫給對方、buffer 都滿了。靠三條規則避免：

1. **transport 讀的一方從不等程式。** 它把送來的放進該 pipe 的接收 buffer，那裡一定放得下，因為寫的一方不會超過窗口。
2. **窗口在程式讀走時還，不在處理完時還。** 處理得慢的程式讀得少，窗口就不還，寫的一方就等。資料因此端到端擋住 Source：Destination 讀 data 的執行緒持有它可以持有的，持有滿了就不再讀。
3. **等待不成環。** 被擋的寫的一方，只等一個從不等它下游的讀者：掃描等 manifest 的窗口，也就是比較；data sender 等 data 的窗口，也就是檢查執行緒；兩者都交給決策者，決策者等 answers 的窗口，也就是 Source 讀 answers 的執行緒，而它從不等，以沒有上限的佇列交給擁有者。control connection 上，reports 的讀者是 client，它不等任何 server。所以待送出的檔案太多時，是擁有者讓掃描等，而不是不讀 answers：它若不讀，決策者寫 answers 會等，檢查執行緒交不出檔案，data pipe 沒人讀，Source 等 data 的窗口，待送出的檔案就永遠減不下來。

被擋的寫的一方照樣能被停下：它的等待和連線上的每個等待一樣，都在 Scope 裡。每一側都只送出它的第一個失敗，只送一次，之後不論其他 stage 如何失敗，都不再做任何事；見 [pipeline.zh-TW.md](pipeline.zh-TW.md#原則)。transport 不讀取它所攜帶的內容來決定誰先走。兩個 server 自己的 session，把走好幾條 stream 的內容編號、確認，並重送斷掉的那條上的內容，是 A2 做的，2026-10-25 隨著它所服務的重連一起拿掉；見架構重新設計草稿的最後一版，在 `f8085a6`。

**已決定。** **control connection** 用於準備、維繫 server 的生命週期，並在傳輸進行時用來追蹤傳輸。它獨立於 stream 之外。

- **通往一個程式的位元組串流。** control connection 把元件接到 host 上的一個程式，也就是它的標準輸入與輸出，以位元組的形式，很像管道。遠端的程式可能對 Ferry 一無所知，例如 Ferry 安裝前的 `sh`，因此這條 connection 攜帶的是位元組，而非訊息；需要訊息時，例如交握，由元件自己在位元組上劃分訊息。程式結束時，讀取也隨之結束；若程式以非 0 的狀態失敗結束，讀取就會失敗，而不是當成一切順利地結束。連到 server，或連結上連到 host 的 monitor，在交握、或連結要做什麼說完之後 ready，此後和 stream 一樣攜帶 pipe。
- **用途。** 準備期間，client 依序開啟 control connection，每一條通往它需要的程式：在 host 上，先是它的第一個 session，一段腳本，探測 host，必要時在那裡安裝這個 build，然後成為 Ferry 在那裡的 monitor；接著經由 monitor，是 sudo 的檢查、`ferry test` 的 fixture，最後是 server，在 server 的 control connection 上進行交握，比對 build，並得知角色、選項、stream 加入它所用的 token，以及它的根目錄是什麼：根目錄的 device 與 inode，以及它上面每一層目錄的 device 與 inode。接著兩個 server 都會等待，什麼都不改動，讓 client 比對兩個根目錄：一個包含另一個時拒絕，`sync` 可能誤清空的 Destination 則先詢問；見[概觀](overview.zh-TW.md#ferry-sync-的行為)。之後 client 才告訴每個 server 可以繼續，並告訴它絕不能進入或刪除什麼；control connection 先關閉的 server 則直接結束。這一切都發生在建立 stream 之前。傳輸進行時，client 透過 server 的 control connection 追蹤傳輸：server 在那裡送出它的 peer 回報的內容、每秒的 [metrics](profiling.zh-TW.md#邊傳輸邊追蹤)，以及 peer 完成後各 thread 的時間花在哪裡，並在被問到它的 thread 停在哪裡時在那裡回答；它也每秒在那裡說 stream 送收了多少（給儀表板），以及它的 peer 做完了。由於與 stream 分開，這些都不會排在傳輸的資料後面。
- **每個 host 一個 session，其餘都走 channel。** host 的 SSH connection 上每開一個 session，都要跑使用者的登入 shell 與它的啟動檔，可能要好幾秒：nano4 負載低時 1 到 2 秒，負載高時 10 到 16 秒，而連到一個 socket 的 channel 只要約 40 ms。session 的輸出又與 shell 共用，也與啟動檔留在背景的程式共用，它們隨時可能往那裡寫。因此一個 host 只開一個 session，即它的**第一個 session**：一段腳本，探測 host，必要時在那裡安裝這個 build，然後成為 Ferry 在那裡的 **monitor**，即 `ferry monitor`；它像 server 一樣在自己的 socket 上等待，並在 session 的輸出上說出 socket 在哪裡。host 上其餘的一切，都走連到它的 channel，不跑 shell。client 以第一條追蹤 monitor，即 client 的 monitor 與這個 monitor 之間的**連結**：monitor 知道那條 channel 的用途之後，兩端就改在它唯一的 pipe，control 上收發，兩個方向每 5 秒各送一次心跳；30 秒完全沒有從對方來的任何東西（連結上或其他地方），例如 SSH connection 靜靜斷了，兩端都把對方當成不在了，不等 `sshd` 發現，那在它的預設值下要兩個多小時。在那之前，由第一個 session 兩個方向一樣頻繁地說它還在；它成為 monitor 之後，client 要在 30 秒內追蹤它。連到它的連線若一開始就什麼都沒送來，第一個位元組或第一個封包都沒有，30 秒後就放掉，以免卡住後面的連線，因為 monitor 是逐條處理的。在其餘每一條上，client 請它啟動一個程式，即一個 worker，那個程式就以那條 channel 為標準輸入與輸出：`--remote-sudo` 時 sudo 的檢查、`ferry test` 的一個 fixture，或一個 server，`--remote-sudo` 時經 `sudo`。monitor 看著每一個，在連結上說出每個怎麼結束、何時被暫停或繼續，以及跑到結束的程式（例如 `sudo`）在標準錯誤上說了什麼，讓 client 顯示；一個程式啟動之後，monitor 不再讀它 channel 上的任何東西。只開一個 session，在 session 貴的地方省最多：2026-10-26 輪流量的 1 KB 複製，到 nano4 是 1.2 秒，開兩個時是 2.0 秒。一個 session 只要 20 到 40 ms 的地方，例如 fatcat，反而慢約 0.02 秒（0.20 對 0.18），因為第一個 session 要等連結關上才結束；這已經接受。見 [Monitor](#monitor)。
- **server 的 control connection 就是啟動它的那條 channel。** 沒有 shell 寫得進去，所以 server 在那裡交握，它的 control connection 也一直留在那裡，直到它結束。在本機，server 由 client 自己啟動，經由管道相連。
- **client 結束 control connection 時，server 才結束。** 握手之後，control connection 攜帶兩條 pipe：control，雙向，client 在上面告訴 server 可以繼續、或問它的 thread 停在哪裡，server 在上面每秒說它的 metrics 與 stream 送收了多少、它的失敗，以及它的 peer 做完了；reports，server 寫，是它的 peer 回報的內容，最後是各 thread 的時間花在哪裡。peer 做完之後，server 會說出來，並等 client 結束它的 control connection；兩個 server 都做完，client 的 monitor 才結束兩條，每個 server 接著把說過的都寫出去，再結束它的 pipe，兩條 pipe 都讀到結束，client 才算聽完它。若在那之前就結束，server 就停止。一個 server 做完或失敗之後，另一個要等多久，沒有自己的時間：看得到的就等；見 [Monitor](#monitor)。
- **它的沉默不代表什麼。** 一個什麼都不說的 server，例如它的 peer 正在工作、或在等很慢的磁碟，不會被當成不在了：它的行程有人看著，在本機是 client 的 monitor，在 host 上是那裡的 monitor，由它告訴 client 它怎麼結束、何時被暫停或繼續。server 另外每秒在上面說一次 stream 送收了多少，給儀表板用。
- **server 綁定在它的 host 的那條 SSH connection 上。** 它的 control connection 與 stream 都走那條，因此失去它就會結束 server，傳輸也跟著失敗。
- **它的 pipe 和 stream 的一樣壓縮。** 它平常攜帶的很少，每秒幾百位元組；但 dry run 的路徑或大量的警告很多，而且很好壓。到 fatcat，和 pipe 之前原樣傳送 control connection 的 build 比：dry run 列出 42,359 條路徑，在那裡占 1.9 MB，之前 3.7 MB；1 萬個讀不到的檔案 0.3 MB，之前 0.9 MB。
- **dry run 的路徑一條一則報告。** 每條比成批送時，多花 Destination 約 1 µs，大半在它的決策者，在 pipe 之前：100 萬個空檔的 dry run，在這台機器 1.66 秒，pipe 之前 0.83 秒；在 nano4 2.5 到 3.5 秒，之前 1.0 秒。
- **peer 的失敗也走它。** peer 失敗時，server 在這裡說出來，client 以任一個 server 第一個說的為傳輸的原因；另一個 peer 則從 stream 的 control pipe 得知，它排在一切之前。
- **它在哪裡執行由 transport 決定。** 與底下的一切一樣，這是隱藏的。它與 stream 共用該 host 的那條 SSH connection，因此不需要自己登入。
- **stream 連到一個 socket。** stream 和每條連到 monitor 的 channel 一樣，連到 server 的 socket：直接連，即 SSH 轉發到它的 channel；SSH 不肯轉發到 socket 時，改連到 loopback 上的一個 port，先交出秘密；在 `--remote-join` 時，在那裡執行 `ferry server --join <token>`；或在本機，經由它交出去；見 [Connection](#connection)。到 server socket 的只有 stream，它先讀到的東西會說明這一點，即它是哪一條、它的 pipe 以什麼等級壓縮；到 monitor socket 的，先到的東西說明這條連線要做什麼。
- **Ferry 在 session 上說的話放在加了標記的行裡，server 的回覆夾在標記之間。** 在 host 上，第一個 session 經由使用者的登入 shell 執行，而 shell 的啟動檔可能往標準輸出印東西，例如在那裡發出警告的工具，也可能在背景留下之後才印東西的程式。因此 session 的腳本、以及之後的 monitor，在那裡說的每一行，都是 `\0ferry:`、一個標籤、這行的種類、內容的長度，各接一個 `:`，然後是內容與換行，以一次寫入送出，管道會讓它在別人的寫入之間保持完整。標籤是 client 自己的，每次執行都重新產生，所以碰巧印出的東西不會被當成一行；長度則能發現被插進一行裡的東西；每一段，例如探測的那段，最後一行會數出前面有幾行，掉了一行就看得出來。client 只讀這些行，並把 shell 印的其他東西說一次，好讓人修正啟動檔。server 在標準輸出上只送出一則訊息，也就是交握的回覆，並把它夾在開頭標記 `\0ferry: start\0` 與結束標記 `\0ferry: end\0` 之間，以一次幾百 byte 的寫入送出：monitor 的 channel 上沒有別的東西，但經過 shell 加入的那種，可能帶著 shell 留在背景的程式印的東西。client 會略過開頭標記之前的內容，而不是當成回覆讀取；缺少結束標記的回覆表示被插入了東西，交握就失敗。輸出在開頭標記之前就結束的程式，就是沒有啟動。在 socket 上等待的 monitor 或 server，會以開頭標記回應直接連來、或經 port 連來的 channel；`--join` 行程則在啟動時、在 shell 印的東西之後寫出它；另一邊的 server 在本機時，由 client 寫，因為那個 server 在它的 pipe 之前什麼都不寫。所以 stream 每個方向正好一個開頭標記，讀它的 server 先跳到那裡。


**已決定。** transport 不只一種，而 client 不知道自己拿到的是哪一種。對每個位置，它要求該位置的 transport，且只使用這兩種 connection。

- **SSH transport** 連到 host，如本節與 [Connection](#connection) 所述。
- **本機 transport** 連到本機。control connection 以子行程啟動其程式，透過管道相連。它的 stream 是 socket pair，兩端由 client 經由 server 的 socket 交給它們，每一端都握到它的 server 說收到了才放：在 macOS 上，傳送途中、要給一條還沒被 accept 的連線、又沒有行程握著的 socket，會被當成垃圾清空。沒有什麼需要登入、探測或安裝：server 就是 client 自己的執行檔。
- **連到 host 的其他方式**就是另一種 transport。transport 之上的一切都不需要為它改變。

**未採用**（2026-10-28，與 pipe 一起）。

- **transport 把大訊息切成封包**，一個目錄一則訊息。讀的一方要收齊一則才能解碼，記憶體一次衝高，Destination 也要等整個目錄到齊才開始比較：比 peer 切的段還糟。
- **保留段，只讓 stat 執行緒從整個目錄認領名稱。** 只解決 stat 怎麼分；訊息大小、needs、Source 記著的項目與流量控制，仍綁在段上。
- **每個項目一則訊息，不打包。** 每則都付封包與交接的成本；打包本來就是 transport 的事。
- **資料由 peer 壓縮，只有 manifest 由 transport 壓**，pipes 草稿的第一版。transport 裡的壓縮串流只有一條執行緒，會卡住資料；但這只說明 transport 的壓縮要能平行。block 各自壓縮是為了單獨重送，已隨重連拿掉，以及照順序排回，這由 pipe 保證。
- **交握放進 control pipe，build 改由 server 的啟動參數比對。** 為的是交握也能壓縮，但它壓了只會變大；交握又帶著 pipe 自己的設定，必須先到。
- **每種內容一條 SSH channel，用 OpenSSH 的 window。** 本機的 socket pair 沒有；channel 的 window 固定 2 MiB，往返時間長時不夠；兩台 host 之間的 stream 是兩條 channel 接起來的，window 不是端到端。
- **壓縮之前，把一條 pipe 的 bytes 攢一段時間，例如 50 ms。** 快速連線上，每次 flush 之後都要等，什麼都換不到；慢速連線上壓得較好；見[壓縮](pipeline.zh-TW.md#壓縮)。
- **gRPC、HTTP/2、QUIC。** RPC 框架帶著自己的分框與 async runtime，而 Ferry 的訊息自己分框，執行緒是會阻塞的。QUIC 要每個 host 開一個 UDP 的 port，但連線一律由 client 建立；它也需要 async runtime，UDP 比 TCP 更常被擋，一條連線的封包在使用者空間處理，CPU 成本高。

**已決定**（2026-10-26）。SSH connection 的一條 channel 只能有一定量的未確認資料，即它的 window，在 OpenSSH 中為 2 MiB（`CHAN_SES_WINDOW_DEFAULT`），因此每個往返最多只能攜帶一個 window 的資料：在 120 ms 時約為 17 MB/s。至於 TCP 連線，無論其 channel 有多少，都會受壅塞控制與封包遺失所限。所以在往返時間長的路徑上，data 的塊分走同一條 SSH connection 上的幾條 channel，不需要登入，window 就不再是限制；在短的路徑上，例如到 fatcat（1.7 ms）或 nano4（6.5 ms），只有一條，因為多開在那裡並沒有比較快。心跳排在它們全部後面、同一條 TCP 連線裡，那段時間由送來的資料代替它；見 [Monitor](#monitor)。

## Connection

![通往 host 的 connection：ferry client 為一個 host 執行一個 ssh master，你自己的 ControlMaster 或 Ferry 的，只有一條 TCP 連線；在它上面走 host 唯一的一個 session，經過登入 shell，它探測 host、安裝 Ferry，然後成為 Ferry 在那裡的 monitor；sshd 轉發到 monitor socket 的 channel，不經過 shell，一條用來追蹤它，即兩個 monitor 之間的連結，其餘每一條上由 monitor 啟動一個 server，成為它的 control connection；以及這次傳輸的 stream，條數照往返時間需要，每條都是 sshd 轉發到 server socket 的 channel，另一端由 client 交給另一個 server；連線中斷，傳輸就失敗](../assets/architecture-connections.svg)

**已決定。** 這是 [transport](#transport) 使用 SSH 的方式。每個 host 一個 `ssh` master，一條 TCP 連線。在它上面走 host 唯一的一個 session，它會成為 Ferry 在那裡的 monitor；連到 monitor 的 channel，每個 server 的 [control connection](#transport) 也在其中；以及把兩個 server 接起來的 stream。SSH connection 在 host 上執行任何東西之前登入。

- **一個 host 一條 SSH connection，若你有自己的 ControlMaster 就共用它。** client 從 `ssh -G <host>` 讀取套用於該 host 的選項。若它們設定了 ControlMaster（`controlmaster` 不是 `false`，且 `controlpath` 不是 `none`），client 就不加任何 control 選項，SSH connection 就是你的 master 的。否則它使用自己的：`ControlMaster=auto`、`ControlPath=/tmp/ferry-<uid>/ssh/%C`，以及 `ControlPersist=600`，加上 `ServerAliveInterval=15` 與 `ServerAliveCountMax=4`，讓 host 約一分鐘沒有回應時，master 連同其上的一切一起結束：傳輸之前做的事，例如上傳，沒有別的東西看著，否則會在已經消失的連線上一直等到 TCP 放棄，長達數小時。你自己的 master 保留你給它的選項。無論哪一種，每個 host 只需輸入一次登入資訊或 OTP，而在保留時間內的後續執行完全不需要。control socket 放在 `/tmp` 下，是因為在 macOS 上 Unix socket 路徑限制為 103 個位元組。沒有任何設定能關閉此行為：你自己的 ControlMaster 不需設定就會被找到。Ferry 執行的每個 `ssh` 都另外加上 `RemoteCommand=none`、`ClearAllForwardings=yes`、`PermitLocalCommand=no` 與 `ForwardX11=no`，因為它的 session 執行自己的指令，也不需要轉發；`ssh -G` 沒有顯示你自己的 `ConnectTimeout` 時，再加上 `ConnectTimeout=10`。
- **一個 host 上的一切都走它唯一的 SSH connection。** 一個 host 名稱可能通往好幾台機器，例如叢集在同一個名稱後面的幾個 login node，每開一條新的 SSH connection 都可能落在另一台：走第二條連線的 stream，可能到了 server 不在的機器。一個 master，以 `%C` 依 host、使用者與埠命名，持有唯一的一條 TCP 連線，所以 Ferry 執行的一切都到同一台。開好幾條的 `--ssh-connections` 在 2026-10-25 拿掉了；還帶著它的指令列會被拒絕，以 2 結束。
- **先開啟 SSH connection，再啟動 server。** 如[概觀](overview.zh-TW.md#先求可靠再求速度)所要求，transport 先開啟 SSH connection：啟動 master，並等到它登入。直到那時，client 才在 host 唯一的 session 裡探測 host、視需要安裝 Ferry，並啟動 Ferry 的 monitor，由它啟動 server，因此登入之後再沒有任何事會要求登入。
- **SSH connection 開啟時不執行任何工作。** transport 執行 `ssh -N`，它會登入並啟動 master，但不在 host 上執行任何東西。開啟它就只是登入；第一個 session、channel 與 stream，都是稍後在已登入的 master 上開啟的，不需要各自登入。有 `ControlPersist` 時，例如 Ferry 自己的 master，`ssh -N` 接著把 master 留在背景執行，自己結束。沒有時，你自己的 ControlMaster 可能就是如此，`ssh -N` 就是 master，transport 在整次執行期間持有它，否則 master 會隨第一個使用它的 `ssh` 一起結束；它一定隨 Ferry 結束，不論 Ferry 怎麼結束，`SIGTERM` 與 `SIGHUP` 也一樣，Ferry 會在結束前先結束它；只有 `SIGKILL` 會留下它，因為那時什麼都不會執行。master 已經在時，不論是你自己的，還是 Ferry 在保留時間內留下的，就照樣使用，不執行 `ssh -N`：有 master 時，`ssh -N` 會向它要一個 session，在 host 上啟動一個登入 shell。
- **先安靜地登入，再到終端機。** client 先嘗試 `BatchMode=yes`，這在使用金鑰驗證或 master 仍存活時會成功；失敗了，再讓 `ssh` 在終端機中詢問。兩邊都在 host 上時，一個登入完才登入另一個，使提示絕不交錯，進度列也絕不會插進提示之中。
- **SSH connection 無法開啟，準備就會失敗。** 登入失敗、需要終端機卻沒有終端機，或在登入期間按下 Ctrl-C，都會在傳輸開始前停止執行，沒有需要清理的東西，並說明原因，例如 `ferry: fatcat: the SSH connection needs a login, and there is no terminal to type it in`。
- **它的 stream。** 一次傳輸有 K 條 stream，照往返時間需要，兩個 server 都被告知可以繼續、以及有幾條之後，由 client 建立：到 host 上的 server，每條是該 host 那條 SSH connection 上的一條 channel；到本機的 server，是一對 socket。每條先在標頭說出它的編號，連同它的 pipe 以什麼等級壓縮，server 收齊之後，它的 peer 才開始。
- **stream 連到它的 server，不經過 shell。** server 執行之後，它在 host 上私有目錄中的一個 Unix socket 等待，socket 以隨機的 token 命名，兩者都經由其 control connection 得知。在 host 上，stream 的一端是 SSH 轉發到該 socket 的 channel，就像 `ssh -W <socket>` 所開啟的：它不執行任何 shell，因此 shell 或它留在背景的程式印出的任何東西，都無法插進來。除非 SSH 不肯轉發到 socket，否則 server 不開任何埠；見下文。client 把 stream 建成一對 socket：一端是 channel 的輸入與輸出，另一端交給本機的 server，或成為另一台 host 那條 channel 的輸入與輸出，讓兩個 `ssh` 背對背接在一起。
- **每個 server 先讀到的東西由 client 寫。** 在交出任何一端之前寫進去：直接連過去的 channel 寫一個表示直連的位元組，連到 port 的則寫它的秘密；標頭，說明它是哪一條 stream，以及它的 pipe 以什麼等級壓縮，或不壓；另一邊的 server 在本機時，再寫開頭標記。host 上的 server 回應直接連來、或經 port 連來的 channel 時寫開頭標記；所以 stream 每個方向正好一個開頭標記，讀它的 server 先跳到那裡。
- **SSH 不肯轉發到 socket 時，改走 loopback 上的 port；什麼都不肯轉發時，用 `--remote-join`。** host 的 `sshd` 可以用 `AllowStreamLocalForwarding no` 禁止轉發到 socket，OpenSSH 6.7 以前的也做不到，但兩者都可能准許轉發到 port。因此 monitor 也在 loopback 上的一個 port 等待，並在 socket 旁一起說出它，附上 64 個十六進位數字的秘密；client 先試 socket，再試 port，第一條連得上的 channel 就決定了：monitor 關掉另一個，它啟動的 server 也只在連得上的那一種上等待。port 是 host 上任何人都連得到的，所以每條連到 port 的連線都先交出秘密，client 是經由第一個 session、或 server 的 control connection 得知的；交出別的、或 30 秒內什麼都不交的，會被放掉，同時最多等 16 條。兩者都連不上時，準備階段失敗，並說明可改用 `--remote-join`。加上它，每條連到 monitor 的 channel，以及 stream 的，都改在 host 上執行 `ferry server --join <token>`，它寫出開頭標記，並透過 socket 把自己的標準輸入與輸出交給 monitor 或 server。這樣的 channel 經由使用者的 shell 執行，只有 shell 在開頭標記之前印出的東西會被擋掉：shell 留在背景的程式仍可能寫進來。因此不會在未要求時自動這麼做，而且每條都要一個 session。加入的行程會留到它加入的對象用完它交出的東西，因為 sshd 在 session 的行程結束時會關掉 session 的輸入；所以 stream 在它的 pipe 都結束時結束，而不是等 channel 關閉，monitor 也會在被追蹤期間一直留著。
- **stream 的讀取端只判斷它的 pipe 的結束。** 上面每條 pipe 都結束之前就到了盡頭的 stream 算斷：讀取端在它是 socket 時從兩個方向關掉它，讓另一端也看到它斷了，讀它的 peer 因此失敗，傳輸也跟著失敗。stream 上沒有任何東西說對方還在，它的沉默也不會讓它斷：host 在不在，由 monitor 說，看心跳，也看 server 告訴它們的 stream 送來了什麼。心跳的 30 秒，以及第一個 session 的，是連線上僅有的計時器，此外只有 client 登入時自己的，即等一個不回應的 master 的 30 秒，還有 monitor 與 server 給連到其 socket 或 port 的連線送來第一個東西的 30 秒。
- **SSH connection 攜帶了多少，會告訴儀表板。** 每個 server 每秒說一次 stream 雙向送收了多少；client 把它顯示為該 host 那條 SSH connection 在[儀表板](dashboard.zh-TW.md#顯示內容)上的速率，並把多久前從 host 那裡收到任何東西（心跳至少每 5 秒一次）顯示為「heard」。

**已決定。** 一次傳輸用到的每一條連線，照建立的順序，每條怎麼建立、上面傳什麼，到每個位元組，寫在[傳輸協定](protocol.zh-TW.md#每一條連線)。

**未採用。**

- **對一個 host 開好幾條 SSH connection**（`--ssh-connections`，A2 做的，2026-10-25 拿掉）。在會掉封包的路徑上，單一 TCP 流量會被壅塞控制壓到遠低於鏈路的容量：連到一個遺失率約 0.01% 的 host 時，一條連線約達 31 MB/s，四條合計約 60 MB/s，即鏈路的上限。但每條連線都可能落在 host 名稱後面的另一台機器，每條都要登入一次，host 要 OTP 時每條各要一組，而把一次傳輸分到幾條上，還需要兩個 server 自己的 session；見架構重新設計草稿的最後一版，在 `f8085a6`。
- **重建斷掉的 stream**（A2 做的，2026-10-25 拿掉）。client 在同一條 SSH connection 上再建一條 stream，連線斷了就先重新登入，session 再重送斷掉的那條上的內容，所以傳輸能撐過短暫的斷線。代價是一個 session，要把送出的一切編號、確認並保留到對方收到；server 要等的一分鐘寬限時間；以及傳輸中途的登入。改為讓失敗的傳輸再執行一次，做過的會略過，如同 rsync。
- **在一個 master 上開更多 session 或轉送埠，以得到更多條流量。** SSH 把一個 master 的每個 session 與每個轉送埠，都作為 channel 走在它唯一的 TCP 連線上，因此它們共用一條流量及其壅塞控制。在一個 master 上開四個 session，並不比一個快。
- **Ferry 自己的 TCP 埠。** 直接連到 server 開啟的埠，可以在不增加登入的情況下得到多條流量。但這需要 Ferry 自己的加密，以及經由 SSH 傳遞的金鑰、host 上開放的埠，還有在防火牆阻擋時退回 SSH 的機制；而且兩台機器必須能互相連線，由 client 建立 stream 則從來不需要。上面 loopback 上的 port 不是這種：它只經由 SSH 連到，是該 host 那條 SSH connection 上的一條 channel。
- **probe 與啟動 server 合成一個 session。** build 已裝好時，probe 直接變成 server。一般的情況一樣省一個 session，但 server 會在別的 host 問過、裝好之前就啟動；sudo 的檢查放不進去；要安裝、或兩邊在同一個 host 時都省不到。
- **第一個 session 裡留一個 `sh` 的迴圈**，每次讀一行請求、執行、以加了標記的行回答，host 上不需要 Ferry。但它一次只能做一件事，同一個 host 上的第二個 server、或一個 fixture，又要再開 session；接在請求後面的上傳要剛好讀到它的長度，只有 GNU 的 `head -c` 做得到，`dd bs=1` 則每個位元組一次系統呼叫。它也不能是從標準輸入讀的腳本（`sh -s`）：dash 會多讀，把後面的東西當成指令執行。
- **只以上傳安裝。** 不需要 host 有什麼，下載也只少傳約 5 MB；但 2026-10-25 起，照 VS Code 預設由 host 下載，上傳留給下載失敗的地方。
- **跨次執行留著 monitor**，像 VS Code 留著它的 server，連第一個 session 都省。下次同一個名稱可能連到另一台 login node；留在 login node 上的行程可能被清掉；它帶著啟動時的環境，`.bashrc` 或 module 改了之後就舊了；每個 build 要各一個；何時結束也要有規則。
- **在 `.bashrc` 加一行，遇到 Ferry 就先 return**，以 Ferry 指令裡的一個字辨認，替加了它的人省下那一個 session 的啟動檔，nano4 上是 1.6 到 3.7 秒。2026-10-25 決定不做。
- **送 `SHLVL` 過去**，bash 看到就不讀 `.bashrc`：host 的 `sshd` 只收它 `AcceptEnv` 列的變數，預設只有 `LANG` 與 `LC_*`。
- **記住上次的 probe。** 同一個名稱後面的每台 login node 各有自己的 machine ID，build 的路徑也就不同；root 每次也不同。
- **以 `ssh -R` 連回 client，給第一個 session 一條自己的輸出。** 只有 bash 的 `/dev/tcp` 連得出去，`sh` 與 dash 不行；host 要允許 remote forwarding；而且會在 host 的 loopback 上開一個 port，那裡的任何使用者都連得到。加了標記的行就夠了。
- **host 之間直接連線，像 mosh**，金鑰經 SSH 交給它們。host 的資料不經過 client，client 可以中途離開，每條 stream 可以在各自的核心上加密，斷了用金鑰再連，不必登入。但 server 得在某個埠上 listen 或往外連，兩台 host 得互通，Ferry 還得維護一層加密（Noise 或 TLS），而連線一律由 client 建立；它能繞過的窗口與登入，一條 SSH connection 上的多條 channel 已經解決（2026-10-23）。
- **一台 host 以 `ssh` 連到另一台，轉送 agent。** 這會在 host 上放憑證或轉送的 agent，而 Ferry 不在任何 host 上放；OTP 也無法轉送。
- **一側在這台機器上時，把 peer 放進 client 的 process。** 少兩個 process，沒有 socket，也不必序列化。但 client 就成了 peer，而它從來不是；server control、停止時刪暫存檔、各 stage 都要在 client 裡另做一套；本機的測試也不再走 host 的那條路。client 把 stream 的兩端交給這台機器上的 server，和 host 上一樣，本機到本機只差選項（2026-10-23）。

## Monitor

**已決定**（2026-10-26）。誰還活著、什麼時候該結束、失去連線留下的東西怎麼收拾、使用者的 Ctrl-C 怎麼傳到每一頭、誰說出哪裡卡住，都是 **monitor** 的事：每個地方一個，client 的機器上一個，每個 host 上一個，是 Ferry 在那裡執行的一切的頂層。peer 對這些一無所知。

- **每個地方一個。** client 的 monitor 是 client 裡的一條 thread：它管傳輸、每個 host，以及 client 啟動的每個行程，包括 `ssh` 與本機的 server。host 的 monitor 是 `ferry monitor`，即 host 的第一個 session 變成的東西；它管它在那裡啟動的東西。
- **一個不做 I/O 的核心。** 兩者跑同一個狀態機，一次被告知一件發生的事，連同時間，然後說要做什麼：結束一個 Scope、對一個行程送訊號、關掉一條連線、叫 host 的 monitor 停下、告訴使用者。所以它們做的事以合成的事件與時間就能測試。它周圍的 thread 做 I/O：心跳、每個它啟動的行程各一個觀察者（以 `waitpid`，除了怎麼結束，也說得出被暫停或繼續）、訊號，以及每條連線。
- **每個等待都在一個 Scope 裡。** monitor 持有一棵 Scope 樹：在 client 上，是傳輸，底下每個 host，每個 host 底下是它的連結、它的 server，以及承載它們連線的東西；本機的 server 直接掛在傳輸底下。每個等待，不論是等連線、listener 還是行程，都在其中一個裡，等到的東西來了，或它的 Scope 結束了，就返回，例如 host 失去連線時它的 Scope 結束：所以 host 不在之後，沒有任何東西還在等它，也沒有等待需要自己的時間。
- **連線就是 wire。** 只有 transport 握著連線的描述元，其他地方都以 **wire** 讀寫它，它的等待都在它的 Scope 裡：先是原始的位元組，例如 shell 印的東西、start mark、握手、port 的秘密；ready 之後，是它的 pipe 的 frame。它的兩端不阻塞；host 的 monitor 把一條交給它啟動的程式時，先改回阻塞，自己不留任何讀寫得了的東西。所以「連線上的每個等待都在它的 Scope 結束時返回」由型別保證；等連線進來（在 listener 上）、等行程（由它的觀察者）也一樣。不經 wire 的寫只有兩處：host 的 monitor 在第一個 session 的輸出上說出 socket 在哪裡的那幾行，是阻塞的寫，因為那個輸出和使用者的 shell 留在背景的程式共用，設成不阻塞，它們的寫入就會失敗；以及 `ferry build-id`，它像一般程式一樣印出幾十個位元組，只有 `--remote-sudo` 的檢查時，那才是一條 channel。兩者都只寫一次，最多幾百個位元組，失去連線時由啟動它的收拾。
- **一個沉默的值。** 會結束東西的時間只有三個，都是設計裡唯一的沉默，30 秒：host 沉默多久算失去連線；心跳出現之前的等待最多多久，例如 monitor 等被追蹤、一條連線說出它是什麼；以及失去連線的 host 給它啟動的東西的寬限。其他的時間只影響顯示，或怎麼解讀按鍵：等超過一秒就說出來，3 秒內的第二次 Ctrl-C 才強制，60 秒沒有進展就說出每個 thread 在哪裡。登入有 `ssh` 自己的限制，等一個不回應的 ControlMaster 30 秒。等網路上的第三方，有它進展的下限：以 `curl` 下載，10 秒連不上、或 30 秒都低於 1 KiB/s 就放棄；以 `wget`，30 秒沒有動靜就放棄。等一個人不計時，見下面。
- **一個 host 的 Liveness。** 在 client 上，一個 host 在不在，一次只由一個來源說：登入時，是 `ssh` 自己的限制；接著是第一個 session 說的每一行，其中每 5 秒一行 `alive`；再來是連結上的心跳，以及從那個 host 來的其他任何東西：server 的 control connection 上的任何東西，或 server 每秒說的、它的 stream 從那裡送來的東西。30 秒沒有聲音，host 就算失去連線：傳輸失敗，並說出來，例如 `nothing came from fatcat for 30 s`，之後不再等那個 host 的任何東西。傳輸因為別處失敗而正在結束時也一樣，因為還在等的 host 上的東西要經它的連線來；直到那裡的一切都做完、它的連結依序關上為止。第一個 session 的腳本同樣每 5 秒聽到 client 一次，30 秒沒聽到就結束整個 session；host 的 monitor 則聽連結上的心跳，以及它的 server 的 stream 從 client 那側送來的東西，那由每個 server 每秒在它的生命線上以一行加了標記的行告訴它。心跳和資料共用 SSH connection 那一條 TCP，排在資料後面：在慢的、或掉過包的連線上，會晚好幾秒（2026-10-26 量的），那段時間由送來的資料代替它。「停」與 host 的回報也一樣：在慢的連線上，可能晚好幾秒才到。
- **server 是 worker。** server 沒有心跳，也沒有 Scope 樹：它的等待都在一個沒有人結束的 Scope 裡，因為 server 是整個結束的：它在被告知停下時結束（control connection 關閉，或收到 `SIGTERM`），在它的 monitor 不在了時結束，或在做完時結束。它的 monitor 把它的標準錯誤給它當**生命線**：一對 socket 的一端，另一端由 monitor 握著，並讀走 server 在那裡說的話。server 等它變成可讀，而這只在 monitor 不在了時才會發生，不論 monitor 怎麼結束，`kill -9` 也一樣；server 接著就像收到 `SIGTERM` 一樣停下。標準錯誤是 `sudo` 會保留的，所以生命線在 `--remote-sudo` 時照樣有效；`sudo` 自己在中間夾一個管道時，例如開了 I/O 記錄，server 看得出來，會說它的生命線不可靠。
- **看得到的就等。** 被暫停的 server，例如被 Ctrl-Z 或 `SIGSTOP`，會被說出來並等它：它繼續，傳輸就繼續。等超過一秒的，都會說出來，連同它從外面看起來怎樣。只有失去連線的 host，那裡已沒有人看得到，才在 `SIGTERM` 之後給它啟動的東西 30 秒，砍掉剩下的，然後結束；client 也不再等它的任何東西。
- **各自的 process group。** 每個經過 master 的 `ssh`，以及本機的每個 server，都在自己的 process group 裡執行，所以終端機的 Ctrl-C 只到 client，由它決定每個怎麼結束。登入用的 `ssh` 留在終端機的 process group，好在那裡讀密碼或 OTP。

**未採用。**

- **每條連線各自帶 keepalive，帶不了的階段各加時限**，即 2026-10-26 之前的做法。連線之間互不牽連；但時限散在十幾處，每多一種連線或一個階段，都得記得補一個：有些就漏了，在一條無聲斷掉的連線上一直等下去。
- **每個行程一個 monitor。** 每個 server 有自己的 Scope 樹與心跳。改成每個地方一個，server 是它的 worker，少一層。
- **host 的 monitor 啟動的東西不依賴它**，即 2026-10-26 之前 launcher 的原則。失去連線時留在 host 上執行的東西就一直留著；monitor 停下它啟動的東西，才收拾得掉。
- **monitor 出現之前，讓 host 盡量不等 client。** client 在第一個腳本裡就給 host 決定所需的一切，由 host 自己下載、安裝、成為 monitor，只有上傳時才等。省下的是幾個來回，到 nano4 每個約 40 ms，相對那裡一個 session 的 1 到 2 秒很小；上傳仍要靠兩個方向的心跳；而且沒有 `--yes` 時，會改變何時問使用者。要啟動更快，可以和健康分開來做。
- **生命線不用標準錯誤。** 另一個描述元，例如 3 號，會被 `sudo` 的 `closefrom` 關掉，它的 `-C` 要 sudoers 設了 `closefrom_override` 才能用。Linux 的 `PR_SET_PDEATHSIG` 在 `sudo` 這個 setuid 程式啟動時就被清掉，macOS 也沒有。管道在 macOS 上沒辦法安靜地等：讀端不在了，只告訴等著寫的一方，而等寫的一方平常就一直被叫醒。
- **client 攔下 Ctrl-Z**，停下之前先叫每個 host 的 monitor 不要判斷它的沉默。這樣停住時，若連線無聲斷掉，host 上它啟動的東西就沒有人結束。所以 client 停住超過 30 秒，傳輸就失敗，和 rsync 不同，rsync 預設沒有逾時。

### 使用者的操作

| 使用者的操作 | 會發生什麼 |
|---|---|
| Ctrl-C、`SIGTERM`，或關掉終端機（`SIGHUP`） | client 在連結上叫每個 host 的 monitor 停下，monitor 對它啟動的東西送 `SIGTERM`，並說出每個怎麼結束；本機的每個 server 收到 `SIGTERM`。client 說出 `stopping; press Ctrl-C again within 3 s to stop at once`，超過一秒就說出還在等什麼。全部結束之後，照那個訊號結束，Ctrl-C 是 130。 |
| 3 秒內的第二次 Ctrl-C | 強制：client 啟動的每個行程都被砍掉，每條連結關掉，client 立刻結束；每個 host 的 monitor 讀到連結結束，在寬限內停下它啟動的東西。 |
| 超過 3 秒的第二次 Ctrl-C | 算是新的第一次：client 再說一次在等什麼，再給 3 秒，免得誤觸兩下就強制結束。 |
| client 被砍掉，例如 `kill -9` | client 裡什麼都不再執行：本機的 server 發現它們的生命線斷了；每個 host 的 monitor 發現連結結束或沉默，停下它啟動的東西，然後結束。 |
| Ctrl-Z | 只停住 client：host 不再聽到它的心跳，30 秒後把它當成不在了。 |

問使用者的問題，例如要不要在 host 上安裝、要不要同步進家目錄，不論多久都等，但第一次 Ctrl-C、`SIGTERM` 或 `SIGHUP` 就結束。

### 使用者看到的

monitor 從外面、從作業系統看它啟動的東西，程式自己說不了話時也看得到，例如它所有的 thread 都在等一個不回應的檔案系統：

- **Linux** 讀 `/proc`：每個 thread 是在執行、在等、被暫停，還是在不可中斷的等待（D state，連 `SIGKILL` 都結束不了）；卡在核心的哪裡；在這種等待裡時，它的系統呼叫與檔案，例如 `reader is in a wait nothing interrupts, in the kernel at request_wait_answer, which waits for a FUSE filesystem, in pread64() on /mnt/x`。核心函式的名字看得出來時，說出 NFS、FUSE、SMB、Lustre 或 Ceph。
- **macOS** 用 `proc_pidinfo`：行程是否被暫停，每個 thread 在執行還是在等。macOS 上造不出不可中斷的等待，沒有試過。
- **其他平台**只說它還在。經 `sudo` 的 server 以 root 執行，只看得出它還在或不在了。

Ferry 的每個 thread 都以它的角色命名，讓作業系統看得出是哪個在等。host 的 monitor 在 client 問時於連結上說出來；client 自己的則自己讀。在 client 說出它在等什麼、server 被暫停或繼續、按下 `Ctrl-\`、以及 60 秒沒有進展時說出來，旁邊照舊附上每個 server 對自己 thread 的說法；見[效能剖析](profiling.zh-TW.md#每條-thread-的快照)。看一眼不做任何決定，也不計時。

## 在遠端 host 上安裝

**已決定。** 本機上的 server 就是 client 自己的執行檔，因此不需要為它探測或安裝任何東西。在 host 上：

- **client 與 server 執行完全相同的 build。** 協定不需要與舊版本相容，各版本之間可以自由變更。交握仍會比對 build，遇到不符時以清楚的錯誤拒絕，而不是默默執行錯誤的那一個。build 在 hello 的其他內容之前就讀取，所以這是每個 build 都保持不變的唯一布局：hello 是第一種 control 訊息，build 是它的第一個欄位，另外還有拒絕訊息的布局。
- **build 並存安裝。** 每個 build 在 host 上都有自己的目錄，client 執行與自己相符的那一個。兩個不同版本的 client，例如在筆電與桌機上，可以連到同一個 host 而不會互相覆蓋。由於 server 不常駐，永遠不會有正在執行的 server 需要替換。
- **只有 release 工作流程會建置 release build。** 它在編譯時設定 `FERRY_RELEASE`。其他 build 都是 dev build，即使是以 `cargo build --release` 最佳化的也一樣，因為它的原始碼可能與同版本的 release 不同。
- **安裝在 host 唯一的 session 裡做**，在探測之後、session 成為 Ferry 的 monitor 之前，所以不必另開 session。上傳的東西，無論是 release 的執行檔還是 dev build 的原始碼，都是 session 讀的最後一樣：負責安裝的指令把它讀到結束，然後自己成為 monitor。
- **release build 由 host 自己下載**，和 VS Code 的 server 一樣：從 GitHub Releases 下載與 client 版本相同、符合 host 作業系統與架構的 release，用 `curl` 或 `wget`，並以該 release 的 `SHA256SUMS` 為它列的 SHA-256 核對；`SHA256SUMS` 由 client 抓來、留著，連同網址一起交給 host。host 兩種工具都沒有、或 `sha256sum` 與 `shasum` 都沒有，或者下載失敗、內容不符時，client 改為上傳，在同一個 session 裡：它自己以 `curl` 下載執行檔，以同一份清單檢查，在本機快取，再經由 SSH connection 上傳。所以 host 不需要對外的網路連線，而叢集的計算節點常常沒有。Linux 執行檔是[靜態 musl build](#linux-上的靜態-musl-build)，因此能在任何發行版的 C 函式庫上執行。host 先檢查下載或上傳的檔案，才以改名把它放到定位：檢查 SHA-256，上傳的還檢查大小，host 沒有 `sha256sum` 也沒有 `shasum` 時就只檢查大小；接著執行檔必須能在那裡執行，並以 `ferry build-id` 表明自己就是要安裝的 build。因此被 Ctrl-C 或斷線截斷的下載或上傳不會安裝任何東西，無法在那裡執行的執行檔，例如家目錄以 `noexec` 掛載時，在安裝時就會發現。每次安裝都寫入自己的檔案，因此兩個同時安裝的 client 不會混到彼此的執行檔。
- **host 自己下載，還沒在真的 host 上跑過。** 只有 release build 會走這條路，而還沒有任何 release 帶著它：v0.16.0 比它早。它的腳本個別測過：下載失敗時說出原因，session 接著讀下一個指令，和上傳時一樣。
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
