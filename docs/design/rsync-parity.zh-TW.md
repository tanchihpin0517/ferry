# 與 rsync 的一致性測試

[English](rsync-parity.md)

Ferry 與 rsync 做同一件事的地方，Ferry 在 DESTINATION 留下的結果與 rsync 相同，結束狀態也相同；為什麼，以及刻意保留的不同，寫在[概觀](overview.zh-TW.md#rsync-也做的事結果與-rsync-相同)。這裡寫的是怎麼檢查：`tests/rsync.rs` 在每次 push 時，在同樣的目錄樹上執行兩者，比較它們留下的結果。

**目錄**

- [一個情境怎麼跑](#一個情境怎麼跑)
- [比較什麼](#比較什麼)
- [在哪裡跑](#在哪裡跑)
- [情境](#情境)
- [刻意保留的不同](#刻意保留的不同)
- [它找到的](#它找到的)
- [沒有比較的](#沒有比較的)
- [未採用](#未採用)

## 一個情境怎麼跑

**已決定**（2026-10-22）。每個情境是一個獨立的測試，可以平行執行：

1. 建好 SOURCE 與 DESTINATION 兩份，一份在給 rsync 的目錄，一份在給 Ferry 的目錄，並排放在一個暫存目錄裡。每個項目都設成同一個 mtime，到奈秒，除非情境另外指定；每個目錄在它的項目建好之後，mtime 再設回去。
2. 一份跑 rsync，一份跑 Ferry：
   - `copy` 對照 `rsync -a SOURCE/ DESTINATION`；
   - `sync` 對照 `rsync -a --delete SOURCE/ DESTINATION`；
   - 選項對照 rsync 同名的選項：`-x`、`--numeric-ids`、`--modify-window`。

   SOURCE 是目錄時，給 rsync 的 SOURCE 加上斜線，因為 Ferry 對每個 SOURCE 都這樣看，除非情境另外指定。兩者的標準輸入都來自 `/dev/null`；Ferry 加上 `--no-ui`。
3. 重跑的情境，對兩份做同樣的改動之後各再跑一次。
4. 每個情境跑兩種方式：原樣，以及 Ferry 另外加上不該改變結果的選項，`--listers 4` 與隱藏的 `--batch-entries 2`，讓好幾個目錄同時列出、比較，每個目錄一批兩個名稱。小小的樹因此也走大樹會走的路。
5. 取每個目錄底下的一切，包括兩個根目錄，以及符號連結的根目錄所指的東西，逐一路徑比較兩份，以及結束狀態。有任何不同，測試就失敗，列出每個不同的路徑、兩邊的結束狀態，以及兩者各說了什麼。

## 比較什麼

| 什麼 | 怎麼比 |
|---|---|
| 種類 | 檔案、目錄、符號連結、FIFO、裝置檔，或其他 |
| 內容 | 檔案的位元組，比雜湊，以及大小 |
| 權限 | mode 的低 12 位元，包括 setuid、setgid 與 sticky |
| 擁有者與群組 | 數字 ID |
| mtime | 到奈秒；由執行設定的、在最近一分鐘內的，兩邊算相同，因為兩次執行設定的時刻不同 |
| 符號連結目標 | 讀到的原樣 |
| 連結數 | 檔案的 |
| 裝置號碼 | 裝置檔的 |
| 結束狀態 | 分成 0、23、24 與錯誤：不比是哪個錯誤，rsync 拒絕參數時的 3，就是 Ferry 的 1 |

過程中說了什麼，訊息、警告與進度，不比：那是各自的事。

## 在哪裡跑

- **只在 Linux 上。** macOS 附的是 openrsync，不是使用者熟悉的 rsync。
- **有 rsync 的地方。** 沒有時，每個測試說它被略過，並算通過，除非 `FERRY_TEST_RSYNC` 是 `required`：CI 的 workflow 對每個 job 都設了它，所以 Linux 的 job 少了 rsync 會失敗，而不是略過。各 distribution 的容器不帶這個變數，在那裡略過。
- **有些要 root。** 掛載點要 root，有自己的掛載的 user namespace 就夠，例如在 `unshare -rm` 中；擁有者與裝置檔要整台機器的 root。不是的話，各自說它被略過。`make root-test` 經 sudo 跑它們，並設 `FERRY_TEST_ROOT` 為 `required`，所以不是 root 的會失敗，而不是沒跑就算過；CI 的 root job 跑它。讀不到的東西的情境，以 root 跑時略過，因為 root 什麼都讀得到。
- **本機兩個目錄之間。** 經 SSH 時跑的是同樣的 server，但沒有測試比較它們在那裡留下的結果。
- **很快。** 36 個測試在這台機器上花 5.5 秒，每個情境跑兩種方式，對照 rsync 3.2.7（2026-10-30）；只跑一種時是 2.8 秒。測試不會說它對照的是哪一版 rsync。

## 情境

| 類別 | 測試 | 內容 |
|---|---|---|
| 複製 | `an_empty_directory` | 空的 SOURCE |
| | `a_tree_of_every_kind` | 巢狀的檔案、空檔案、3 MiB 的檔案，相對、絕對與斷掉的符號連結，FIFO，空目錄 |
| | `names_of_every_byte` | 名稱有空白、換行、tab、反斜線、非 UTF-8 的位元組、開頭的 `-`、`*` |
| | `copy_keeps_what_the_destination_has_besides` | `copy` 保留 DESTINATION 有而 SOURCE 沒有的 |
| | `hard_links_are_two_files` | 不加 `-H` 時，同一個檔案的兩個名稱是兩個檔案 |
| 屬性 | `every_permission` | 600、755、444、setuid、setgid；sticky 目錄、setgid 目錄、裡面有檔案的唯讀目錄 |
| | `mtimes_to_the_nanosecond` | 檔案、目錄與符號連結的 |
| | `a_permission_alone_differing` | 同樣的檔案，不同的 mode |
| 重跑 | `a_rerun_with_nothing_changed` | |
| | `a_rerun_with_content_changed_its_size_kept` | 新的 mtime |
| | `a_rerun_with_a_permission_changed` | |
| | `a_rerun_with_an_mtime_changed` | |
| | `content_changed_in_place_its_size_and_mtime_kept` | 兩者都當成沒變 |
| | `mtimes_within_the_modify_window` | 五種：相差半秒，加與不加 `--modify-window=-1`；相差一秒；加 `--modify-window=2` 時相差兩秒與三秒 |
| 種類 | `a_directory_where_a_file_is` | |
| | `a_file_where_a_full_directory_is` | `copy` 與 `sync` |
| | `a_file_where_an_empty_directory_is` | |
| | `a_file_where_a_symlink_is` | |
| | `a_symlink_where_a_file_is` | |
| | `a_directory_where_a_symlink_to_one_is` | 換掉連結，絕不穿過它 |
| | `a_symlink_where_a_full_directory_is` | `copy` 與 `sync` |
| 刪除 | `sync_deletes_what_the_source_has_not` | 檔案、子樹、FIFO |
| | `sync_deletes_a_deep_tree` | 150 層深 |
| | `sync_deletes_in_and_under_read_only_directories` | |
| 讀取 | `an_unreadable_file` | `copy` 與 `sync`；不以 root 跑 |
| | `an_unreadable_directory` | `copy` 與 `sync`；不以 root 跑 |
| 根目錄 | `roots_of_every_kind` | 25 種：SOURCE 是檔案、目錄、指向檔案的符號連結、指向目錄的、或指向不存在的東西，對上 DESTINATION 是檔案、目錄、指向檔案的符號連結、指向目錄的、或不存在 |
| | `a_source_symlink_run_again` | 指向檔案的 SOURCE 符號連結，重跑一次，以及改指向別處之後再跑 |
| 要 root | `a_mount_point_crossed_or_not` | 加與不加 `-x` |
| | `sync_one_file_system_deletes_nothing_on_another` | `sync -x` |
| | `owners_and_devices` | 這裡沒有名稱的擁有者、setuid、字元裝置檔；加與不加 `--numeric-ids` |

## 刻意保留的不同

[概觀](overview.zh-TW.md#rsync-也做的事結果與-rsync-相同)列出它們與原因。其中五項對照 rsync 檢查，測試跑兩者，檢查各自照表上的做；兩項只檢查 Ferry 這一邊：

| 不同之處 | 由誰檢查 | 怎麼檢查 |
|---|---|---|
| SOURCE 結尾沒有斜線 | `deliberately_a_source_is_its_contents_without_a_slash` | 對照 rsync：rsync 建出 `DESTINATION/source/a`，Ferry 建出 `DESTINATION/a` |
| 把檔案複製到尚不存在的 `new/` | `deliberately_a_slash_on_a_missing_destination_means_nothing` | 對照 rsync：rsync 建出目錄 `new`，Ferry 建出檔案 |
| root 無法設定的擁有者 | `src/destination/tests.rs` 的 `an_owner_not_set_takes_setuid_and_setgid_away` | 只有 Ferry；rsync 怎麼做，是 2026-10-22 實際執行得知的 |
| DESTINATION 在 SOURCE 裡面 | `deliberately_overlapping_roots_are_refused` | 對照 rsync：rsync 把 SOURCE 複製進自己，Ferry 拒絕，結束狀態不是 0 |
| `sync` 遇到讀不到的目錄之後 | `deliberately_sync_deletes_past_what_it_could_not_read` | 對照 rsync：rsync 留下其餘的，Ferry 刪掉，只保留與讀不到的項目同名的；結束狀態相同；不以 root 跑 |
| `sync` 到 `/` 或家目錄 | `src/prepare/plan.rs` 的 `sync_asks_before_emptying_root_or_a_home`；`tests/remote.rs` 的 `syncing_into_a_hosts_home_is_refused_without_a_terminal` | 只有 Ferry |
| 還沒開始就被拒絕的傳輸 | `deliberately_a_refusal_exits_with_1` | 對照 rsync：目錄複製到檔案上，兩者都是錯誤，rsync 說 code 3，Ferry 說 DESTINATION 不是目錄 |

## 它找到的

寫測試之前，2026-10-22 先以 rsync 3.2.7 手動跑了 60 個情境，在 ext4 上以一般使用者跑，要 root 的在 `unshare -rm` 中跑。50 個相同，刻意保留的不同也都確認了；另有三個新的：

- **SOURCE 是指向檔案的符號連結。** rsync 複製連結本身；Ferry 原本跟隨它，複製目標檔案。現在照 rsync 複製連結，只跟隨指向目錄的連結（`ed7291b`）。
- **SOURCE 裡讀不到的路徑。** rsync 的結束狀態是 23，只有讀取期間消失的才用 24；Ferry 原本用 24。現在是 23（`ed7291b`）。
- **還沒開始就被拒絕的傳輸。** rsync 是 3，Ferry 是 1。保留，因為 Ferry 的 3 是 `ferry update --check` 發現新版。

測試在那之後才寫（`5332b2e`），`--modify-window` 那個則是草稿併入概觀時加的（`8c67679`）。為什麼 rsync 的行為要實際執行，而不憑記憶：2026-10-20 曾記下「擁有者設不了時，rsync 照留 setuid」，Ferry 當時也這樣做；2026-10-22 實際執行，rsync 卻不設權限與 mtime，檔案停在 `0700`。

## 沒有比較的

- Ferry 經 SSH 在 host 上留下的結果。
- 兩台機器之間依名稱對應的擁有者：這裡兩邊是同一台機器，`owners_and_devices` 的擁有者也沒有名稱。
- 設不了的擁有者，對照 rsync；見上表。
- 結束狀態 24，即讀取期間消失的路徑。
- dry run 列出的東西，因為說了什麼都不比；dry run 的目錄樹也沒有比。

## 未採用

- **以腳本跑兩者，而不是測試**：它不會和其他測試一起跑，CI 也不跑，比對又變回手動的。
- **隨機產生的目錄樹**：找得到沒人想到的情況，但失敗時難以判讀。表中的情況穩定之後，可以再加一個固定種子的版本。
- **對照 macOS 的 openrsync**：它不是使用者熟悉的 rsync。
