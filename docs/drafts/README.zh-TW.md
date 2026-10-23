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
| [architecture-redesign.md](architecture-redesign.md) | 已採納：B1 的四步都做了，驗收已量；本機小檔要重量 | 從高層重新設計架構：可靠性放在兩端的 session，以可重送的 block 端到端確認；只保留必要的順序；連線仍由 client 建立，建好就把兩端交給 server，自己不碰資料；由 Destination 發額度；每個 peer 一個狀態的擁有者加平行的執行者，只有 Destination 做決策；連線的健康只在一處判斷；以及量測的結果 | 目標架構；ARC-04、D16；TODO.md 的 Reconnecting |

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
