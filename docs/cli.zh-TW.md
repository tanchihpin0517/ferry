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

兩者都把 SOURCE 的內容複製到 DESTINATION 中，就像 SOURCE 以斜線結尾一樣，不論它是否真的如此：從 `data` 到 `backup`，`data/x` 會成為 `backup/x`，絕不會是 `backup/data/x`。Ferry 中不存在 rsync 對 `data` 與 `data/` 的區分。DESTINATION 結尾的斜線同樣沒有意義：把檔案複製到尚不存在的 `new/`，結果是檔案 `new`；rsync 則會把 `new` 建成目錄，再把檔案複製進去。

`copy` 複製的內容與 `rsync -a` 相同：一般檔案、目錄、符號連結、FIFO、socket 與裝置，連同它們的權限（包括 setuid、setgid 與 sticky）、精確到奈秒的 mtime，以及擁有者。`sync` 做同樣的事，接著再從 DESTINATION 刪除 SOURCE 沒有的一切。

每個檔案都先寫在看不到的地方，以來源的 BLAKE3 雜湊檢查，然後才取得它的名稱，因此位於最終名稱的檔案一定是完整的：在 Linux 上，檔案系統支援時，它在那之前沒有名稱；其他情況，例如 macOS 或 NFS，它寫到旁邊的暫存名稱，再重新命名就位。在應該是目錄或檔案的地方，若是符號連結、檔案或空目錄，會被取代。在應該是檔案的地方，若是有內容的目錄，`copy` 不會動它：該檔案失敗，其餘照常複製。`sync` 則會移除該目錄，連同其中的一切，dry run 會把它列為 `deleting name/`。

SOURCE 與 DESTINATION 本身會在任何變動之前先檢查。`sync` 只能從目錄到目錄，或到尚不存在的 DESTINATION；SOURCE 是檔案時會被拒絕，並提示改用 `copy`。`copy` 拒絕把目錄複製到目錄以外的東西上；把檔案複製到目錄上時，與 rsync 相同，會把它放進目錄裡，成為 `DESTINATION/NAME`。

SOURCE 與 DESTINATION 不可重疊。DESTINATION 若就是 SOURCE、在 SOURCE 裡面，或包含 SOURCE，`copy` 與 `sync` 都會拒絕，dry run 也一樣：`ferry sync a/sub a` 會刪掉 Source，`ferry copy a a/sub` 會把它複製進自己裡面。兩者依 kernel 眼中的 device 與 inode 比較，所以 symlink 或同一目錄的其他寫法也一樣會被拒絕。傳輸期間會再檢查一次，所以若有 mount 讓其中一個出現在另一個裡面，傳輸會停止。共用網路檔案系統（例如 NFS）的兩台 host 目前不會比較：一台上的 DESTINATION 若在另一台上的 SOURCE 裡面，不會被發現。

`sync` 在刪除 `/`、某個家目錄，或包含家目錄的目錄（例如 `/home`，或單獨的 `host:`，也就是 `host` 上的家目錄）裡的東西之前，會先詢問：這些很容易打錯，而在那裡 sync 會刪掉 SOURCE 沒有的一切。沒有終端機可以詢問時，除非給了 `--allow-dangerous-destination`，否則拒絕；`--yes` 不算同意。dry run 只警告。`copy` 不刪除任何東西，所以不會詢問。mount point（例如 `/mnt/backup`）也不會詢問。

`sync` 邊走邊刪，一次一個目錄，比較完一個目錄就刪除它多出來的項目，與 rsync 的預設相同。SOURCE 列得出、但讀不到的項目仍在它的清單中，標記為無法讀取：DESTINATION 中同名的項目保持原樣，既不覆寫也不刪除，並回報該路徑。SOURCE 無法列出的目錄沒有任何清單，因此其中也什麼都不刪。

中途停止的傳輸可能讓 DESTINATION 比原本少：已經刪除的，還沒有東西取代，例如檔案從一個較早比較的目錄搬到一個較晚比較的目錄。SOURCE 永遠不會被更動；再執行一次 `sync` 就會補齊。

刪除絕不跟隨符號連結：刪除的是連結本身，而不是它指向的東西。

檔案最多讀到開啟時的大小，與 rsync 相同：讀取期間多出來的部分留給下一次執行，那時會發現它變了。讀取期間被就地改寫、大小不變的檔案，只有在 mtime 的變化超過 `--modify-window` 時，下一次執行才會發現：在預設下，與開啟時的 mtime 落在同一秒內的改寫不會被發現，副本保留讀到的內容，新舊混雜，與 rsync 相同；`--modify-window -1` 會比較到奈秒。讀取期間變小，或讀取失敗的檔案會失敗；不會重送。每個失敗的項目都會在失敗時回報，複製結束時會附上失敗的數量，並以結束狀態 23 結束，與 rsync 的部分傳輸相同：

```text
ferry: cannot copy notes.txt: a directory that is not empty is where this belongs
ferry: 1 entry could not be copied
```

