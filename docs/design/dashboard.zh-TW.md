# Ferry 儀表板

[English](dashboard.md)

狀態：**草稿，討論中。** 每一節都標明它是已決定還是提議。[架構](architecture.zh-TW.md)說明顯示儀表板的 client，以及 server 傳給它的 report；[管線](pipeline.zh-TW.md)則說明它所顯示的 stage。

**已決定。** 傳輸在終端機上執行時，client 會顯示儀表板：一個就地更新的全螢幕畫面。它讓你追蹤傳輸，並找出拖慢傳輸的原因：進行到哪裡、每條 SSH connection 有多快，以及其他 stage 在等待管線中的哪一個 stage。

```text
ferry: ./data → ntu:/backup                                     00:42 elapsed
 ████████████████░░░░░░░░░░░░  58%   612 MiB of 1.0 GiB   ETA 00:31

 compared   10,000 entries  2,100/s   to send   8,000 files
 sent        4,620 files      340/s   failed        0

 transferring                              size          rate     ETA
   photos/2024/08/IMG_4211.jpg     45%    1.2 GiB    10.0 MiB/s   01:02
   photos/2024/08/IMG_4210.jpg    100%    3.1 MiB             —       —

 SSH connections to ntu           rate       heartbeat
   1 of 4                      9.8 MiB/s     2 s ago
   2 of 4                     10.1 MiB/s     1 s ago
   3 of 4                      9.6 MiB/s     3 s ago
   4 of 4                      9.9 MiB/s     2 s ago

 pipeline   read ▮▮▯▯  compress ▮▯▯▯  network ▮▮▮▮  decompress ▮▯▯▯  write ▮▯▯▯
 bottleneck: network          compression level 1, 41% saved

 queues                                 now   room   peak
   Source       read                      3     64     12  ▮▯▯▯
                compress                  2     16      4  ▮▯▯▯
                data                    257    256    258  ▮▮▮▮
                send ×4                  34     32     36  ▮▮▮▮
   client       from Source             256    256    256  ▮▮▮▮
                to Destination ×4         9     32     12  ▮▯▯▯
   Destination  receive                   0    256      2  ▯▯▯▯
                decompress                0     16      1  ▯▯▯▯
                write                     1     32      3  ▯▯▯▯
   back         Destination send 0/32 ↑1 · Source receive 0/256 ↑1
   messages     Source 0 ↑1 · Destination 0 ↑1
   manifest     Source 1.0 MiB/8.0 MiB

 warnings
   cannot read sub/locked.txt: permission denied
```

## 顯示內容

**已決定。**

