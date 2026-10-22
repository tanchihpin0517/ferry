# Destination 與檔案系統層：待修項目

> 2026-10-22：已完成的項目（18 項）移到 [done/destination.md](done/destination.md)，這裡只留還沒完成或決定不做的。

- 對象：HEAD `3c17a2f`（version 0.14.4）。以下所有行號都對照這個 commit 重新核對過。
- 範圍：`src/destination.rs`、`src/fsys.rs`、`src/compare.rs`、`src/owners.rs`、`src/names.rs`、`src/record.rs`、`src/interrupt.rs`（暫存檔登記），以及它們如何使用 `src/manifest.rs`。
- 不在此文件中：`sync` 防範打錯的 DESTINATION（`$HOME`、`/`、`host:`）屬於 CLI 審查；Source 掃描每層祖先目錄占一個 fd 屬於 Source 審查。
- 每一項都可單獨閱讀，不需要先讀其他項目。
- 實驗程式：[`materials/destination/exp/`](materials/destination/exp/) 是一個以 path 依賴 ferry 的小 crate，直接把手工組出的 packet 餵給 `destination::run`。在 repo 根目錄執行 `CARGO_TARGET_DIR=$PWD/.scratch/review-dest/target cargo run --release --manifest-path docs/review/2026-10-17/materials/destination/exp/Cargo.toml`，或在最後加上 `-- dfs`，只跑 DST-08。下面每一項也都附有不依賴它的重現方式或測試草稿。
- 驗證狀態的意思：「已實測重現」是實際跑過並看到現象；「已核對程式碼」是逐行追過程式碼路徑，但沒有實際觸發；「推測」是可能，但沒有完整追到。

## 總表

都已完成，移到 [`done/destination.md`](done/destination.md)。

DST-01 到 DST-10 採完整格式；DST-11 到 DST-19 是較小的項目，以簡短格式列在最後。

---

## 較小的項目

以下各項影響較小，以簡短格式列出；每一項仍可單獨閱讀。

這一節的項目都已移到 [`done/destination.md`](done/destination.md)。
