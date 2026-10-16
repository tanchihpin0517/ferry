# Ferry CLI

[English](cli.md)

`ferry` 指令目前的行為。[設計](design/overview.zh-TW.md)說明它的發展方向，[TODO.md](../TODO.md) 則說明其餘部分的實作順序。此處未描述的功能都尚未實作。

## 概要

```text
ferry copy [OPTIONS] SOURCE DESTINATION
ferry sync [OPTIONS] SOURCE DESTINATION
ferry test [OPTIONS] SOURCE DESTINATION
ferry update [--check | --version VERSION]
```

| 指令 | 讓 DESTINATION |
|---|---|
| `copy` | 包含 SOURCE 中的一切，並保留它原有的其他內容，如同 `rsync -a SOURCE/ DESTINATION/` |
| `sync` | 與 SOURCE 相同，刪除它原有的其他內容，如同 `rsync -a --delete SOURCE/ DESTINATION/` |

兩者都把 SOURCE 的內容複製到 DESTINATION 中，就像 SOURCE 以斜線結尾一樣，不論它是否真的如此：從 `data` 到 `backup`，`data/x` 會成為 `backup/x`，絕不會是 `backup/data/x`。Ferry 中不存在 rsync 對 `data` 與 `data/` 的區分。

`copy` 複製的內容與 `rsync -a` 相同：一般檔案、目錄、符號連結、FIFO、socket 與裝置，連同它們的權限（包括 setuid、setgid 與 sticky）、精確到奈秒的 mtime，以及擁有者。`sync` 做同樣的事，接著再從 DESTINATION 刪除 SOURCE 沒有的一切。

每個檔案都先寫到旁邊的暫存名稱，以來源的 BLAKE3 雜湊檢查，然後才重新命名就位，因此位於最終名稱的檔案一定是完整的。在應該是目錄或檔案的地方，若是符號連結、檔案或空目錄，會被取代。在應該是檔案的地方，若是有內容的目錄，`copy` 不會動它：該檔案失敗，其餘照常複製。`sync` 則會移除該目錄，連同其中的一切。

`sync` 只在其他一切都複製完成後才刪除，而且只在沒有任何失敗、且 SOURCE 全部可讀時才刪除。否則，SOURCE 清單中缺少的東西可能其實存在，因此 `sync` 什麼都不刪，並說明此事：

```text
ferry: warning: deleted nothing, since some of the source could not be read
```

刪除絕不跟隨符號連結：刪除的是連結本身，而不是它指向的東西。

讀取期間有變動的檔案，會在其餘檔案之後重送一次。若它再次變動，就會失敗。每個失敗的項目都會在失敗時回報，複製結束時會附上失敗的數量，並以結束狀態 1 結束：

```text
ferry: cannot copy notes.txt: a directory that is not empty is where this belongs
ferry: 1 entry could not be copied
```

擁有者依名稱對應，與 rsync 相同：來源會送出它使用的每個使用者與群組 ID 的名稱，僅送一次，目的地再把該名稱在當地的 ID 給予項目。在那裡不存在的名稱，或未送出的名稱，保留其數字；`--numeric-ids` 保留所有數字。使用者與群組 0 絕不對應：root 在任何地方都是 root。在 Linux 上，名稱以系統的 `getent` 查詢，因此找得到 LDAP 或 SSSD 中的帳號。

沒有 root 時，行程只能把自己的使用者，以及自己所屬的群組，給予項目。其他無法設定的部分，它會保持原樣，並針對每種情況警告一次，附上數量與幾個路徑：

```text
ferry: warning: 10 entries kept a group of this user's: giving them another needs root or membership in it (cli.md, assets/a.svg, …)
```

傳送的內容以 zstd 壓縮，frame 最大 2 MiB，不理會檔案的起訖位置，因此許多小檔案會一起壓縮。不會縮小的 frame，例如由已壓縮資料組成的 frame，會原樣送出。等級為 1，即最快的等級；`--compress-level` 可設定其他等級，`--no-compress` 則全部原樣傳送。