- **整體傳輸**：進度條、已送出與待送出的位元組數、速率，以及剩餘時間。
- **計數**：已比較的項目、待送出的檔案、已送出的檔案，以及失敗的檔案；已比較的項目與已送出的檔案旁邊，附上過去 5 秒每秒的數量，讓有許多小檔案的目錄樹，在每秒位元組數說明不了什麼時，也看得出進度。
- **每條 SSH connection**：其速率，以及距上一次 [heartbeat](architecture.zh-TW.md#connection) 抵達已過多久，依 host 分組。本機上的位置沒有 SSH connection。
- **管線**：每個 stage 前的佇列有多滿，從讀取到寫入，以及哪個 stage 是瓶頸。
- **佇列**：依各側最近一秒的狀況，資料經過的佇列以表格呈現，依資料流動的順序，從 Source 經 client 到 Destination：每個佇列目前的量、上限、這一秒的峰值，以及表示充滿程度的格子；一條 connection 的各條 path 合為一列，例如 `send ×4`。接著，回傳的 needs 與 results、訊息，以及 manifest 的佇列，各一行，只列有用到的。佇列是在東西放進去時計數的，所以顯示超過上限的佇列，表示有人正等著再放進去；見[效能剖析](profiling.zh-TW.md#邊傳輸邊追蹤)，每條 path 各自每一秒的數字則見 `--profile-file`。
- **警告與失敗**：最新的幾則，隨到隨顯示。
- **正在傳輸的檔案**，如同 rclone 的顯示：Destination 正在接收或寫入的每個檔案，以相對於目的地的路徑列出，附上已到達的比例、大小、速率與剩餘時間。最先開始的排在最前面，讓大檔案停在原處，小檔案來來去去；顯示前 6 個，其餘只列出數量。路徑太長、螢幕放不下時，中間會以 `…` 省略。與 rclone 平行傳送檔案不同，record stream 一次只帶來一個檔案的位元組：最常見的是一個檔案在前進，排在它前面、已經全部到達的檔案則在等各自的 writer。

## 行為方式

**已決定。**

- **預設開啟。** 在標準錯誤輸出是終端機的地方，傳輸會顯示儀表板。`--no-ui` 則改為顯示進度列，就像有儀表板之前那樣。
- **擁有自己的全螢幕畫面。** 儀表板使用終端機的替代畫面。傳輸結束時，無論以何種方式結束，終端機都會恢復原狀，留下的是摘要列，接著是每則警告與失敗，寫到標準錯誤輸出，讓它們留在捲動紀錄中。
- **不會有任何東西印在它上面。** 傳輸期間原本會寫到標準錯誤輸出的警告與失敗，會顯示在儀表板中，並在它關閉時寫出。
- **dry run 也有儀表板。** 在進度條的位置，它顯示是什麼拖慢了執行：執行緒最忙的那一側，以及它在做什麼；在已送出與正在傳輸的檔案的位置，顯示列出了幾項，以及最近幾筆；不顯示 pipeline，因為它不攜帶任何東西。它的清單一找到就寫到標準輸出，除非標準輸出就是儀表板所在的終端機：那時不寫出，以免成千上萬行把畫面洗掉，結束時說明共有幾項，並提示把標準輸出導向別處才能看到。`--no-ui` 則照舊把清單一找到就寫到終端機。
- **不在終端機上時，照舊。** 在標準錯誤輸出不是終端機的地方，什麼都不顯示，或在使用 `--progress` 時每隔幾秒顯示一次進度列。
- **Ctrl-C 取消傳輸**，就和沒有儀表板時一樣。其他按鍵都不會，以免誤觸而取消。
- **它隨傳輸一起開始。** 每次登入都在準備期間、儀表板開啟之前完成，因此任何提示都不會出現在其中。

## 數字從何而來

**已決定。**

- **計數**來自 peer 的 report，與進度列相同。每秒的檔案數，在每個檔案的內容全部到達 Destination 時逐一計數，因為已送出的檔案數是以 rename 批次增加的，一次最多 256 個。
- **正在傳輸的檔案**來自 Destination：只要有這樣的檔案，它每秒回報一次每個已交給 writer、尚未完成的檔案，附上大小與已轉交給 writer 的位元組數；沒有時，它回報一次沒有。client 依據每個檔案過去 5 秒回報的位元組數，算出其速率。
- **管線的佇列**也來自 peer：每個 server 以一種新的 report 回報其佇列有多滿。Source 擁有讀取與壓縮之前的佇列，Destination 擁有解壓縮與寫入之前的佇列。
- **每條 SSH connection 的速率與 heartbeat** 來自 client 自己的 transport，它攜帶每條 path 與 heartbeat channel。
- 以 `ratatui` 搭配 `crossterm` 繪製。

- **進度條在內容抵達 Destination 時就計入**，不等其檔案完成，讓少數大檔案和許多小檔案一樣能推動進度條。待送出的位元組，是 Destination 比較後判定需要其內容的那些檔案的位元組。

**提議。**

- **每個 stage 前的佇列**：讀取是 Source 交給其讀取執行緒的檔案；壓縮與解壓縮是等待執行緒池的 frame；網路是 Source 等待 connection 的 data packet；寫入是 Destination 交給其寫入執行緒的檔案。每個 server 每秒回報四次其佇列。
- **瓶頸**是第一個在最近 3 秒平均下佇列至少四分之三滿、而其後一個 stage 的佇列並未如此的 stage；若直到最後一個的每個佇列都是滿的，則為最後一個。沒有任何佇列那麼滿時，就沒有瓶頸。
- **剩餘時間**是剩餘的位元組除以最近 5 秒的速率。
