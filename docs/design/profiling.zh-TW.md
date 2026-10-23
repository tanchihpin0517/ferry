# Ferry 效能剖析

[English](profiling.md)

狀態：**已決定，且已實作。** [架構](architecture.zh-TW.md)說明本文所量測的 client 與 server 的 thread；[管線](pipeline.zh-TW.md)則說明它們執行的 stage。

**目錄**

- [它回答什麼](#它回答什麼)
- [Thread 與狀態](#thread-與狀態)
- [Thread 在哪裡切換狀態](#thread-在哪裡切換狀態)
- [它的成本](#它的成本)
- [剖析結果](#剖析結果)
- [邊傳輸邊追蹤](#邊傳輸邊追蹤)
- [每條 thread 的快照](#每條-thread-的快照)
- [它不涵蓋什麼](#它不涵蓋什麼)

## 它回答什麼

**已決定。** 效能剖析回答關於一次傳輸的三個問題：

1. 時間花在哪裡：client 與每個 server 的每條 thread，在每種工作上各花了多久。
2. 誰在等誰：其他部分在等哪一側、哪一個佇列。
3. 傳輸卡住時，每條 thread 停在哪裡，以及停了多久。
4. 傳輸進行時，每一秒裡每一側被什麼拖住：client 追蹤這些，之後也將據此調整傳輸。

## Thread 與狀態

**已決定。** Ferry 像 kernel 一樣記錄時間：只在每份工作必經的少數地方記錄，而不是包住每份工作。

- Ferry 的每條 thread 都以一個角色登記，例如 `scan`、`compress` 或 `pump`。同一個角色的 thread 會加總成一筆。每一側的 stage 在它們的 stop 之下啟動時登記。
- 已登記的 thread 任何時刻都只處於一個狀態：執行某種具名的工作，例如 `stat entries` 或 `compress`；或等待某個具名的對象，例如 `wait for the compressors` 或 `read the connection`。沒有命名的工作是 `run`。
- Thread 只在工作經過的地方切換狀態：佇列、鎖、連線、pool 的工作。每次切換時，距上一次切換的時間會記到它離開的那個狀態。狀態可以巢狀：內層的時間歸內層，內層結束後再回到外層。
- 對每個狀態，角色會記錄總時間、停留了幾次、最長的一次，以及以 2 的次方分桶的停留時間 histogram，p50 與 p99 由它讀出。

每條 thread 只寫自己的 slot，而且用一次讀取加一次寫入，而非 read-modify-write，因此 thread 之間從不爭用。Thread 結束時，它的 slot 會併入所屬角色的總計。

**已決定。** 效能剖析一直開著。它是 Ferry 本身的一部分：client 靠它追蹤傳輸，而它的成本小到不值得省略；見[它的成本](#它的成本)。它不是給使用者關掉的。只有測試與效能量測會關掉它，以量測它的成本，用的是隱藏的 `--no-profile`，它會在 client 與各個 server 上把一切都關掉：不登記任何 thread、不計時、不送出 metrics，也沒有快照。不會用任何估計來替代。

## Thread 在哪裡切換狀態

**已決定。** 以下是這些地方，以及它們讓 thread 進入的狀態：

| 位置 | 執行 | 等待 |
|---|---|---|
| Pool（stat、壓縮、解壓縮） | 該 pool 的工作 | 等工作；送件者等佇列的空間；收件者等下一個結果 |
| 掃描 | `list directories`、`stat entries` | 列出者等 stat 執行緒；送出者等下一個結果 |
| Mux 與它的 pump | | 等訊息的空間；等資料的空間；pump 等封包，以及寫出封包 |
| Striped transport | | 每條路徑的寫入者等 piece，以及寫出它；每條路徑的讀取者讀取，以及等待接收者；接收者等下一個 piece |
| Relay | | 從一個 server 讀取，寫到另一個 |
| Server | | 讀取連線；寫給 peer；從 peer 讀取 |
| Source | `read files`、`look up owners' names` | 等讀取者；等要送的檔案；掃描等需要的檔案送出，以及等 Destination 比較 |
| Destination | `list directories`、`compare`、`apply comparisons`、`delete`、`write files`、`rename files` | 等比較執行緒；等 record stream；applier 等刪除的執行緒；等寫入者；等某個寫入者；等檔案的位元組；等寫入者完成 |

## 它的成本

**已決定。** 效能剖析的成本不得超過傳輸時間的 2%。

在一台 24 核心的 Linux host（i7-13700K，時鐘來源為 TSC）上量測，兩側都在本機，每種各交替執行五次，取中位數：

| 傳輸 | 加入剖析之前 | 已內建，不計時 | 計時 |
|---|---|---|---|
| 1010 萬個項目的 dry run | 9.42 s，CPU 40.1 s | 9.29 s，CPU 40.3 s | 9.35 s，CPU 40.6 s |
| 複製 10 萬個 1 KiB 的檔案，`--no-fsync` | 1.42 s，CPU 17.5 s | 1.33 s，CPU 16.6 s | 1.38 s，CPU 17.2 s |

時間上的差距都在同一個版本多次執行之間的波動範圍內。計時時，dry run 的 CPU 時間約多 1%，而它的狀態切換超過一億次。16 條 thread 同時切換時，每次切換約 22 ns；未開啟剖析時約 2 ns。在 macOS 上分別約為 45 ns 與 4 ns。

改為一直開著、每秒送出 metrics、並由 client 追蹤之後，又與 `--no-profile` 比較量測了一次，取交替執行的中位數：

| 傳輸 | `--no-profile` | 一直開著 |
|---|---|---|
| tmpfs 上 10 萬個 1 KiB 的檔案，`--no-fsync --no-compress`，最壞的情況，在該 Linux host 上 | 0.91 s，CPU 10.7 s | 0.90 s，CPU 10.5 s |
| 1010 萬個項目的 dry run，在該 Linux host 上 | 2.87 s，CPU 17.0 s | 2.80 s，CPU 17.0 s |
| 10 萬個 1 KiB 的檔案，在一台 M 系列晶片的 Mac 上 | 23.07 s，CPU 131 s | 22.93 s，CPU 129 s |

時間與 CPU 都看不出超出多次執行之間波動範圍的差異。

## 剖析結果

**已決定。** 傳輸結束時，加上 `--profile` 才顯示時間花在哪裡，接在摘要後面：在儀表板之後、在進度列之後，或在標準錯誤輸出不是終端機時單獨顯示。沒有加時什麼都不說，因為多數執行都用不到它。它很簡短：先是 client，再是每個 server，每個角色一行，附上它的時間（該角色所有 thread 的總和），以及占該時間至少二十分之一的狀態，由長到短，最多四個。一千萬個項目的 dry run，不到 30 行就說完。`--profile-file FILE` 另外把完整內容以 JSON 寫到 `FILE`，包含每個狀態的次數、最長的一次停留與 histogram，以便比較兩次執行；不論是否顯示摘要都會寫出。以下為節錄：

每個 server 在它的 peer 完成後，以一則 report 把剖析結果送給 client；client 以該側的名稱保存它。

```text
ferry: 10,141,738 entries compared, 10,116,010 files to send, in 2.9 s
ferry: where the time went
the client
  relay from the Source ×1, 2.92 s: read the connection 95%
  relay from the Destination ×1, 2.92 s: read the connection 82% · run 17%
the Source on this machine
  scan ×1, 2.88 s: list directories 83% · wait for the stat threads 14%
  stat ×8, 23.02 s: wait for work 59% · stat entries 41%
  manifest ×1, 2.88 s: wait for the next result 64% · run 36%
the Destination on this machine
  compare ×1, 2.91 s: wait for work 99%
  receive ×1, 2.91 s: apply comparisons 44% · read the connection 42% · run 13%
```

p50 或 p99 是它在 histogram 中所屬桶子的上限，因此是 2 的次方奈秒，但絕不超過最長的那次停留。

## 邊傳輸邊追蹤

**已決定。** 傳輸進行時，client 一秒一秒地追蹤每一側的 thread 把時間花在哪裡。

- **每個 server 每秒送出它的 metrics**，走它的 control connection，那裡絕不會排在傳輸的資料後面：對每個角色，它有幾條 thread，以及剛過去這一秒裡每個狀態的時間與停留次數。狀態以編號表示，因為 client 與它的 server 是同一個 build。一秒的 metrics 只有幾百位元組。
- **它們也說明每個佇列有多滿：** 對程序裡的每個佇列，這一秒結束時裡面有多少、這一秒內最多有多少，以及它的上限。server 的佇列有：它的訊息、它的 manifest（以位元組計）、送進 connection 前的資料；每個 stage 前的佇列，從讀取到寫入；Source 上待送出的 `needed files`，以及已送出、還沒比較的 `entries in flight`；每條 path 等待寫出的片段，記為 `send path N`；以及已收到、尚未被取走的片段，記為 `receive`。client 的則是它轉送的每一側的各條 path 與 `receive`。佇列是在東西放進去時計數的，所以顯示超過上限的佇列，表示有人正等著再放進去。計數的成本是每次放入或取出一次原子加法，峰值再多一次；讀取則是每秒幾次讀取。
- **client 每秒也取得自己的 metrics**，並保留每一側最近 10 秒的資料。它是唯一能同時看到兩側，以及兩者之間經過 client 的網路的地方。每一側的佇列有多滿，它也顯示在儀表板上，並把整次執行的每一秒都保留下來，寫進 `--profile-file` 的 `queues`：每一側每一秒一筆，附上它到達的時間（從傳輸開始算起），以及每個佇列的量、峰值與上限。
- **它看到的內容交給一個 policy**，每當某一側的一秒資料到達時。policy 是將來調整傳輸的地方，例如壓縮等級或某個 stage 有幾條 thread，方法是 client 透過 control connection 送給各個 server 的指令；不論收到什麼指令，server 都會保有自己的硬上限，例如限制共用檔案系統負載的 8 條 stat thread。目前還沒有任何 policy 調整任何東西：policy 與指令都留待之後設計。在那之前，client 只是在看。

## 每條 thread 的快照

**已決定。** 在以下情況，client 會在標準錯誤輸出逐行說明每條 thread 停在哪裡：

- 收到 `SIGQUIT` 時，也就是按下 Ctrl-\ 時；或在儀表板上按下 Ctrl-\ 時，那時終端機不會送出訊號：這些行顯示在儀表板的警告中，儀表板關閉後再寫到標準錯誤輸出；或
- Server 之間 60 秒沒有任何東西經過時，每次停滯說一次。「經過」指的是一個 server 送給另一個 server 的東西，以及它經由 SSH connection 傳來的位元組，因為在很慢的鏈路上，一個 frame 可能就要更久；每個 server 每四分之一秒送給 client 的 report 不算，因為不論有沒有卡住都會送。

它說明自己的 thread 停在哪裡，並透過 control connection 詢問每個 server，server 也在那裡回答，因此回答不必排在傳輸自身的封包後面。同一個角色、處於同一個狀態的 thread 合成一行，並附上其中任一條停留最久的時間：

```text
ferry: where every thread is, as asked:
ferry: the client: relay from the Source ×1: write the connection, for 9 ms
ferry: the Source on this machine: scan ×1: list directories, for 176 ms
ferry: the Source on this machine: stat ×8: wait for work, for up to 103 ms
ferry: the Source on this machine: manifest ×1: wait for the next result, for 91 ms
ferry: the Destination on this machine: compare ×1: wait for work, for 9 ms
```

`SIGQUIT` 在 Ferry 啟動的 `ssh` 行程中保持封鎖，因此 Ctrl-\ 只會要求快照，不會結束它們；本機的 server 收到它時則什麼都不做。

## 它不涵蓋什麼

- **時間花在哪個函式。** 狀態說明 thread 在做什麼，而不是哪段程式碼；要知道這個，該用 `perf` 這類抽樣式的 profiler。
- **個別的項目。** 為數百萬個項目逐一切換狀態，成本會比它所量測的工作還高。
- **時間軸。** 沒有實作逐次切換的紀錄供 trace viewer 使用；之後可以加入，每條 thread 一個 ring buffer。
- **CPU 時間。** 狀態的時間是牆鐘時間：在 NFS 上執行 stat 的 thread，在等 server 回應時也處於 `stat entries`。要在一個狀態裡分辨工作與等待，就得在每次切換時讀取每條 thread 的 CPU 時鐘，成本較高；等有 policy 需要時再做。
- **調整傳輸。** 見[邊傳輸邊追蹤](#邊傳輸邊追蹤)。