在檔案的暫存名稱被重新命名之前，其內容會先 `fsync`，而每個曾在其中建立東西的目錄會在最後 `fsync`，因此斷電絕不會留下沒有內容的檔名，而在 Ferry 完成後立刻斷電也不會遺失任何東西。檔案以批次重新命名，讓這個做法即使在大量小檔案時代價也很小。`--no-fsync` 會略過它。

在 macOS 上，`fsync` 只把資料交給磁碟機，磁碟機可能在自己的快取中重新排序。因此在每批重新命名之前，Ferry 會在磁碟機上設一道屏障（`F_BARRIERFSYNC`），並在傳輸完成時讓磁碟機清空快取一次（`F_FULLFSYNC`）。以這種方式複製 100,000 個小檔案約需 17 s，相較之下只用 `fsync` 需 15 s，但無法提供這項保證，而每個檔案都做 `F_FULLFSYNC` 則超過 500 s。

收到 Ctrl-C，或 `SIGTERM`、`SIGHUP` 時，未完成檔案的暫存檔會在 Ferry 結束前移除。已重新命名就位的檔案是完整的，因此下次執行只傳送缺少的部分。

沒有權限就無法建立的 socket 與裝置，也會以同樣的方式警告。無法設定的擁有者不算差異，因此不會讓項目每次執行都看起來有變動。

```text
ferry copy --dry-run ./data ./backup
ferry copy --dry-run ./data workbox:/scratch/data
ferry copy --dry-run ./data me@workbox:data
```

## 位置

位置是本機路徑或 host 上的路徑，寫法與 scp 和 rsync 相同：

| 寫法 | 意義 |
|---|---|
| `path`, `./path`, `/abs/path` | 本機路徑 |
| `host:path`, `user@host:path` | `host` 上的 `path`，經由 SSH |
| `host:` | `host` 上的家目錄 |
| `host:relative` | 相對於 `host` 上家目錄的 `relative` |

第一個 `/` 之前的冒號，會使其前面的部分成為 host。含有這種冒號的本機路徑，例如 `a:b`，要寫成 `./a:b`。host 不能以 `-` 開頭，因此絕不會被當成 `ssh` 的選項。`host` 可以是 `ssh` 接受的任何東西，包括 `~/.ssh/config` 中的 `Host` 別名。

SOURCE 或 DESTINATION，或兩者，都可以位於 host 上：複製到 host 是 push，從 host 複製是 pull，在兩個 host 之間則是遠端到遠端，資料會經過本機。無論兩側為何，`ferry` 本身都會印出任一側發生的問題，因此 pull 會回報 host 無法讀取的東西，就像 push 一樣。

## 比較什麼

SOURCE 是目錄或檔案。若是目錄，就比較它的內容與 DESTINATION 的內容，不論兩者名稱為何：`ferry copy ./data
./backup` 比較 `./data/x` 與 `./backup/x`。若是檔案，就與 DESTINATION 本身比較。

指定為 SOURCE 的符號連結會被跟隨。其他符號連結都不會：SOURCE 中的連結是一個獨立的項目，以連結的身分比較。DESTINATION 中的連結也絕不跟隨，因此經過連結的路徑底下的任何東西，都不算存在。

當 DESTINATION 缺少某個項目，或有不同的東西時，該項目就會被列出：

| SOURCE 有 | DESTINATION 有下列項目時，內容視為未變動 |
|---|---|
| 檔案 | 大小與 mtime 相同的檔案 |
| 目錄 | 目錄 |
| 符號連結 | 目標相同的符號連結 |
| FIFO 或 socket | 相同種類 |
| 裝置 | 種類與編號相同的裝置 |