擁有者依名稱對應，與 rsync 相同：來源會送出它使用的每個使用者與群組 ID 的名稱，僅送一次，目的地再把該名稱在當地的 ID 給予項目。在那裡不存在的名稱，或未送出的名稱，保留其數字；`--numeric-ids` 保留所有數字。使用者與群組 0 絕不對應：root 在任何地方都是 root。在 Linux 上，名稱以系統的 `getent` 查詢，因此找得到 LDAP 或 SSSD 中的帳號。

沒有 root 時，行程只能把自己的使用者，以及自己所屬的群組，給予項目。其他無法設定的部分，它會保持原樣，不發出警告，與 rsync 相同：以另一個使用者、且不是 root 的身分複製到 host 時，檔案會屬於該使用者；群組在該使用者屬於來源群組時沿用來源的群組，否則為該使用者自己的群組。

要在 host 上保留所有擁有者，可以用 root 登入，或加上 `--remote-sudo`：以一般使用者登入，host 上的 server 再透過 `sudo` 以 root 執行。探測與安裝仍以登入的使用者進行，因此 Ferry 照常安裝在其家目錄。若 `sudo` 需要密碼，Ferry 會在本機的終端機詢問一次（不顯示輸入），或從 `FERRY_SUDO_PASSWORD` 取得，先在 host 上確認無誤，再交給每個 server 的 `sudo` 從標準輸入讀取（`sudo -k -S`），因此密碼不會出現在命令列上。本機這一側不受影響：要在本機以 root 執行，請用 `sudo ferry`。host 的 sudo 若設定 `requiretty`，則無法使用。

傳送的內容以 zstd 壓縮，每個 block 最大 2 MiB，各自壓縮：一個 block 裝許多小檔案，讓它們一起壓縮，或是大檔的一段。不會縮小的 block，例如由已壓縮資料組成的 block，會原樣送出。等級為 1，即最快的等級；`--compress-level` 可設定其他等級，`--no-compress` 則全部原樣傳送。兩側都在本機時，沒有東西經過網路，所以不壓縮，除非以 `--compress-level` 要求。

Ferry 不對寫入的東西 `fsync`，與 rsync 沒給 `--fsync` 時相同：系統自己決定何時寫到儲存媒體，Linux 預設約在半分鐘內。傳輸後不久斷電或系統當機，可能遺失它寫的東西：有名稱的檔案可能是空的，或只有部分內容，被它取代的舊檔也已不在。Ferry 自己結束，不論怎麼結束，都不會這樣遺失：它寫的東西在系統的快取裡。NFS 上關檔時會等 server 存好，關檔失敗的檔案就算失敗，例如 server 的磁碟滿了，與 rsync 相同；之後寫回時才遇到的錯誤，例如磁碟故障，不會被看到，檔案照樣算複製成功。若傳輸後立刻斷電也不能遺失，傳輸完在 Destination 的機器上執行 `sync`：在 Linux 上，它會等到一切都寫到儲存媒體。`--fsync` 還沒實作，給了會被拒絕。

收到 Ctrl-C，或 `SIGTERM`、`SIGHUP` 時，Ferry 會停下：每個 server，不論在本機或在每個 host 上，都被告知停下、移除未完成檔案的暫存檔，然後結束；Ferry 等它們，等超過一秒就說出還在等什麼，以及它們從外面看起來怎樣；3 秒內的第二次 Ctrl-C 立刻停下，之後的則再說一次在等什麼。接著 Ferry 為了撐住每條 SSH connection 而啟動的 `ssh` 也會結束；傳輸失敗時也一樣，例如 Source 失敗或 connection 中斷。Ferry 被 `SIGKILL` 終止時，server 照樣發現 Ferry 不在了而停下、收拾：在本機立刻，在 host 上則在它的連線結束時，或沉默 30 秒之後；只有撐住連線的 `ssh` 會留著。沒有名稱寫入的檔案，即使 Ferry 被終止，也不會留下任何東西。已經就位的檔案是完整的，因此下次執行只傳送缺少的部分。被暫停的 server，例如被 Ctrl-Z 或 `SIGSTOP`，會被說出來並等它。

沒有權限就無法建立的 socket 與裝置會被略過，並警告一次，附上數量與幾個路徑：

```text
ferry: warning: 2 entries were not created: creating sockets and devices needs privilege (dev/null, run/app.sock)
```

同樣每種一則警告的，還有無法設定的擁有者、權限或 mtime，例如別人擁有的項目、root 被 squash 的地方、FAT 上，或在容器中對應不到 ID 的擁有者：項目照樣複製。root 無法設定擁有者的項目，會拿掉 setuid；無法設定群組的，會拿掉 setgid，並另有一則警告：否則它在這裡會以 root 的身分執行，而 Source 上並沒有這樣的權限。rsync 這時則完全不設定權限與 mtime。無法設定的不算差異，因此不會讓項目每次執行都看起來有變動。

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
| `[address]:path`, `user@[address]:path` | 位於 IPv6 位址的 host 上的 `path`，寫法與 scp 和 rsync 相同 |
| `host:` | `host` 上的家目錄；`sync` 會先詢問 |
| `host:relative` | 相對於 `host` 上家目錄的 `relative` |
| `host:~`、`host:~/relative` | 與 `host:` 和 `host:relative` 相同，和 scp、rsync 的解讀一致 |
| `host:./~/relative` | `host` 上家目錄裡一個名為 `~` 的目錄 |

