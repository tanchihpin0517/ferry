# Destination 與檔案系統層：待修項目

> 2026-10-22：已完成的項目（17 項）移到 [done/destination.md](done/destination.md)，這裡只留還沒完成或決定不做的。

- 對象：HEAD `3c17a2f`（version 0.14.4）。以下所有行號都對照這個 commit 重新核對過。
- 範圍：`src/destination.rs`、`src/fsys.rs`、`src/compare.rs`、`src/owners.rs`、`src/names.rs`、`src/record.rs`、`src/interrupt.rs`（暫存檔登記），以及它們如何使用 `src/manifest.rs`。
- 不在此文件中：`sync` 防範打錯的 DESTINATION（`$HOME`、`/`、`host:`）屬於 CLI 審查；Source 掃描每層祖先目錄占一個 fd 屬於 Source 審查。
- 每一項都可單獨閱讀，不需要先讀其他項目。
- 實驗程式：[`materials/destination/exp/`](materials/destination/exp/) 是一個以 path 依賴 ferry 的小 crate，直接把手工組出的 packet 餵給 `destination::run`。在 repo 根目錄執行 `CARGO_TARGET_DIR=$PWD/.scratch/review-dest/target cargo run --release --manifest-path docs/review/2026-10-17/materials/destination/exp/Cargo.toml`，或在最後加上 `-- dfs`，只跑 DST-08。下面每一項也都附有不依賴它的重現方式或測試草稿。
- 驗證狀態的意思：「已實測重現」是實際跑過並看到現象；「已核對程式碼」是逐行追過程式碼路徑，但沒有實際觸發；「推測」是可能，但沒有完整追到。

## 總表

| ID | 嚴重度 | 標題 |
|---|---|---|
| DST-06 | 中 | 檔案的擁有者、權限或 mtime 設不上時，已驗證的檔案整個被丟棄 |
| DST-12 | 低 | `remove_tree` 的遞迴深度沒有上限 |

DST-01 到 DST-10 採完整格式；DST-11 到 DST-19 是較小的項目，以簡短格式列在最後。

---

### DST-06：檔案的擁有者、權限或 mtime 設不上時，已驗證的檔案整個被丟棄

- 嚴重度：中
- 驗證狀態：已核對程式碼（未實測；需要以 root_squash 匯出的 NFS，或 Linux 上的 FAT、exFAT 掛載）。
- 位置：
  - `src/destination.rs:1686` `apply_file_attributes(&file, &target.entry, state)?;`
  - `src/destination.rs:1708-1723` `apply_file_attributes`
  - `src/destination.rs:1220-1235` `set_owner`：root 一律嘗試 chown
  - `src/owners.rs:54-62` `can_set_user`、`can_set_group`
  - `src/destination.rs:1150-1162`、`1191-1205`：只需屬性的項目與特殊檔案，同樣以 `?` 傳遞這類錯誤
- 現象：以 `sudo ferry copy` 或 `--remote-sudo` 寫進以 root_squash 匯出的 NFS（叢集上很常見），`fchown` 回 `EPERM`，每個檔案都以「cannot copy X: Operation not permitted」失敗，暫存檔被刪除，一個檔案都沒複製到。寫到 Linux 上沒有以 `quiet` 掛載的 FAT、exFAT，或某些 FUSE、SMB 檔案系統時，chmod 失敗的效果相同。rsync 遇到這些情況會保留檔案並回報屬性錯誤；Ferry 的概觀也說「若 Ferry 缺少設定某個項目的權限，它會略過該項並發出警告……檔案本身仍會傳輸」。
- 根本原因：
  1. 檔案的內容驗證通過後，`write_file` 在 `fsync` 與重新命名之前套用屬性，任何錯誤都讓這個檔案失敗並刪除暫存檔：

     ```rust
     // src/destination.rs:1710-1723
     fn apply_file_attributes(file: &File, entry: &Entry, state: &Mutex<State>) -> Result<()> {
         let metadata = file.metadata()?;
         let ownership = Arc::clone(&state.lock().expect("not poisoned").ownership);
         set_owner(
             &ownership,
             metadata.uid(),
             metadata.gid(),
             entry,
             |uid, gid| fsys::set_owner(file, uid, gid),
         )?;
         fsys::set_mode(file, entry.mode & 0o7777)?;
         fsys::set_mtime(file, entry.mtime)?;
         Ok(())
     }
     ```
  2. `set_owner` 只在「這個行程不能設定」時（非 root 且不是同一使用者，或不屬於該群組）才略過；root 一律嘗試，而 root_squash 下 root 的 chown 會被拒絕。