檔案的比較方式與 rsync 的 quick check 相同：依大小與 mtime，而不讀取其內容。mtime 預設以整秒比較，與 rsync 相同，因為有些檔案系統與工具保存的時間比其他的粗：FAT 保存到 2 秒，HFS+ 到 1 秒，有些複製方式會捨去奈秒。在這些情況下比較奈秒，每次執行都會列出同樣那些未變動的檔案。`--modify-window` 可以改變這一點。

內容未變動的項目，若其權限或 Ferry 能設定的擁有者不同，仍會被列出；此時複製會設定它們，而不傳送任何內容。只有 mtime 不同的目錄、符號連結或特殊檔案不會被列出，但複製時會設定其 mtime。目錄的內容逐項比較，因此未變動的目錄底下，仍可能列出有變動的項目。

## 選項

`copy` 與 `sync` 接受相同的選項。

| 選項 | 作用 |
|---|---|
| `-n`, `--dry-run` | 列出將傳輸的項目，不複製任何東西 |
| `--compress-level LEVEL` | 以此 zstd 等級壓縮，從 1 到 19；未指定時為 1 |
| `--no-compress` | 不壓縮就傳送。在網路快到不需要壓縮時，可節省 CPU |
| `--progress` | 即使標準錯誤輸出不是終端機，也顯示進度與摘要；在終端機上本來就會顯示 |
| `--no-ui` | 在終端機上顯示進度列，而非儀表板 |
| `--profile[=FILE]` | 傳輸結束時，在標準錯誤輸出說明 client 與每個 server 的 thread 把時間花在哪裡；指定 `FILE` 時，另外把完整內容以 JSON 寫到那裡。見[效能剖析](design/profiling.zh-TW.md) |
| `--no-fsync` | 不對檔案與目錄執行 `fsync`。較快，但若不久後斷電，可能遺失已複製的內容 |
| `--numeric-ids` | 保留擁有者的數字使用者與群組 ID，而不依名稱對應。與 rsync 的同名選項相同 |
| `--modify-window SECONDS` | 把相差不超過 `SECONDS` 的 mtime 視為相同。預設的 `0` 比較整秒；`2` 適用於 FAT；`-1` 連奈秒也比較。與 rsync 的同名選項相同 |
| `-y`, `--yes` | 不經詢問就在 host 上安裝 Ferry |
| `--ssh-connections N` | 對每個 host 開啟 N 條 SSH connection，每一條都是獨立的 TCP 連線，並把傳輸分散到它們上面；見 [SSH connection](#ssh-connection)。預設：1 |
| `--remote-ferry PATH` | 在 host 上執行位於 `PATH` 的 Ferry 執行檔，而非 Ferry 安裝的 build。除非 `PATH` 是絕對路徑，否則它相對於 host 上的家目錄 |
| `-h`, `--help` | 印出說明；`--help` 印出的比 `-h` 多 |
| `-V`, `--version` | 印出版本 |

## 輸出

`--dry-run` 以固定的順序，每行一個項目寫到標準輸出：先是根目錄，然後是每個目錄依名稱排序的項目，深度優先。

- 根目錄是 `.`。
- 目錄以 `/` 結尾。
- 路徑相對於 SOURCE，以檔案系統保存的位元組寫出，因此不是 UTF-8 的名稱會原樣印出。

```text
$ ferry copy --dry-run ./data ./backup
.
a.txt
link
sub/
sub/b.txt
```

`sync --dry-run` 也會列出它將刪除的項目，每個目錄多出的項目排在該目錄其他項目之後，寫成 `deleting path`：

```text
$ ferry sync --dry-run ./data ./backup
deleting old.txt
deleting sub/stale/
```

未變動的目錄樹什麼都不印。

在終端機上，傳輸會顯示儀表板，一個就地更新的全螢幕畫面：進行到哪裡，以及剩餘時間；各項計數；每條 SSH connection 的速率，以及距上一次 heartbeat 抵達已過多久；管線中每個 stage 前的佇列有多滿，以及哪個 stage 拖住了其他 stage；以及最新的警告。Ctrl-C 會取消傳輸。傳輸結束時，終端機會恢復原狀，摘要以及其後的每則警告會寫到標準錯誤輸出。見[儀表板的設計](design/dashboard.zh-TW.md)。

使用 `--no-ui` 時，或在 dry run 中，標準錯誤輸出改為以一行就地更新的方式顯示進度，並在傳輸結束時顯示摘要：

```text
110,101 entries · 76,448 of 100,000 files · 74.7 KiB · 14.9 KiB/s
ferry: 110,101 entries compared, 100,000 files sent, 97.7 KiB in 5.4 s (18.2 KiB/s)
```

計入的項目是走訪過的項目；檔案則是內容已送出的檔案，在需要送出的檔案之中占多少。一旦壓縮有所節省，該行就會顯示節省了多少。走訪通常比傳送快，因此總數很早就會穩定。dry run 只顯示摘要，因為它的清單寫到標準輸出。在標準錯誤輸出不是終端機的地方，除非 `--progress` 要求，否則什麼都不顯示；此時會每隔幾秒寫出一行，而不是重繪。

錯誤與警告寫到標準錯誤輸出，以 `ferry:` 開頭。SOURCE 中無法讀取的路徑會在那裡回報、略過並計數。

按下 Ctrl-\、收到 `SIGQUIT`，或 server 之間 60 秒沒有任何東西經過時，標準錯誤輸出會說明 client 與每個 server 的每條 thread 停在哪裡，以及停了多久；傳輸照常進行。見[效能剖析](design/profiling.zh-TW.md#每條-thread-的快照)。

## 結束狀態

| 狀態 | 意義 |
|---|---|
| 0 | SOURCE 的每個項目都已讀取、比較，並在沒有 `--dry-run` 時已複製 |
| 1 | 有東西失敗，或 SOURCE 中有些路徑無法讀取 |
| 128 + 訊號 | 被中斷，例如 Ctrl-C 為 130 |

## 遠端 host

Ferry 執行系統的 `ssh`，因此 `~/.ssh/config` 中的一切都適用：別名、`User`、`Port`、`ProxyJump`、身分、agent。密碼或 OTP 會在終端機中詢問，就像 `ssh` 詢問的那樣。

在 host 上，client 執行一個與自己 build 完全相同的 `ferry server`。server 經由 SSH connection 與 client 通訊，並在 connection 關閉時結束。不同 build 的 server 會拒絕連線，並說明是哪兩個 build 相遇。

### 在 host 上安裝

當 host 沒有 client 的 build 時，client 會安裝它：

1. 它經由 SSH 探測 host 的 machine ID、作業系統、架構、已安裝的 build，以及 `cargo`。
2. 在從未安裝過 Ferry 的 host 上，它會先詢問。沒有終端機可詢問時，除非指定 `--yes`，否則拒絕。已經有其他 build 的 host 不會再詢問。
3. release build 會以 `curl` 從 GitHub Releases 下載自己版本、符合 host 平台的執行檔，以該 release 的 `SHA256SUMS` 檢查，保存在 `~/.ferry/releases/<version>/`，再經由 SSH 上傳。host 既不需要網路連線，也不需要工具鏈。release 支援 x86_64 與 aarch64 上的 Linux 與 macOS；其他 host 請使用 `--remote-ferry`。
4. dev build 會上傳其原始碼，並在 host 上以 host 的 `cargo` 建置，因此 host 需要 Rust 工具鏈。建置輸出會即時顯示。若 client 的原始碼在建置之後已有變動，client 會拒絕：請重新建置，再執行一次。

只有來自 GitHub release 的執行檔才是 release build。從原始碼建置的，即使用的是 `cargo install` 或 `--release`，也是 dev build。

host 保留最近使用的三個 build。使用或安裝一個 build 時，會移除其餘的。

Ferry 絕不改動 host 的 `PATH`，也絕不執行在那裡找到的 `ferry`。

### SSH connection

每個 host 都經由 SSH connection 連線，這些 connection 在那裡執行任何東西之前最先開啟，每一條都只登入，一次一條。密碼或 OTP 在那時詢問，絕不在傳輸期間詢問。

- **每個 host 登入一次。** 若你的 SSH 設定為該 host 設定了 `ControlMaster`（如 `ssh -G` 所示），第一條 SSH connection 會共用你自己的，否則共用 Ferry 自己的。無論哪一種，之後對該 host 的 `ssh`，不論來自同一次執行，或接下來十分鐘內的執行，都會重複使用它，而不必再次登入。
- **`--ssh-connections N` 開啟 N 條。** 第一條之後的每一條都有 Ferry 自己的 `ControlMaster`，因此每一條都是獨立的 TCP 連線，傳輸會分散到所有連線上；在會掉封包的路徑上，多條 TCP 連線合起來比一條快。每一條都可能需要自己登入：先嘗試不詢問，這在使用金鑰時可行，否則在終端機中詢問，並說明是為哪一條 connection 詢問。沒有終端機時，或登入失敗時，執行會在複製任何東西之前停止，並說明是哪條 SSH connection 以及原因。Ferry 絕不會默默以少於 N 條的連線傳輸。
- **任何一條 SSH connection 中斷都會使傳輸失敗**，並說明是哪一條。每一條都每 5 秒雙向傳送 heartbeat，因此陷入沉默的連線，例如網路斷了或筆電進入睡眠時，會在 30 秒內被發現，而關閉的連線則會立即被發現。重新執行會略過已完成的檔案。

## 檔案

release build 與 dev build 絕不共用以下任何東西：release build 使用以下名稱，dev build 則使用加上 `-dev` 的相同名稱，例如 `~/.ferry-dev/`。

| 所在 | 路徑 | 存放內容 |
|---|---|---|
| client | `/tmp/ferry-<uid>/ssh/` | 共用 SSH connection 的 socket，模式 0700 |
| client、host | `/tmp/ferry-<uid>/join/` | server 執行期間，其 path 用來加入它的 socket，模式 0700 |
| client | `~/.ferry/releases/<version>/` | 下載的 release 執行檔，以及它們的 `SHA256SUMS` |
| host | `~/.ferry/hosts/<machine-id>/bin/<build>/ferry` | 每個已安裝的 build |
| host | `~/.ferry/hosts/<machine-id>/src/`, `build/` | dev build 的原始碼與保留的 target 目錄 |

machine ID 用來區隔共用家目錄的叢集節點。沒有 machine ID（`/etc/machine-id`，或 macOS 上的 `IOPlatformUUID`）的 host 會被拒絕。

## 環境

| 變數 | 作用 |
|---|---|
| `FERRY_HOME` | Ferry 的目錄，取代 `~/.ferry`。必須是絕對路徑。在 host 上，於 SSH 提供給指令的環境中設定 |
| `HOME` | 未設定 `FERRY_HOME` 時，`~/.ferry` 所在之處 |
| `FERRY_SSH` | 取代 `ssh` 執行的程式，傳入相同的引數，如同 rsync 的 `RSYNC_RSH` |

## `ferry update`

```text
ferry update [--check | --version VERSION]
```

把這個執行檔換成最新的 release，或換成 VERSION，不論較舊或較新。它從 GitHub 的 `releases/latest` 轉向的位置找出最新的 release，以 `curl` 下載符合這台機器的執行檔，以該 release 的 `SHA256SUMS` 檢查，並與 host 用的一樣保存在 `~/.ferry/releases/<version>/`，因此之後要安裝到同平台的 host 時不必再下載。執行檔是在它所在的目錄中以改名的方式替換，因此更新失敗時，舊的執行檔會完整保留。若該目錄不可寫入，更新會停止並說明原因。

Ferry 絕不自行檢查是否有新的 release；只有 `ferry update` 會。host 不需要任何處理：下一次傳輸會在那裡安裝新的 build，與任何 build 一樣。

dev build（從原始碼建置的）會拒絕：請從原始碼更新。

| 選項 | 效果 |
|---|---|
| `--check` | 只說明是否有較新的 release：有則以 2 結束，沒有則以 0 結束，檢查失敗則以 1 結束 |
| `--version VERSION` | 改為更新到 VERSION，例如 `0.1.0` |

## `ferry test`

```text
ferry test [--yes] [--remote-ferry PATH] SOURCE DESTINATION
```

在它所建立的目錄樹上，執行從 SOURCE 到 DESTINATION 的完整流程，並在之後移除這些目錄樹。SOURCE 與 DESTINATION 是目錄，可以是本機的或 `[user@]host:path`，也可以是同一個目錄。測試在其中建立 `ferry-test-<id>-source` 與 `ferry-test-<id>-destination`，並且只移除這兩者：移除時會拒絕沒有 `.ferry-fixture` 標記的目錄，而每個測試目錄樹都有這個標記。

```text
$ ferry test /tmp workbox:/tmp
ferry test: from /tmp/ferry-test-aecd29714c78-source to workbox:/tmp/ferry-test-aecd29714c78-destination
phase 1: prepare
  ok    workbox: ready (0.41s)
  ok    copy: a missing source fails before anything changes (0.01s)
  ok    copy: a server that does not start fails before anything changes (0.04s)
  …
phase 2: transfer
  ok    copy: a missing destination needs everything (0.12s)
  ok    copy: an identical destination needs nothing (0.10s)
  …
  ok    sync: a directory in the way fails one file, unless syncing (0.13s)
  ok    sync: other permissions are restored (0.11s)
37 passed, 0 failed, 0 skipped
```

來源目錄樹包含一個檔案、巢狀目錄、一個空目錄、一個符號連結、一個懸空的符號連結，以及一個 FIFO，內容與 mtime 都是固定的，精確到奈秒。每個情境都在目的地放置一棵已知的目錄樹，在兩處之間執行真正的 `ferry` 指令，並檢查它印出的內容。每個情境都會以每個傳輸指令，即 `copy` 與 `sync`，各執行一次。

情境分屬一次執行的兩個階段（見[管線](design/pipeline.zh-TW.md)）：

1. **準備。** 首先讓每個 host 就緒，並像傳輸時一樣在其上安裝 Ferry。接著是必須在準備期間失敗的情境，例如來源不存在，或 server 無法啟動：每一個都必須以正確的錯誤失敗，並讓目的地的目錄樹完全保持原樣。需要 host 上 server 的情境，在兩側都位於本機時會略過。
2. **傳輸。** 進行比較與複製的情境。只有在第一階段沒有任何失敗時才會執行。

需要尚未實作之功能的情境會略過，並說明原因。一旦某個指令被略過，它的其他情境也會一併略過。情境隨 Ferry 一同成長：每一步都會把自己的情境加到它所屬的階段。

在 host 上，測試目錄樹由 Ferry 自己透過隱藏的 `ferry fixture` 子指令建立與移除，因此 host 除了 SSH 之外什麼都不需要。`--yes` 與 `--remote-ferry` 的意義與在傳輸中相同。

沒有任何情境失敗時，結束狀態為 0，否則為 1。

## `ferry server`

一個隱藏的子指令：傳輸的一側。client 為每一側啟動一個，在 host 上經由 SSH，或在本機上，並在兩者之間轉送。它的標準輸入與輸出就是它的 control connection：它在那裡交握，並在它保持開啟期間持續存在。接著它在加入它的 path 上執行其 peer：`ferry server --join <token>`，它透過 Unix socket 把自己的標準輸入與輸出交給 server，並等到 server 完成。兩者都不是供人手動執行的。
