# 設計草稿

[English](README.md)

這裡放的是還在研擬、還沒有決定的設計改動。`docs/design/` 說的是 Ferry 現在是什麼、已經決定要成為什麼；這個目錄放的是它可能變成什麼，在仍在討論或仍在實作的期間。

## 一份草稿的生命週期

1. **草稿。** 用下面的範本，在這裡新增一個檔案 `docs/drafts/<主題>.md`。用一種語言寫就夠了：草稿被採納之前，不需要另一種語言的版本。
2. **討論中。** 待決定的問題列在草稿裡，答案也寫回草稿，讓草稿隨時都說得出討論進行到哪裡。
3. **已採納。** 把決定寫進 `docs/design/` 的英文版與 `.zh-TW.md` 版本。這必須在第一次需要它的程式碼的同一個 commit 中完成，或更早。
4. **已併入或放棄。** 之後刪除草稿，連同下表中它那一列；歷史留在 git 裡。放棄的草稿也照樣刪除；放棄的理由若值得保留，就寫進該設計文件的 **Not chosen**。

草稿從不描述 Ferry 現在的行為。要知道現在的行為，請讀 `docs/design/` 與程式碼。

## 草稿

| 草稿 | 狀態 | 內容 | 相關 |
|---|---|---|---|
| [preparation.md](preparation.md) | 已採納：第 1 至 5 步已完成 | 把準備階段整理成明確的狀態機：各步驟、順序，以及每一步涵蓋的審查項目 | 目標架構第 6 節；WP-01、WP-09、WP-10；D11 |
| [rsync-parity.md](rsync-parity.md) | 已採納：第 1 至 5 步已完成 | 在同樣的目錄樹上跑 rsync 與 Ferry、比較結果的測試，以及刻意的不同 | DST-06、DST-13、SRC-09、SRC-13；D9、D10 |
| [architecture-redesign.md](architecture-redesign.md) | 已採納：B1 已完成，manifest 已以一條 stream 壓縮，A1 的六步也已完成：資料改以 block 傳送，Source 改成擁有者，資料額度，錯誤由決策者分類；之後在硬碟上量到讀大檔變慢，已改成照順序讀；B1 的補完也已完成：Destination 的決策者不做 I/O，收尾、分段檔案與批次都由它掌握，writing 這條執行緒拿掉，Want 比較完就送；A2 的前兩步：session 的核心，以模擬的 stream 和另一個對接測過，以及它的驅動者與 stream 的執行者，以 socketpair 測過，還沒接上；第三步：報告改走 server 的 control 連線；第四步：client 建 stream、把兩端交給 server，不經手資料，peer 改走 session，relay、全域重排與 heartbeat 拿掉，第五步：stream 斷了由 client 重建，SSH 連線斷了重新登入，必要時在終端機上問；第六步驗收：設計文件與圖照做好的改寫，stream 一再斷掉的複製測過，4 GiB 單檔快了約一成；server 的 control 連線沒有交給 session 判斷，所以第一條 SSH 連線斷了仍會使傳輸失敗；2026-10-25 重新決定：nano4 的同一個主機名稱後面有好幾台 login node，每條 SSH 連線可能落在不同的一台，所以每個 host 只有一條 SSH 連線、斷線就失敗、不重連，`--ssh-connections` 拿掉，session 縮成一條 stream 上的薄層；同一天以五步收回 A2 的重連：一條 SSH 連線、一條 stream 上的薄層、不重連、client 看 control 連線的沉默，以及驗收，驗收找到兩個問題，推上去之後 CI 又找到四個，都已修好；nano4 在推上去之後驗收了：三種組合的 `ferry test` 各 39 個全過，1 GiB 兩個方向的複製與同步照常 | 從高層重新設計架構：可靠性放在兩端的 session，以可重送的 block 端到端確認（2026-10-25 撤回：一條 SSH 連線，斷了就失敗）；只保留必要的順序；連線仍由 client 建立，建好就把兩端交給 server，自己不碰資料；由 Destination 發額度；每個 peer 一個狀態的擁有者加平行的執行者，只有 Destination 做決策；連線的健康只在一處判斷；以及量測的結果與改進選項 | 目標架構；ARC-04、D16；TODO.md 的 Reconnecting |
| [fsync.md](fsync.md) | 擱置：2026-10-24 拿掉 fsync，`--fsync` 會被拒絕；未來再回頭看 | Ferry 為什麼 fsync 每個檔案、怎麼做、在四種檔案系統上量到的代價、rsync 怎麼做、為什麼拿掉、拿掉之後少了什麼，以及加回來的做法 | c06ff61；ARC-01d、WP-19a、WP-15b |
| [one-session.md](one-session.md) | 已採納：第 0 至 7 步都已完成，只剩第 7 步在 nano4 上的驗收，等使用者開好 ControlMaster；第 6、7 步：追蹤 launcher 的連線與每個 server 的 control connection 都改走 stream 層，靠它的 keepalive 與沉默判斷，launcher 在連線沉默 20 秒內結束，不等可能要兩小時的 sshd，連線的健康也只在一處判斷；本機、fatcat、nano4 的 `ferry test` 都通過；與之前的 build 輪流跑，複製 1 KB 的中位數，到 nano4 從 2.00 秒變成 1.22 秒，到 fatcat 卻從 0.18 秒變成 0.20 秒：那裡一個 session 比 launcher 多出的那段還便宜 | 每個 host 只開一個 SSH session：啟動腳本 probe，需要時在同一個 session 裡安裝，然後 exec 成 Ferry 的 launcher，由它替 client 經 `ssh -W` 啟動 server、sudo 的檢查與 `ferry fixture`，和 VS Code 的 Remote-SSH 同一個結構；以及 nano4 上一個 session 的代價、考慮過的其他做法 | preparation.md；TRN-04、TRN-19 |
| [silent-drops.md](silent-drops.md) | 草稿：2026-10-26 建立，還沒討論；one-session 做完後再回來處理 | 網路無聲斷線，例如網路中斷時：每條連線的每個階段，每個等待都要有上限，不等 TCP 或 `sshd` 發現，那可能要兩個小時；目前看到的缺口，以及怎麼找其餘的 | one-session.md |