- 重現方式：
  - root_squash：在一台以 `root_squash` 匯出 NFS 的機器上，`sudo ferry copy src /nfs/dst`。現況：每個檔案都失敗。
  - FAT（Linux，需要 polkit 允許 `udisksctl`）：

    ```sh
    truncate -s 64M .scratch/dst06/fat.img && mkfs.vfat .scratch/dst06/fat.img
    udisksctl loop-setup -f .scratch/dst06/fat.img && udisksctl mount -b /dev/loopN
    chmod 4755 .scratch/dst06/src/a; chmod 600 .scratch/dst06/src/b
    $F copy --no-ui .scratch/dst06/src /media/$USER/<label>/dst
    ```
  - 不需特殊環境的單元測試：`set_owner` 已經以閉包接收 chown，可傳入一個回傳 `EPERM` 的閉包，見「要加的測試」。
- 修法：
  - 加一個分類函式，例如 `fn is_unsettable(error: &io::Error) -> bool`：`EPERM`、`ENOTSUP`、`EOPNOTSUPP`（Linux 上兩者同值，rustix 可能有兩個同值常數，用 `||` 比較即可），以及 utimens 的 `EINVAL`（某些檔案系統無法表示該時間，例如 FAT 不能存 1980 年以前）。
  - `apply_file_attributes`：chown、chmod、utimens 各自執行，順序不變（先擁有者，再模式，最後 mtime）。遇到可略過的錯誤時，`state.warnings.add(Warning::Owner | Warning::Mode | Warning::Times, shown)` 並繼續；其他錯誤仍讓檔案失敗。之後照常 `fsync` 與重新命名。函式需多收一個 `shown` 參數，供警告列出路徑範例。
  - `apply_entry` 的 `Verdict::Attributes` 與特殊檔案分支採同樣的分類。
  - 可選最佳化：同一台裝置上第一次 chown 得到 `EPERM` 之後，記在 `State` 中，之後該裝置上的檔案不再嘗試。
  - 可另外考慮（與 rsync 不同，需要決定）：擁有者沒能設成 Source 的擁有者時，清掉 `S_ISUID`；群組沒能設定時，清掉 `S_ISGID`，以免產生一個由接收端使用者擁有、Source 上並不存在的 setuid 檔案。
  - 平台：Linux 的 vfat、exfat 在模式無法表示時回 `EPERM`（掛載時加 `quiet` 則默默成功）；macOS 上 FAT、exFAT、SMB 的 chmod 多半被默默忽略；NFS root_squash 兩個平台都會回 `EPERM`。
- 要加的測試：
  - `src/destination.rs` 的 `mod tests`：`an_owner_that_cannot_be_set_is_a_warning`。在 `set_owner` 改成能記錄警告之後（例如回傳一個結果，或收 `&mut Warnings`），以回傳 `io::Error::from_raw_os_error(libc::EPERM)` 的閉包呼叫它，斷言回傳 `Ok`，且 `Warnings` 中有一筆。
  - 分類函式的單元測試：`EPERM`、`EOPNOTSUPP` 可略過，`EIO`、`ENOSPC` 不可。
  - 手動：如重現方式，在 FAT 映像上複製，檔案都在，警告彙總為一則。
- 驗收條件：
  - [ ] chown、chmod、utimens 因權限或不支援而失敗時，檔案仍以驗證過的內容就位，並以彙總的警告回報。
  - [ ] `EIO`、`ENOSPC` 等其他錯誤仍讓該檔案失敗，並刪除暫存檔。
  - [ ] 屬性套用的順序不變：擁有者、模式、mtime。
  - [ ] `cargo test` 全部通過。
- 文件要同步的地方：
  - `docs/design/overview.md`「Behaviour of `-a` metadata」的「Privilege warnings are summarised」（`overview.zh-TW.md`「權限警告會彙總」）：列出警告的種類（擁有者、權限、mtime、特殊檔案）。
  - `docs/cli.md`「Output」（`cli.zh-TW.md`「輸出」）：說明這類警告不讓檔案失敗。
- 相關項目：DST-05（目錄的同類問題）、DST-14。

> 2026-10-20 補記：WP-07 照修法做。檔案、只需屬性的項目、特殊檔案與目錄收尾共用 `set_attributes`：擁有者、權限、mtime 依序各自嘗試；`EPERM`、`EOPNOTSUPP`、`ENOTSUP`，以及 mtime 的 `EINVAL`，記為彙總的警告（`Warning::Owner`、`Mode`、`Times`），檔案照樣就位；其他錯誤在三者都試過之後，回傳第一個。沒有做的：同一裝置第一次 `EPERM` 之後不再嘗試的最佳化；擁有者或群組沒能設定時清掉 setuid 或 setgid，這需要使用者決定，沒有做，行為與 rsync 相同。測試：`what_cannot_be_set_is_a_warning_and_the_rest_is_set`、`only_privilege_and_support_make_an_attribute_unsettable`。沒有在 FAT 或 root_squash 的 NFS 上實測。

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

