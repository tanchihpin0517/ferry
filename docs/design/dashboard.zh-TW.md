# Ferry 儀表板

[English](dashboard.md)

狀態：**草稿，討論中。** 每一節都標明它是已決定還是提議。[架構](architecture.zh-TW.md)說明顯示儀表板的 client，以及 server 傳給它的 report；[管線](pipeline.zh-TW.md)則說明它所顯示的 stage。

**已決定。** 傳輸在終端機上執行時，client 會顯示儀表板：一個就地更新的全螢幕畫面。它讓你追蹤傳輸，並找出拖慢傳輸的原因：進行到哪裡、每條 SSH connection 有多快，以及其他 stage 在等待管線中的哪一個 stage。

```text
ferry: ./data → ntu:/backup                                     00:42 elapsed
 ████████████████░░░░░░░░░░░░  58%   612 MiB of 1.0 GiB   ETA 00:31

 compared   10,000 entries        to send   8,000 files
 sent        4,620 files          failed        0
 writing    photos/2024/08/IMG_4211.jpg

 SSH connections to ntu           rate       heartbeat
   1 of 4                      9.8 MiB/s     2 s ago
   2 of 4                     10.1 MiB/s     1 s ago
   3 of 4                      9.6 MiB/s     3 s ago
   4 of 4                      9.9 MiB/s     2 s ago

 pipeline   read ▮▮▯▯  compress ▮▯▯▯  network ▮▮▮▮  decompress ▮▯▯▯  write ▮▯▯▯
 bottleneck: network          compression level 1, 41% saved

 warnings
   cannot read sub/locked.txt: permission denied
```

## 顯示內容

**已決定。**

- **整體傳輸**：進度條、已送出與待送出的位元組數、速率，以及剩餘時間。
- **計數**：已比較的項目、待送出的檔案、已送出的檔案，以及失敗的檔案。
- **每條 SSH connection**：其速率，以及距上一次 [heartbeat](architecture.zh-TW.md#connection) 抵達已過多久，依 host 分組。本機上的位置沒有 SSH connection。
- **管線**：每個 stage 前的佇列有多滿，從讀取到寫入，以及哪個 stage 是瓶頸。
- **警告與失敗**：最新的幾則，隨到隨顯示。
- **正在寫入的檔案**：每秒一次，顯示 Destination 此刻正在寫入的某一個檔案的路徑，相對於目的地，讓你看得出傳輸進行到哪裡。這是抽樣，不是每個檔案都顯示：有許多小檔案時，每個檔名都會變化太快，根本來不及看。路徑太長、螢幕放不下時，中間會以 `…` 省略。沒有檔案正在寫入時，這一行是空的。

## 行為方式

**已決定。**

- **預設開啟。** 在標準錯誤輸出是終端機的地方，傳輸會顯示儀表板。`--no-ui` 則改為顯示進度列，就像有儀表板之前那樣。
- **擁有自己的全螢幕畫面。** 儀表板使用終端機的替代畫面。傳輸結束時，無論以何種方式結束，終端機都會恢復原狀，留下的是摘要列，接著是每則警告與失敗，寫到標準錯誤輸出，讓它們留在捲動紀錄中。
- **不會有任何東西印在它上面。** 傳輸期間原本會寫到標準錯誤輸出的警告與失敗，會顯示在儀表板中，並在它關閉時寫出。
- **dry run 沒有儀表板。** 它在標準輸出上的清單才是它的目的，而且很快；它照舊顯示摘要。
- **不在終端機上時，照舊。** 在標準錯誤輸出不是終端機的地方，什麼都不顯示，或在使用 `--progress` 時每隔幾秒顯示一次進度列。
- **Ctrl-C 取消傳輸**，就和沒有儀表板時一樣。其他按鍵都不會，以免誤觸而取消。
- **它隨傳輸一起開始。** 每次登入都在準備期間、儀表板開啟之前完成，因此任何提示都不會出現在其中。

## 數字從何而來

**已決定。**

- **計數**來自 peer 的 report，與進度列相同。
- **正在寫入的檔案**來自 Destination：每秒一次，它回報某個 writer 正在寫入的檔案路徑，前提是與上次回報的不同。
- **管線的佇列**也來自 peer：每個 server 以一種新的 report 回報其佇列有多滿。Source 擁有讀取與壓縮之前的佇列，Destination 擁有解壓縮與寫入之前的佇列。
- **每條 SSH connection 的速率與 heartbeat** 來自 client 自己的 transport，它攜帶每條 path 與 heartbeat channel。
- 以 `ratatui` 搭配 `crossterm` 繪製。

- **進度條在內容抵達 Destination 時就計入**，不等其檔案完成，讓少數大檔案和許多小檔案一樣能推動進度條。待送出的位元組，是 Destination 比較後判定需要其內容的那些檔案的位元組。

**提議。**

- **每個 stage 前的佇列**：讀取是 Source 交給其讀取執行緒的檔案；壓縮與解壓縮是等待執行緒池的 frame；網路是 Source 等待 connection 的 data packet；寫入是 Destination 交給其寫入執行緒的檔案。每個 server 每秒回報四次其佇列。
- **瓶頸**是第一個在最近 3 秒平均下佇列至少四分之三滿、而其後一個 stage 的佇列並未如此的 stage；若直到最後一個的每個佇列都是滿的，則為最後一個。沒有任何佇列那麼滿時，就沒有瓶頸。
- **剩餘時間**是剩餘的位元組除以最近 5 秒的速率。
