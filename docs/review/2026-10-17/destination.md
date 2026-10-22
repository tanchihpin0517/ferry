# Destination 與檔案系統層：待修項目

> 2026-10-22：已完成的項目（18 項）移到 [done/destination.md](done/destination.md)，這裡只留還沒完成或決定不做的。

- 對象：HEAD `3c17a2f`（version 0.14.4）。以下所有行號都對照這個 commit 重新核對過。
- 範圍：`src/destination.rs`、`src/fsys.rs`、`src/compare.rs`、`src/owners.rs`、`src/names.rs`、`src/record.rs`、`src/interrupt.rs`（暫存檔登記），以及它們如何使用 `src/manifest.rs`。
- 不在此文件中：`sync` 防範打錯的 DESTINATION（`$HOME`、`/`、`host:`）屬於 CLI 審查；Source 掃描每層祖先目錄占一個 fd 屬於 Source 審查。
- 每一項都可單獨閱讀，不需要先讀其他項目。
- 實驗程式：[`materials/destination/exp/`](materials/destination/exp/) 是一個以 path 依賴 ferry 的小 crate，直接把手工組出的 packet 餵給 `destination::run`。在 repo 根目錄執行 `CARGO_TARGET_DIR=$PWD/.scratch/review-dest/target cargo run --release --manifest-path docs/review/2026-10-17/materials/destination/exp/Cargo.toml`，或在最後加上 `-- dfs`，只跑 DST-08。下面每一項也都附有不依賴它的重現方式或測試草稿。
- 驗證狀態的意思：「已實測重現」是實際跑過並看到現象；「已核對程式碼」是逐行追過程式碼路徑，但沒有實際觸發；「推測」是可能，但沒有完整追到。

## 總表

| ID | 嚴重度 | 標題 |
|---|---|---|
| DST-12 | 低 | `remove_tree` 的遞迴深度沒有上限 |

DST-01 到 DST-10 採完整格式；DST-11 到 DST-19 是較小的項目，以簡短格式列在最後。

---

## 較小的項目

以下各項影響較小，以簡短格式列出；每一項仍可單獨閱讀。

### DST-12：`remove_tree` 的遞迴深度沒有上限
- 嚴重度：低。驗證狀態：已核對程式碼。
- 位置：`src/fsys.rs:531-550`。
- 說明：每一層都持有一個開啟的目錄 fd 與一個 stack frame。一棵很深的多餘目錄樹（例如由之前某次執行、從惡意的 Source 建立）會用盡 fd 而刪不掉，或在開檔上限很高時讓 thread 的 stack 溢位而使行程崩潰。
- 修法：改成以明確的 `Vec` 當堆疊的迭代版本，並設深度上限（超過就回報 `NotDeleted`）；遇到 EMFILE 時以錯誤回報，不崩潰。
- 測試：`src/fsys.rs` 的 `mod tests`：建立 5,000 層的目錄樹，`remove_tree` 成功或回錯，但不崩潰。
- **2026-10-21 補記（WP-19a）**：照修法做：`remove_tree` 改成以 `Vec` 當堆疊，一次一層往下，深度上限 `REMOVE_DEPTH` = 4,096。超過時回錯「it is more than 4096 directories deep」，`sync` 把這個多餘的項目回報為未刪除，其餘照做；只有比上限更深的目錄才拒絕，最深一層的檔案照刪。讓進去的目錄刪不掉時還原權限，與原本相同。每一層仍持有一個開啟的目錄 fd，所以開檔上限低於 4,096 的系統上，更淺的樹也可能以 EMFILE 失敗，但只是回錯，不會崩潰；沒有做 scan 那種「超過一定深度就從根重新開啟」，因為刪除時從名稱重新開啟目錄，可能刪到中途被換進來的另一棵樹。與「要加的測試」不同：測試放在 `tests/sync.rs` 的 `a_deep_extra_tree_is_deleted_or_said_too_deep`，不放在 `fsys` 的 `mod tests`，因為測試行程持有的數千個 fd 會干擾同一行程中計算 fd 的 `a_deep_tree_holds_a_bounded_number_of_directories_open`；改成整合測試後，fd 開在子行程中。起初它刪掉一棵剛好 4,096 層的多餘目錄樹、留下一棵 5,097 層的；用舊的遞迴版本跑這個測試，Destination 會 stack overflow 而崩潰（本機確認過）。但這在 CI 上不可行：macOS 處理數千層的目錄樹超過 180 秒，容器的 overlayfs 在那種深度回 ENAMETOOLONG，測試行程從中途清理時開的數千個 fd 也讓同時執行的測試 EMFILE。因此加了只在 debug build 生效的 `FERRY_TEST_REMOVE_DEPTH`，測試以上限 64 刪掉一棵 64 層的、留下一棵 100 層的並以 23 結束，從中途刪掉之後再 `sync` 就刪完。這個淺的測試不再證明不會 stack overflow，那一點只靠上面那次手動確認。