審查時寫下、仍放在審查目錄中的提案：

| 提案 | 狀態 | 內容 |
|---|---|---|
| [ARC-01 至 ARC-05](../review/2026-10-17/done/architecture.md) | ARC-01、02、03、05 已完成；[ARC-04](../review/2026-10-17/architecture.md) 決定不做（D16：等需要續傳單一檔案時再分段），之後由架構重新設計採用（2026-10-23：資料改成 block） | 寫入 stage；thread 的組合與停止；Destination 狀態的擁有者；分段的 record；其他小調整 |
| [目標架構](../review/2026-10-17/target-architecture.md) | 長期方向 | 如果從零設計，Ferry 會是什麼樣子，以及從現在走過去的步驟 |

一個提案開始動工時，就在這裡開一份草稿，並連回原本的提案。

草稿與提案用到的圖，放在 [assets/](assets/)：

- [stages-and-stop.svg](assets/stages-and-stop.svg)：ARC-02 的 supervisor 在一個 server 中的樣子，包括它的 stage、停止機制，以及一個 writer 失敗時會發生什麼。
- `redesign-*.svg`：[architecture-redesign.md](architecture-redesign.md) 的 17 張圖，各放在草稿用到它的地方。

## 範本

```markdown
# <主題>

- 狀態：草稿 | 討論中 | 已採納 | 已放棄
- 開始：YYYY-MM-DD；最後修改：YYYY-MM-DD
- 會改動：docs/design/ 中的哪些段落
- 相關：審查項目、issue、commit

## 問題

現在哪裡不對，或缺了什麼，附上證據：量測、失敗的情形、程式碼。

## 提案

改什麼、怎麼改，細到可以照著實作。

## 其他做法

還考慮過哪些做法，為什麼不採用。

## 待決定的問題

- [ ] 每個問題，以及由誰決定。

## 計畫

各個步驟。每一步結束時，都要有能執行、也測試過的東西。
```
