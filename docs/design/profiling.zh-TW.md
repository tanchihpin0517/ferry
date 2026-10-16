# Ferry 效能剖析

[English](profiling.md)

狀態：**已決定，且已實作。** [架構](architecture.zh-TW.md)說明本文所量測的 client 與 server 的 thread；[管線](pipeline.zh-TW.md)則說明它們執行的 stage。

**目錄**

- [它回答什麼](#它回答什麼)
- [Thread 與狀態](#thread-與狀態)
- [Thread 在哪裡切換狀態](#thread-在哪裡切換狀態)
- [它的成本](#它的成本)
- [剖析結果](#剖析結果)
- [每條 thread 的快照](#每條-thread-的快照)
- [它不涵蓋什麼](#它不涵蓋什麼)

## 它回答什麼

**已決定。** 效能剖析回答關於一次傳輸的三個問題：

1. 時間花在哪裡：client 與每個 server 的每條 thread，在每種工作上各花了多久。
2. 誰在等誰：其他部分在等哪一側、哪一個佇列。
3. 傳輸卡住時，每條 thread 停在哪裡，以及停了多久。

## Thread 與狀態

**已決定。** Ferry 像 kernel 一樣記錄時間：只在每份工作必經的少數地方記錄，而不是包住每份工作。

- Ferry 的每條 thread 都以一個角色登記，例如 `scan`、`compress` 或 `pump`。同一個角色的 thread 會加總成一筆。
- 已登記的 thread 任何時刻都只處於一個狀態：執行某種具名的工作，例如 `stat entries` 或 `compress`；或等待某個具名的對象，例如 `wait for the compressors` 或 `read the connection`。沒有命名的工作是 `run`。
- Thread 只在工作經過的地方切換狀態：佇列、鎖、連線、pool 的工作。每次切換時，距上一次切換的時間會記到它離開的那個狀態。狀態可以巢狀：內層的時間歸內層，內層結束後再回到外層。
- 對每個狀態，角色會記錄總時間、停留了幾次、最長的一次，以及以 2 的次方分桶的停留時間 histogram，p50 與 p99 由它讀出。

每條 thread 只寫自己的 slot，而且用一次讀取加一次寫入，而非 read-modify-write，因此 thread 之間從不爭用。Thread 結束時，它的 slot 會併入所屬角色的總計。

**已決定。** 不論傳輸是否開啟剖析，每條 thread 目前的狀態與進入的時間都一直記錄著，這樣卡住的傳輸不必事先為此開啟，也能說出它的 thread 停在哪裡；見[每條 thread 的快照](#每條-thread-的快照)。在 Linux 上它用的是粗略時鐘，讀一次只要幾奈秒，每幾毫秒前進一次。每個狀態所花的時間只在加上 `--profile` 時才記錄：此時每次切換會讀取精確時鐘。

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
| Source | `read files`、`look up owners' names` | 等讀取者；等要送的檔案 |
| Destination | `list directories`、`compare`、`apply comparisons`、`write files`、`rename files` | 等比較執行緒；等 record stream；等寫入者；等某個寫入者；等檔案的位元組；等寫入者完成 |

## 它的成本

**已決定。** 效能剖析的成本不得超過傳輸時間的 2%。

在一台 24 核心的 Linux host（i7-13700K，時鐘來源為 TSC）上量測，兩側都在本機，每種各交替執行五次，取中位數：

| 傳輸 | 加入剖析之前 | 已內建，未開啟 | 加上 `--profile` |
|---|---|---|---|
| 1010 萬個項目的 dry run | 9.42 s，CPU 40.1 s | 9.29 s，CPU 40.3 s | 9.35 s，CPU 40.6 s |
| 複製 10 萬個 1 KiB 的檔案，`--no-fsync` | 1.42 s，CPU 17.5 s | 1.33 s，CPU 16.6 s | 1.38 s，CPU 17.2 s |

時間上的差距都在同一個版本多次執行之間的波動範圍內。加上 `--profile` 時，dry run 的 CPU 時間約多 1%，而它的狀態切換超過一億次。16 條 thread 同時切換時，每次切換約 22 ns；未開啟剖析時約 2 ns。在 macOS 上分別約為 45 ns 與 4 ns。

## 剖析結果

**已決定。** `--profile` 會在傳輸結束時把剖析結果寫到標準錯誤輸出：先是 client，再是每個 server，列出每個角色，以及占其時間至少 0.5% 的狀態，由長到短。時間是該角色所有 thread 的總和。`--profile=FILE` 另外把完整內容以 JSON 寫到 `FILE`，包含每個狀態的 histogram，以便比較兩次執行。

每個 server 在它的 peer 完成後，以一則 report 把剖析結果送給 client；client 以該側的名稱保存它。

```text
ferry: profile
the client
  relay from the Destination ×1, 3.07 s in all
    read the connection                 2.52 s   82%        3,831×  p50  16.4 µs  p99   4.2 ms  max 134.5 ms
    run                               525.1 ms   17%       29,132×  p50   2.0 µs  p99 524.3 µs  max 944.1 µs
the Source on this machine
  scan ×1, 3.02 s in all
    list directories                    2.54 s   84%       48,751×  p50   4.1 µs  p99  32.8 µs  max 252.0 ms
    wait for the stat threads         400.9 ms   13%       25,366×  p50   1.0 µs  p99 131.1 µs  max   7.8 ms
  stat ×8, 24.16 s in all
    wait for work                      14.74 s   61%       25,374×  p50  32.8 µs  p99   1.0 ms  max 213.3 ms
    stat entries                        9.40 s   39%       25,366×  p50   8.2 µs  p99  16.8 ms  max  16.8 ms
```

p50 或 p99 是它在 histogram 中所屬桶子的上限，因此是 2 的次方奈秒，但絕不超過最長的那次停留。

## 每條 thread 的快照

**已決定。** 在以下情況，client 會在標準錯誤輸出逐行說明每條 thread 停在哪裡：

- 收到 `SIGQUIT` 時，也就是按下 Ctrl-\ 時；或
- Server 之間 60 秒沒有任何東西經過時，每次停滯說一次。

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