第一個 `/` 之前的冒號，會使其前面的部分成為 host。含有這種冒號的本機路徑，例如 `a:b`，要寫成 `./a:b`。其他使用者的家目錄 `host:~user/path` 目前還不支援，會被拒絕：請寫出完整路徑。本機路徑中的 `~` 由你的 shell 展開，在 Ferry 看到之前就展開了。host 不能以 `-` 開頭，因此絕不會被當成 `ssh` 的選項。`host` 可以是 `ssh` 接受的任何東西，包括 `~/.ssh/config` 中的 `Host` 別名。

SOURCE 或 DESTINATION，或兩者，都可以位於 host 上：複製到 host 是 push，從 host 複製是 pull，在兩個 host 之間則是遠端到遠端，資料會經過本機。無論兩側為何，`ferry` 本身都會印出任一側發生的問題，因此 pull 會回報 host 無法讀取的東西，就像 push 一樣。

## 比較什麼

SOURCE 是目錄或檔案。若是目錄，就比較它的內容與 DESTINATION 的內容，不論兩者名稱為何：`ferry copy ./data
./backup` 比較 `./data/x` 與 `./backup/x`。若是檔案，就與 DESTINATION 本身比較。

指定為 SOURCE 的符號連結，若指向目錄就會被跟隨，與 rsync 進入 `SOURCE/` 相同；指向檔案或不存在的東西時，則以符號連結本身複製，與 rsync 相同。其他符號連結都不會被跟隨：SOURCE 中的連結是一個獨立的項目，以連結的身分比較。DESTINATION 中的連結也絕不跟隨，因此經過連結的路徑底下的任何東西，都不算存在。DESTINATION 本身若是指向目錄的符號連結，會進入那個目錄；指向其他東西的符號連結則被取代，與 rsync 相同，它指向的東西保持原樣。

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
| `-0`, `--null` | 與 `--dry-run` 一起使用：每個項目寫成 `+ PATH`，要刪除的寫成 `- PATH`，位元組原樣，以 NUL 結尾，給腳本使用 |
| `--compress-level LEVEL` | 以此 zstd 等級壓縮，從 1 到 19；未指定時為 1。指定時，兩側都在本機的複製也會壓縮，否則不壓縮 |
| `--no-compress` | 不壓縮就傳送。在網路快到不需要壓縮時，可節省 CPU |
| `--progress` | 即使標準錯誤輸出不是終端機，也顯示進度與摘要；在終端機上本來就會顯示 |
| `--no-ui` | 在終端機上顯示進度列，而非儀表板 |
| `--profile` | 在摘要之後，說明 client 與每個 server 的 thread 把時間花在哪裡：是哪一側、哪個 stage 拖住了傳輸。見[效能剖析](design/profiling.zh-TW.md) |
| `--profile-file FILE` | 另外把 client 與每個 server 的 thread 把時間花在哪裡，以 JSON 寫到 `FILE`，包含每個狀態的 histogram。見[效能剖析](design/profiling.zh-TW.md) |
| `--fsync` | 還沒實作：會被拒絕，結束狀態 2。Ferry 不對寫入的東西 `fsync`，見[概要](#概要) |
| `--no-fsync` | 沒有作用，因為本來就不 fsync：仍然接受，讓為 Ferry 0.16 以前寫的命令列照樣能跑（那時不給它就會 fsync）。rsync 沒有這個選項 |
| `--numeric-ids` | 保留擁有者的數字使用者與群組 ID，而不依名稱對應。與 rsync 的同名選項相同 |
| `-x`, `--one-file-system` | 停留在每個根目錄所在的檔案系統上。掃描不進入另一個檔案系統上的目錄（mount point）：mount point 本身以目錄複製，但不複製其中的內容，Destination 在其中已有的東西保持原樣。`sync` 不刪除 Destination 根目錄以外的檔案系統上的任何東西，也不刪除這種目錄本身；包含這種目錄的目錄會連同它一起保留，其中其他的東西照常刪除，並發出警告。dry run 仍會把這個目錄列為要刪除。不加 `-x` 時，掃描與刪除都會進入 mount point，與 rsync 相同。與 rsync 只給一次的 `-x` 相同。rsync 的 `-xx` 連 mount point 本身也不送，因此 `sync` 會刪除 Destination 中同名的東西；這個沒有做，`-x` 給兩次會被拒絕 |
| `--modify-window SECONDS` | 把相差不超過 `SECONDS` 的 mtime 視為相同。預設的 `0` 比較整秒；`2` 適用於 FAT；`-1` 連奈秒也比較；不接受其他負數。與 rsync 的同名選項相同 |
| `-y`, `--yes` | 不經詢問直接同意在 host 上安裝 Ferry |
| `--allow-dangerous-destination` | 即使 DESTINATION 是 `/`、某個家目錄，或包含家目錄的目錄（例如單獨的 `host:`），也不經詢問直接 sync；見[概要](#概要)。`--yes` 不算同意這一項 |
| `--remote-sudo` | 在 host 上透過 `sudo` 以 root 執行 Ferry，以便保留所有擁有者。與 rsync 的 `--rsync-path="sudo rsync"` 相同；見下文 |
| `--cpus N\|all` | 在每台機器上最多只用 N 個核心，包括 client 與每個 host，或用全部的核心；見[核心](#核心)。預設：8 |
| `--remote-ferry PATH` | 在 host 上執行位於 `PATH` 的 Ferry 執行檔，而非 Ferry 安裝的 build。除非 `PATH` 是絕對路徑，否則它相對於 host 上的家目錄 |
| `--remote-join` | 讓每條連到 host 上 monitor 的連線（其 server 的 control connection 也在其中），以及 stream，都透過 host 上的 shell 執行 Ferry 來加入，而不是經由 SSH 連到它：用於 `sshd` 既不肯轉發到 Unix socket、也不肯轉發到 port 的 host（`AllowStreamLocalForwarding no` 與 `AllowTcpForwarding no`）。只肯轉發到 port 時，不用這個選項，Ferry 也會改連 loopback 上的一個 port。這樣的每條連線都要一個 session，shell 啟動檔留在背景的程式也可能寫進去；見[架構](design/architecture.zh-TW.md#connection) |
| `-h`, `--help` | 印出說明；`--help` 印出的比 `-h` 多 |
| `-V`, `--version` | 印出版本 |

## 輸出

`--dry-run` 以固定的順序，每行一個項目寫到標準輸出：先是根目錄，然後是每個目錄依名稱排序的項目，深度優先。在終端機上，由儀表板顯示執行狀況，清單不會寫到終端機，以免成千上萬行把畫面洗掉：儀表板會計數並顯示最近幾筆，結束時說明共有幾項。要看清單，請把標準輸出導向別處，例如 `> file` 或 `| less`；或加上 `--no-ui`，讓清單一找到就寫到終端機。

- 根目錄是 `.`。
- 目錄以 `/` 結尾。
- 路徑相對於 SOURCE。終端機會執行的位元組（例如換行或 ESC），以及不是 UTF-8 的位元組，寫成 `\#` 加三位八進位數字，與 rsync 相同；名稱以 `deleting ` 開頭時，第一個位元組也這樣寫，因此任何名稱都偽造不了一行輸出。`-0` 則把每個項目寫成 `+ PATH` 或 `- PATH` 加 NUL，位元組原樣。
- 讀取端提早停止時，例如 `head`，清單會安靜地結束，結束狀態為 0。

```text
$ ferry copy --dry-run ./data ./backup
.
a.txt
link
sub/
sub/b.txt
```

`sync --dry-run` 也會列出它將刪除的項目，寫成 `deleting path`，每個目錄多出的項目排在該目錄其他項目之前，與實際執行的順序相同：實際執行時，會先刪除目錄裡多出的項目，才複製任何東西進去，與 rsync 相同，因此複製進去的東西絕不會被當成多餘的：

```text
$ ferry sync --dry-run ./data ./backup
deleting old.txt
deleting sub/stale/
```

未變動的目錄樹什麼都不印。

在終端機上，傳輸會顯示儀表板，一個就地更新的全螢幕畫面：進行到哪裡，以及剩餘時間；各項計數；每條 SSH connection 的速率，以及距上一次有東西到達已過多久；管線中每個 stage 前的佇列有多滿，以及哪個 stage 拖住了其他 stage；以及最新的警告。Ctrl-C 會取消傳輸。傳輸結束時，終端機會恢復原狀，摘要以及其後的每則警告會寫到標準錯誤輸出。見[儀表板的設計](design/dashboard.zh-TW.md)。

傳輸開始之前，一旦某個步驟花了一點時間，標準錯誤輸出會有一行說明 client 正在做什麼，以及已經花了多久：登入 host、檢查 host 上的 Ferry、下載並上傳要安裝的 release，或啟動某一側的 server。安裝到 host 的 dev build 會在那裡以 cargo 建置，此時改為顯示 cargo 自己的進度：

```text
ferry: checking Ferry on fatcat… 4 s
ferry: uploading Ferry to fatcat: 3.1 MiB of 8.2 MiB… 2 s
```

使用 `--no-ui` 時，標準錯誤輸出改為以一行就地更新的方式顯示進度，並在傳輸結束時顯示摘要：

```text
110,101 entries · 76,448 of 100,000 files · 74.7 KiB · 14.9 KiB/s
ferry: 110,101 entries compared, 100,000 files sent, 97.7 KiB in 5.4 s (18.2 KiB/s)
```

計入的項目是掃描過的項目；檔案則是內容已送出的檔案，在需要送出的檔案之中占多少。一旦壓縮有所節省，該行就會顯示節省了多少。掃描通常比傳送快，因此總數很早就會穩定。dry run 的這一行，因為可能很長一段時間什麼都不列出，改為顯示比較進行到哪裡，以及是什麼拖慢了它：執行緒最忙的那一側，以及它在做什麼，例如 `312,450 compared · 12 to send · 1:42 · Source on fatcat: list directories`。它的清單寫到標準輸出，在終端機上一找到就寫出，寫出前會先清掉該行。在標準錯誤輸出不是終端機的地方，除非 `--progress` 要求，否則什麼都不顯示；此時會每隔幾秒寫出一行，而不是重繪。

加上 `--profile` 時，摘要之後是時間花在哪裡：對 client 與每個 server，其 thread 的每個角色一行，附上它把時間花在哪些狀態。它顯示是哪一側、哪個 stage 拖住了傳輸。見[效能剖析](design/profiling.zh-TW.md#剖析結果)。

錯誤與警告寫到標準錯誤輸出，以 `ferry:` 開頭。SOURCE 中無法讀取的路徑會在那裡回報、略過並計數。列出之後、讀取之前就消失的路徑，會發出警告並略過，不算失敗：`sync` 會刪除 DESTINATION 中對應的項目。DESTINATION 中比較不了、寫不進或收不了尾的項目單獨失敗，其餘照常複製；但 DESTINATION 的磁碟滿了、配額用盡或唯讀時，傳輸立刻停止，因為之後的每個項目都會同樣失敗。

按下 Ctrl-\、收到 `SIGQUIT`，或 60 秒都沒有任何進展、兩個 server 回報的計數都沒有變動時，標準錯誤輸出會說明 client 與每個 server 的每條 thread 停在哪裡，以及停了多久；傳輸照常進行。見[效能剖析](design/profiling.zh-TW.md#每條-thread-的快照)。

## 結束狀態

| 狀態 | 意義 |
|---|---|
| 0 | SOURCE 的每個項目都已讀取、比較，並在沒有 `--dry-run` 時已複製，但警告的部分除外，例如需要權限的裝置檔，或無法設定的擁有者 |
| 1 | 傳輸失敗，或根本沒開始：連不上 host、兩邊的根目錄重疊、磁碟已滿 |
| 2 | 命令列有誤：未知的選項、缺少參數，或還沒實作的選項，例如 `--fsync` |
| 3 | `ferry update --check`：有較新的 release |
| 23 | 有些項目無法複製或刪除，或 SOURCE 中有些路徑無法讀取，其餘的都完成了，與 rsync 的部分傳輸相同 |
| 24 | 每個項目都已複製，但 SOURCE 中有些路徑在讀取時消失，與 rsync 的「來源檔案消失」相同 |
| 101 | 內部錯誤：Ferry 的 bug，會有一行以 `ferry: internal error` 開頭的訊息說明 |
| 128 + 訊號 | 被中斷，例如 Ctrl-C 為 130 |

結束狀態 2、3、23 與 24 是這一版新增的：之前後三者分別是 2、1、1。

## 遠端 host

Ferry 執行系統的 `ssh`，因此 `~/.ssh/config` 中的一切都適用：別名、`User`、`Port`、`ProxyJump`、身分、agent、`ConnectTimeout`（沒設時為 10 秒）。密碼或 OTP 會在終端機中詢問，就像 `ssh` 詢問的那樣。只有會破壞 Ferry 自己 session 的設定不採用：它們不執行 `RemoteCommand` 與 `LocalCommand`，也不建立轉發，否則 Ferry 的每個 master 都會再綁一次轉發埠，並一直佔著直到 master 結束。host 上的登入 shell 是什麼都可以，包括 csh、tcsh 與 fish：Ferry 的指令以單行、只含一般字元的形式送到登入 shell，再由它交給 `sh`。

在 host 上，client 開一個 session，它探測 host、需要時安裝 Ferry，然後成為 Ferry 在那裡的 monitor。monitor 為那裡的每一側啟動一個與 client build 完全相同的 `ferry server`，在 SSH connection 上一條不跑 shell 的 channel 上，並看著它；server 在上面與 client 通訊，並在它關閉時結束。不同 build 的 server 會拒絕連線，並說明是哪兩個 build 相遇。

### 在 host 上安裝

當 host 沒有 client 的 build 時，client 會安裝它，在 host 唯一的 session 裡、它成為 Ferry 的 monitor 之前：

1. 它經由 SSH 探測 host 的 machine ID、作業系統、架構、已安裝的 build、`cargo`，以及能用什麼下載並核對 release：`curl` 或 `wget`，`sha256sum` 或 `shasum`。這次執行的每個 host 都先探測，需要安裝的都先詢問，再安裝，然後才啟動任何 server。
2. 在從未安裝過 Ferry 的 host 上，它會先詢問。沒有終端機可詢問時，除非指定 `--yes`，否則拒絕。已經有其他 build 的 host 不會再詢問。
3. release build 由 host 自己下載：從 GitHub Releases 以 `curl` 或 `wget` 下載自己版本、符合 host 平台的執行檔，並以該 release 的 `SHA256SUMS` 為它列的 SHA-256 核對；`SHA256SUMS` 由 client 抓來，保存在 `~/.ferry/releases/<version>/`。host 下載不了、或沒辦法核對時，改由 client 自己下載執行檔，以同一份清單檢查，保存在那裡，再經由 SSH 上傳，所以 host 既不需要網路連線，也不需要工具鏈。release 支援 x86_64 與 aarch64 上的 Linux 與 macOS；其他 host 請使用 `--remote-ferry`。host 只在下載或上傳的檔案 SHA-256 相符（上傳的還要大小相符）、而且能在那裡執行時才安裝它：被截斷的不會安裝任何東西，下次執行會重新安裝。
4. dev build 會上傳其原始碼，並在 host 上以 host 的 `cargo` 建置，因此 host 需要 Rust 工具鏈。建置輸出會即時顯示。若 client 的原始碼在建置之後已有變動，client 會拒絕：請重新建置，再執行一次。host 建出的執行檔必須就是 client 的 build 才會安裝。同一個 host 上的兩個 dev 安裝會依序建置。

只有來自 GitHub release 的執行檔才是 release build。從原始碼建置的，即使用的是 `cargo install` 或 `--release`，也是 dev build。

host 保留最近使用的三個 build。使用或安裝一個 build 時，會移除其餘的，但最近一小時內用過的不會移除。host 上若沒有能判斷使用時間的 `find`，就只保留三個，其餘都移除。

Ferry 絕不改動 host 的 `PATH`，也絕不執行在那裡找到的 `ferry`。

### SSH connection

每個 host 都經由一條 SSH connection 連線，它在那裡執行任何東西之前最先開啟，而且只登入。密碼或 OTP 在那時詢問，絕不在傳輸期間詢問。

- **每個 host 登入一次。** 若你的 SSH 設定為該 host 設定了 `ControlMaster`（如 `ssh -G` 所示），SSH connection 就是你自己的，否則是 Ferry 自己的。無論哪一種，之後對該 host 的 `ssh`，不論來自同一次執行，或接下來十分鐘內的執行，都會重複使用它，而不必再次登入。登入先嘗試不詢問，這在使用金鑰時可行，否則在終端機中詢問。沒有終端機時，或登入失敗時，執行會在複製任何東西之前停止，並說明原因，原因照 `ssh` 所說：只有 `ssh` 被拒絕登入時才說需要登入，其他情況例如找不到 host，就照實說明。
- **一個 session。** host 上每個 session 都要跑你的登入 shell 與它的啟動檔，可能要好幾秒。Ferry 只開一個，其餘都走 channel，不跑 shell，就像 `ssh -W` 開的那樣：`sshd` 不肯轉發到 Unix socket 時，改連 loopback 上的一個 port，每條連線先交出秘密。只有加上 `--remote-join` 時，每條連線才各開一個 session。
- **一切都走它。** host 唯一的 session，它探測 host、安裝 Ferry，然後成為 Ferry 的 monitor；monitor 啟動的 server；以及這次傳輸唯一的一條 stream，都走該 host 唯一的一條 SSH connection，所以都會到同一台機器，即使 host 名稱可能通往好幾台，例如叢集在同一個名稱後面的幾個 login node。開好幾條的 `--ssh-connections` 已經拿掉：還帶著它的指令列會被拒絕，以 2 結束。
- **SSH connection 斷了，傳輸就失敗**，和 rsync 一樣。client 與 host 上 Ferry 的 monitor 之間，兩個方向每 5 秒各送一次心跳，其他送來的東西（包括資料）也一樣算數，因此完全沒有東西送來的連線，例如網路斷了或筆電進入睡眠時，會在 30 秒內被發現，關閉的則會立即被發現；host 的 monitor 也會發現，並停下它在那裡啟動的東西。傳輸接著失敗，並說明原因，例如 `nothing came from fatcat for 30 s`；不會重新登入。已完成的檔案會留著，重新執行會略過它們。只是慢、或在等磁碟而什麼都不說的 server，絕不會被當成不在了。

## 核心

在 Linux 上，Ferry 在每台機器上最多只用 8 個核心，除非 `--cpus` 另有指定：client 如此，每個 server 在它的 host 上也如此，因此在許多人共用的主機上，例如叢集的 login node，傳輸會把其餘核心留給那裡的其他人。用哪幾個核心，由 client 的行程 ID 決定，client 與每個 server 都一樣，所以同一台機器上的幾個 Ferry 不會全擠在同樣的核心上；這些核心也都在同一個 NUMA node 上，只要有一個 node 的核心夠多：分散在兩個 node 上時，同樣的複製要多花一半的時間。在 96 核心上複製 20 萬個 4 KiB 的檔案，限制在 8 個核心時一樣快或更快，CPU 只用一半或更少：核心多時多用的，大多是 thread 在 kernel 中互相等待。`--cpus all` 使用全部的核心。在 macOS 上一律使用全部的核心。

## 檔案

release build 與 dev build 絕不共用以下任何東西：release build 使用以下名稱，dev build 則使用加上 `-dev` 的相同名稱，例如 `~/.ferry-dev/`。

| 所在 | 路徑 | 存放內容 |
|---|---|---|
| client | `/tmp/ferry-<uid>/ssh/` | Ferry 自己的 SSH connection 的 socket，每個 host 一個，模式 0700 |
| client、host | `/tmp/ferry-<uid>/join/`，Linux 上則是 `$XDG_RUNTIME_DIR/ferry/join/` | server 或 Ferry 的 monitor 執行期間，stream 或每條連到 monitor 的連線用來連到它的 socket，模式 0700 |

若別的使用者先建立了 `/tmp/ferry-<uid>`，Ferry 不會使用它，而改用 `/tmp/ferry-<uid>-1` 到 `-9` 之中第一個只屬於你的目錄，每次都是同一個。
| client | `~/.ferry/releases/<version>/` | 下載的 release 執行檔，以及它們的 `SHA256SUMS` |
| host | `~/.ferry/hosts/<machine-id>/bin/<build>/ferry` | 每個已安裝的 build |
| host | `~/.ferry/hosts/<machine-id>/src/`, `build/` | dev build 的原始碼與保留的 target 目錄 |

machine ID 用來區隔共用家目錄的叢集節點。沒有 machine ID（`/etc/machine-id`，或 macOS 上的 `IOPlatformUUID`）的 host 會被拒絕。

## 環境

| 變數 | 作用 |
|---|---|
| `FERRY_HOME` | Ferry 的目錄，取代 `~/.ferry`。必須是絕對路徑。在 host 上，於 SSH 提供給指令的環境中設定 |
| `HOME` | 未設定 `FERRY_HOME` 時，`~/.ferry` 所在之處 |
| `FERRY_SSH` | 取代 `ssh` 執行的指令，例如 `ssh -p 2222`，如同 rsync 的 `RSYNC_RSH`：以空白分詞，引號內的字不分開，`ssh` 的引數接在它自己的引數之後 |
| `FERRY_SUDO_PASSWORD` | 用於 `--remote-sudo`：每個 host 上 `sudo` 需要的密碼，不再詢問。Ferry 啟動時讀取一次，不會傳給它執行的任何程式 |
| `NO_COLOR` | 設為任何值時，儀表板不使用顏色 |

Ferry 會把自己的開檔上限（`ulimit -n`）調到系統允許的最大值，也就是硬上限，在 macOS 上則不超過 `kern.maxfilesperproc`；它執行的程式（例如 `ssh`）拿到的則是它啟動時的上限。調整後在本機上仍低於 1024 時，它會警告，並在這個上限內運作，只是比較慢。

## `ferry update`

```text
ferry update [--check | --version VERSION]
```

把這個執行檔換成最新的 release，或換成 VERSION，不論較舊或較新。它從 GitHub 的 `releases/latest` 轉向的位置找出最新的 release，以 `curl` 下載符合這台機器的執行檔，以該 release 的 `SHA256SUMS` 檢查，並與 host 用的一樣保存在 `~/.ferry/releases/<version>/`，因此之後要安裝到同平台的 host 時不必再下載。執行檔是在它所在的目錄中以改名的方式替換，因此更新失敗時，舊的執行檔會完整保留。若該目錄不可寫入，更新會停止並說明原因。

Ferry 絕不自行檢查是否有新的 release；只有 `ferry update` 會。host 不需要任何處理：下一次傳輸會在那裡安裝新的 build，與任何 build 一樣。

dev build（從原始碼建置的）會拒絕：請從原始碼更新。

| 選項 | 效果 |
|---|---|
| `--check` | 只說明是否有較新的 release：有則以 3 結束，沒有則以 0 結束，檢查失敗則以 1 結束 |
| `--version VERSION` | 改為更新到 VERSION，例如 `0.1.0` |

## `ferry test`

```text
ferry test [--yes] [--remote-ferry PATH] [--remote-join] [--scale SIZE --kind KIND] SOURCE DESTINATION
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

在 host 上，測試目錄樹由 Ferry 自己透過隱藏的 `ferry fixture` 子指令建立與移除，由 Ferry 在那裡的 monitor 啟動，和傳輸的 server 一樣，因此 host 除了 SSH 之外什麼都不需要，host 的 shell 印的東西也不會混進它說的話。host 唯一的 session 開始時 shell 印出的東西會被跳過，並說一次，和傳輸時一樣。`--yes`、`--remote-ferry` 與 `--remote-join` 的意義與在傳輸中相同，並套用到測試執行的每一次傳輸。

沒有任何情境失敗時，結束狀態為 0，否則為 1。

### 實際規模

加上 `--scale SIZE`（例如 `500M` 或 `20G`，以二進位單位計）與 `--kind KIND`（兩者都必須給）時，測試不執行任何情境，而是像使用者一樣，以該大小與種類的目錄樹實際傳輸一次：在終端機上，每次傳輸都會顯示其 dashboard，接著是它的摘要。

```text
$ ferry test /tmp workbox:/tmp --scale 1G --kind mixed
ferry test: 1.0 GiB of mixed, from /tmp/ferry-test-371f59084456c9-source to workbox:/tmp/ferry-test-371f59084456c9-destination
  ok    workbox: ready (0.82s)
  ok    generate: the source tree, 9,525 files in 579 directories, 46 symlinks, 1.0 GiB (7.42s)
ferry: 10,152 entries compared, 9,525 files sent, 1.0 GiB in 32.8 s (31.2 MiB/s) · 19% saved by compression
  ok    copy: the tree is received, at 31.1 MiB/s (32.91s)
  ok    copy: the destination matches the source (1.03s)
  ok    change: the source, 96 rewritten, 96 touched, 96 deleted, docs/d7/d1/d1/ removed, 212 files, 10.2 MiB added (0.24s)
ferry: 10,264 entries compared, 404 files sent, 14.3 MiB in 0.4 s (34.3 MiB/s) · 27% saved by compression
  ok    sync: the changes are received (0.53s)
  ok    sync: the destination matches the source (0.97s)
  ok    sync: nothing more is needed (0.15s)
8 passed, 0 failed
```

各步驟依序執行，並在第一個失敗的步驟停下：

1. **產生。** 在來源端產生種類為 KIND、檔案共 SIZE 的目錄樹（見下文）。相同的 KIND 與 SIZE 在每台機器上都產生相同的目錄樹。
2. **複製**到目的地，之後目的地必須與來源一致。
3. **改動**來源，如同使用者在兩次同步之間會做的：移除一個目錄；在剩下的檔案中，每百個裡有一個以相同大小的其他位元組改寫、一個只改為較舊的 mtime、一個被刪除；並新增大約 1% 的資料。
4. **同步**，之後目的地必須再次與來源一致。
5. 接著，同一個同步的 dry run 必須什麼都不列出。

各種類的形狀都仿照該類資料常見的樣子。檔案大小如同真實檔案系統，呈對數常態分布並帶有長尾；每個目錄的檔案有多有少（多數很少），巢狀深淺不一；內容則是文字（壓縮率如一般文章）、隨機位元組（無法壓縮），或兩者交替的區塊（約可壓縮一半）。

| KIND | 模擬 | 檔案 | 目錄 |
|---|---|---|---|
| `small` | 原始碼、套件與環境、小圖片資料集 | 每個數 KiB，3% 為空檔，一成與另一個檔案內容相同，含一些符號連結；多為文字 | 深，中位數每個目錄 8 個檔案 |
| `large` | checkpoint、影片、壓縮檔 | 每個 64 到 256 MiB，多為隨機位元組，旁邊附幾個小文字檔 | 淺，每個目錄幾個檔案 |
| `mixed` | 專案目錄或家目錄 | `code/` 同 `small`，佔 SIZE 的一成；`docs/` 為各種大小、最大 256 MiB 的檔案，佔三成；`data/` 同 `large`，佔其餘 | 依各部分 |

以 1 GiB 而言，`small` 約產生 87,000 個檔案、5,000 個目錄，`large` 約 30 個檔案（其中 8 個是大檔），`mixed` 約 9,500 個檔案。benchmark 所用的 `ferry fixture generate` 產生相同的目錄樹。

所謂一致，是兩棵目錄樹有相同的摘要：每個 entry 的路徑、類型、大小、權限、精確到奈秒的 mtime 與內容，與情境所比較的相同。兩側各自透過隱藏的 `ferry fixture digest` 計算自己的摘要，因此只有摘要會經過網路。

所有步驟完成後，兩棵目錄樹都會被移除。若有步驟失敗，則會保留它們並印出路徑，以便調查；之後請手動移除。

第一次 Ctrl-C 會停止測試：正在執行的步驟會停下（在 host 上也是），兩棵目錄樹不論處於什麼狀態都會被移除，結束狀態為 130。在 dashboard 上，Ctrl-C 會停止傳輸，測試也隨之停止。第二次 Ctrl-C 會立即結束，尚未移除的部分會保留在第一行所列的路徑。

## `ferry server`

一個隱藏的子指令：傳輸的一側。client 為每一側啟動一個，在 host 上經由 Ferry 在那裡的 monitor，在本機上由自己啟動，並建立兩者之間的 stream。它的標準輸入與輸出就是它的 control connection：它在那裡交握，之後改以 stream 層在上面收發，client 結束它時，server 就結束。有 `--lifeline` 時（它的 monitor 都這樣啟動它），它的標準錯誤就是生命線：monitor 一不在了，它就像收到 `SIGTERM` 一樣停下。接著它經由通往另一個 server 的 stream 執行其 peer，stream 在 Unix socket 上連到它；有 `--port` 時則在 loopback 上的一個 port，先交出秘密，monitor 自己是經 port 連到的時候，就會這樣啟動 server。`ferry server --join <token>` 用在 SSH 兩種都不肯轉發的地方，它透過 Unix socket 把自己的標準輸入與輸出交給 server 或 monitor，並等到對方用完。兩者都不是供人手動執行的。

## `ferry monitor`

一個隱藏的子指令：host 唯一的 session 在 Ferry 裝好之後變成的東西，即 `ferry monitor --tag TAG`。它在私有目錄中的一個 Unix socket 上等待，也在 loopback 上的一個 port 上等待，並在標準輸出上，以標了 TAG 的行說出兩者在哪裡、port 的秘密，以及它的 build；client 的第一條連線決定留下哪一個，另一個就關掉。client 以那條連線追蹤它，即連結，並在其餘每一條上請它啟動一個程式，那個程式就以該連線為標準輸入與輸出，以它的標準錯誤為生命線：`ferry server`（`--remote-sudo` 時經 `sudo`）、sudo 的檢查，或 `ferry fixture`。它看著每一個，在連結上說出每個怎麼結束、何時被暫停或繼續、跑到結束的程式在標準錯誤上說了什麼，以及被問時每個從外面看起來怎樣。連結在說出用途之後改走 stream 層，兩個方向每 5 秒各送一次心跳。在連結上被告知停下時，monitor 對它啟動的東西送 `SIGTERM`，並說出每個怎麼結束。連結結束、30 秒什麼都沒收到，或 client 沒在 30 秒內追蹤它時，monitor 對它啟動的東西送 `SIGTERM`，再過 30 秒砍掉剩下的，然後結束。它不是供人手動執行的。
