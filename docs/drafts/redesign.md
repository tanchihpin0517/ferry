# 重新審視 ferry 的設計：缺陷、共用模組與順序

- 狀態：草稿，問題都已決定，照「順序」進行中
- 開始：2026-11-20；最後修改：2026-11-20
- 起因：使用者要完全重新思考 ferry 的設計，包括架構上的修改與取代。目標是更多共用模組、更精簡的程式碼、更規範的行為、更穩定、更少 bug、效能更好。前一份草稿 stage-module 已放棄，也從歷史中移除。
- 依據：
  - 五份清點：transport、client 與 monitors、Source、Destination、歷史與測試；
  - 只讀了程式碼、文件與 git，沒有建置，也沒有執行；
  - 行號以 f73b24a 為準；
  - 標「核對過」的，是我親自讀過那幾行；其餘是清點確認的；
  - 清點的整理在 `.scratch/redesign/findings.md`，165 個修正的分類表在 `.scratch/redesign/bugs.md`。
- 第四節是使用者另外提的想法：所有 stage 共用一組執行緒。

## 目標

使用者的期待，換成可以檢查的說法：

1. **更多共用模組、更精簡。** 同一件事只寫一次，用「現在寫了幾份」來量。
2. **更規範。** 同一種情況，不論在哪裡遇到，都用同樣的方式處理。每個共用模組附一條由測試強制的規則，不靠人記得。
3. **更穩定、更少 bug。** 修掉現有的缺陷，也擋住歷史上一再出現的那幾類。
4. **效能更好。** 每一步都不能變慢，和上一步輪流量測；效能項目在動工時才量。

## 結論

![Bar chart of the 118 fixes to product code in four periods, each bar split into the transport with sessions and monitors, and every other area. Oct 14 to 17, the first build: 20 fixes, 2 in the transport and monitors. Oct 18 to 22, the Oct 17 review's list: 54, 7 there. Oct 23 to 27, the rewrites A1, B1, A2, one-session and silent-drops: 32, 24 there; 89% of them changed code under 7 days old, where about 54% of the code then was. Oct 28 to Nov 13, pipes, large directories, filter and watch: 12, 3 there.](assets/redesign-history.svg)

1. **可以重新設計，但不該一次重寫。**
   - 165 個修正裡，90% 改到不滿 7 天的程式碼（圖 1）。
   - 10-23 到 10-27 那次大重寫，修正有 89% 落在新程式碼，而同期程式碼裡不滿 7 天的只占約 54%。
   - transport 從 10-15 到 10-28 換過約五版。產品程式碼的修正，有三成在 transport 和 session/monitor。
   - 從零重寫，預期會再來一波同樣規模的 bug，以平台差異和 transport 為主。所以要逐區替換，每一步都在 macOS CI 和真實主機上驗收。
2. **大的架構決定都站得住。** 見「七、該保留的」。例如：
   - 不做 I/O 的 decider：10-24 之後 15 個 commit，沒有一個是修正；
   - 集中判斷連線存活：做出來之後，這類 bug 沒再出現。
3. **真正的問題是「同一件事寫在好幾處，每處略有不同」。**
   - 歷史上有約 11 條「同一個修法，別處還要再修一次」的鏈，例如主機輸出的標記修了三次、測試缺 `kill(1)` 修了四次。
   - 這次找到的缺陷 1、3、4、7，也都是同一件事有兩種寫法，彼此不一致。
   - 共用模組要用在這裡才划算。

## 一、現有的缺陷

照「記問題，不附做法」，這裡只記問題與證據。都沒有實際重現。

1. **sync 可能刪掉規則要保留的東西。**（核對過，圖 2）
   - 條件，四個同時成立：sync；錨定的規則，例如 `--exclude /data/sub/f/keep`；SOURCE 寫成 `data` 而不是 `data/`；DEST 上的目錄 `sub/f`，在 SOURCE 那邊變成了檔案。
   - 經過：
     1. renamers 從 decider 拿到的是顯示路徑 `data/sub/f`（`decider.rs:1485-1492`，`rename.rs:177-183`）；
     2. `clear_the_way` 用它算比對規則的位置（`apply.rs:1110-1125`）；
     3. `Filter::excludes` 又加一次 SOURCE 的名稱（`filter/mod.rs:137-148`，`under` 由 `peer.rs:505` 設定）；
     4. 比對的路徑成了 `data/data/sub/f/keep`，對不上規則，`keep` 被刪掉。
   - makers 傳的是相對路徑（`apply.rs:803`、`:826`），所以規則對得上。dry run 也是用相對路徑（`receive.rs:255-261`），所以 dry run 列出的和實際做的不一樣。
   - 來由：cb44d7c 開始用這個參數算比對的位置，當時它是相對路徑；caf6950 把它改成顯示路徑，這裡沒有跟著改。
   - 測試沒有涵蓋：`destination/tests.rs:451` 與 `tests/rsync.rs:1123` 只用不錨定的 `*.o`，也沒有 SOURCE 的名稱。
2. **大檔的分段狀態整個 run 都不釋放。**（核對過）
   - `files.segmented` 只有插入（`decider.rs:1544`），沒有任何地方移除。
   - 記憶體隨大於 2 MiB 的檔案數成長。清點估計每個檔約 0.9 KB，100 萬個約 0.9 GB，沒有量。和 5a11736 是同一類。
3. **檔名裡的控制字元會直接送到終端機。**（核對過）
   - 會讓傳輸停止的錯誤，訊息裡的路徑是 `from_utf8_lossy` 轉出來的原始位元組（`decider.rs:1371`；顯示路徑本身沒有跳脫，`report.rs:87-98`）。
   - 最後一行錯誤由 `main.rs:28` 直接寫到 stderr，沒有經過 `printable`。`status::say`（`status.rs:91-95`）也一樣。
   - 違背 452f542「名稱要安全地印出」的用意。沒有用惡意檔名實測。
4. **非 UTF-8 的名稱，在訊息裡變成 `�`。**
   - 約 18 處先 `String::from_utf8_lossy`，再組成訊息（例如 `source/mod.rs:557`、`scan.rs:630`），`say` 收的是 `String`（`report.rs:410`）。
   - `a\xff` 和 `a\xfe` 在警告裡都顯示成 `a�`；dry run 的清單裡卻是 `\#ooo`，和 rsync 一樣。
5. **錯誤訊息重複「the stream broke:」。**（核對過）
   - wire 層的錯誤已經寫成「the stream broke: …」（`wire.rs:539`、`:562`、`:622`、`:636`、`:680`）。
   - pipes 的 `receive` 與 `pump` 把它交給 `broke()`（`pipes.rs:1751`、`:1983`），`broke()` 又加一次（`pipes.rs:1520`）。
   - 測試只用 `contains` 比對（`pipes/tests.rs:483`、`:497`），所以沒抓到。
6. **SOURCE 是檔案時，讀檔會跟隨 symlink。**（前半核對過）
   - `open_regular_root` 刻意拿掉 `NOFOLLOW`（`fsys.rs:827-831`，由 `read.rs:175-177` 呼叫）。
   - 清點說，ed7291b、acae230 之後，最後一段是 symlink 的 SOURCE 一律當 symlink 傳送，所以跟隨已經沒有用處，只剩一個競態：掃描之後有人把 SOURCE 換成 symlink，reader 就讀到別的檔案。用 `--remote-sudo` 時，能寫 SOURCE 所在目錄的人就能利用。這半句我沒有核對。
   - 掃描先用路徑 stat 根（`scan.rs:206`），再用路徑另開一次（`scan.rs:227`），兩次沒有比對 dev/ino。
7. **Source 遇到消失的檔案，三個地方三種結果。**
   - stat 時：vanished，只警告，結束碼 24（`scan.rs:728-742`）。
   - 開目錄時：Unreadable，結束碼 23（`scan.rs:603-605`、`:640-642`）。
   - reader 開檔時：「cannot read」，送出 `Data::Unopened`，結束碼 23（`read.rs:76-78`、`source/mod.rs:551-561`）。
   - 清點說 rsync 在開檔與開目錄時遇到 ENOENT，都說「file has vanished」，結束碼 24。這是依 rsync 原始碼推斷的，照規矩要先實跑 rsync 確認。
8. **查家目錄沒有走 getent。**
   - `homes()`、`home_of()` 直接呼叫 libc 的 `getpwuid_r`（`fsys.rs:406-453`），不是已決定的 `getent`（`overview.md:320-324`）。
   - 用 `sudo ferry sync` 時，LDAP 帳號的家目錄查不到，「清空家目錄前先問」（`prepare/plan.rs:171`）就漏掉它。沒有在 LDAP 主機上實測。
9. **顯示壞掉時，警告整批不見。**
   - 清空 channel 時只算 `Counts`（`progress.rs:313-318`）；dashboard 交出 counts，`state.said` 被丟掉（`dashboard.rs:146`）；`cli.rs:1218-1220` 拿空的 `Vec` 頂替。
10. **stop 之後仍會改動磁碟。**
    - makers（`make_part`，`apply.rs:773`）、finishers（`finish_directory`，`finish.rs:23`）、renamers（`rename_into_place`，`rename.rs:147`）、writers 的 `Job::Finish`（`write.rs:280`）都不查 stop，違反 `pipeline.md:117-119`。
    - sync 時，`clear_the_way` 可能整棵刪掉擋路的目錄（`apply.rs:1116`）。
11. **watch 的結尾少一行。** `Board::end` 不說「N paths could not be read」，一次傳輸的版本會說（`cli.rs:805-962` 對照 `cli.rs:1265-1293`）。
12. **兩個 Ctrl-C 的小問題。**
    - `update.rs:83` 的暫存檔 `.ferry-update-PID` 沒有登記，Ctrl-C 後會留下。
    - 推測：sudo 的密碼提示直接阻塞讀 `/dev/tty`（`sudo.rs:164-189`），第一次 Ctrl-C 結束不了它。
13. **推測：準備階段 server panic，可能以 1 結束而不是 101。** `failed_preparing`（`cli.rs:1121`）不像 `transferred`（`cli.rs:1307`）那樣檢查 server 的 101。

## 二、共用模組

![Four columns: the transport, the client and monitors, the Source, the Destination. Eight shared modules, by strength of evidence, top first, each drawn across the areas it replaces code in. 1, output and path types, across the client, Source and Destination. 2, error classes, across Source and Destination. 3, the first cause, across all four. 4, threads, spawn(role) and a shared pool, across all four. 5, timers and telemetry, across all four. 6, the display pipe, in the client. 7, host output markers, in the transport. 8, a filesystem layer of Name, Meta and TempFile, across Source and Destination. Under each, what it replaces. Each comes with one rule a test enforces.](assets/redesign-modules.svg)

依證據強弱排序（圖 3）。依「規則越少越好」：每個模組加一條由測試強制的規則，但拿掉的是好幾條現在靠人記得的慣例。

### 1. 輸出出口與路徑型別

![How defect 1 happens, and the proposal. Conditions: sync, an anchored rule such as --exclude /data/sub/f/keep, SOURCE given as data rather than data/, and a directory sub/f on DEST that is a file on SOURCE. a, the makers pass the relative path sub/f; Filter::excludes prefixes SOURCE's name and matches data/sub/f/keep, so keep is kept. b, the renamers get the shown path data/sub/f from the decider; clear_the_way takes the place to match from it, Filter::excludes prefixes SOURCE's name again, and data/data/sub/f/keep matches no rule, so keep is deleted. c, proposed: three kinds of path as three types: the relative path, the only one filesystem calls and rules take, which stops defect 1; the shown path, made only when reported, a Report carrying bytes; the printable string, made only at the one way out, which escapes control characters and prints non-UTF-8 as \#ooo, stopping defects 3 and 4.](assets/redesign-paths.svg)

- **現在**：
  - 路徑以原始位元組傳來傳去，各處自己決定怎麼顯示。
  - 執行端的 `shown` 有時是相對路徑，有時是顯示路徑（缺陷 1）。
  - 約 18 處先轉成字串（缺陷 4）。
  - `printable::line` 只在三處套用（`progress.rs:405`、`dashboard.rs:340`、`cli.rs:993`），其他輸出都沒有（缺陷 3）。
  - `cli.rs` 有 17 處直接寫 stderr，`ferry: ` 前綴各處自己加。
- **提案**：
  - 三種路徑是三個型別（圖 2c）：
    - 相對路徑：檔案系統與規則只收它；
    - 顯示路徑：只在 decider 與 State 回報時產生，`Report` 帶位元組；
    - 可印出的字串：只在輸出出口產生。
  - 只有一個輸出出口，例如就是 `status`。它負責前綴、跳脫、清狀態列。
- **強制規則**：client 用 clippy 的 `disallowed_macros` 禁止 `eprintln!`；執行端的函式只收相對路徑的型別。
- **證據**：缺陷 1、3、4。**強。**

### 2. 錯誤分類

- **現在**：
  - `pipeline.md:122-134` 說錯誤依它是什麼來分類，不論在哪裡遇到。
  - 但 Source 沒有分類函式（缺陷 7）。
  - Destination 分在六處：
    - `decider.rs:2252` 的 `what_failed`、`:2293` 的 `why_failed`、`:2144` 的 `tell_deleted`、`:1357` 的 `finished`、`:1042` 的 `extras`；
    - 執行端另有 `apply.rs:976` 的 `unsettable` 和 `:916` 的 PERM→Special。
  - delete lane 讀回 spill 失敗時，直接讓 stage 失敗（`apply.rs:157`、`:607`），繞過 decider。
  - 表格說警告「每類一條、結尾彙總」，但「not deleting」是逐條立即說（`decider.rs:2128-2138`、`:2155-2159`）。
- **提案**：一個 `classify(error, context)`，回傳 Fail、Warn(kind)、Vanished 或 Stop，兩邊共用。`pipeline.md` 的那張表就是它。
- **證據**：缺陷 7。另外，fa74725 把分級規則集中之後，這類只再修過一次（f738729），說明集中是有效的。**中強。**

### 3. 第一個原因

- **現在**：
  - peer 用 `stop.rs`。transport 另有兩份：wire 的 `Scope.why`（`wire.rs:433-470`）與 pipes 的 `Core.broken`（`pipes.rs:1518-1521`）。缺陷 5 就是兩份各加一次前綴。
  - 「先記原因，再關輸出」在 peer 裡手寫五處（`receive.rs:66`、`:439`、`:466`，`destination/mod.rs:472`，`source/mod.rs:429`）。
- **提案**：一個型別記住第一個原因。輸出由它持有，原因記下之後才放手，就不必每處手寫順序。
- **證據**：歷史上「說出的是後果，或最後的話不見了」有 12 個（例如 f7fffd6、2e3385a、24c3a38、0b75bdd、9c2d80d），清點估計這樣做擋得住約 4 個。**中。**

### 4. 執行緒：`spawn(role)` 與共用的執行緒池

- **現在**：
  - client 這一區約 23 處開執行緒，只有 2 處有名字（`monitor/client.rs:135`、`watcher.rs:79`），但 `architecture.md:978` 說每個執行緒都以 role 命名。
  - 只有 `remove.rs`、`stage.rs`、`pipes.rs` 用 `profile::thread` 登記。
  - 沒開 profiling 時，panic 的訊息一律是「internal error in main」（`profile.rs:377-384`，`cli.rs:451`）。
  - `remove::trees` 繞過 `Stages`（`remove.rs:105`）。
  - Source 與 Destination 有 7 處手寫的 worker 迴圈，執行緒數有的寫死、有的用 `pool::threads`。
- **提案**：
  - 所有執行緒都經過一個 `spawn(role)`，panic 的 hook 從執行緒的名字取 role。
  - 執行者共用一組執行緒，見第四節。
- **強制規則**：`src/` 裡除了這個模組，不准直接 `std::thread::spawn` 或 `scope.spawn`。
- **證據**：
  - 有了 `Stages` 之後，還有兩次繞過它（373aec1、6273896），所以要靠規則擋。
  - 清點估計，這一層加上規則，歷史上大約擋得住 2 到 3 個。**中，但擋下的 bug 不多**，價值主要在精簡與一致。

### 5. 定時與 telemetry

- **現在**：
  - 每個 server 有三個定時執行緒，各用不同條件結束：`report_carried`（`peer.rs:361`）、`report_metrics`（`peer.rs:464`）、`report_queues`（`report.rs:496`）。
  - 同一批 gauge 走兩條路：`Report::Queues` 每 250 ms，`Levels` 每 1 s，而且只在 profiling 開著時才送。
  - heartbeat 有三種寫法（`transport/mod.rs:758`、`transport/monitor.rs:485`、`monitor/host.rs:444`）。
- **提案**：
  - 每個 server 只留一個 telemetry tick，dashboard 從同一份資料算出瓶頸與佇列表；
  - heartbeat 用 `Scope::sleep(period)`。`Scope::wait` 已經支援 deadline（`scope.rs:134-166`），scope 一結束就醒。
- **證據**：中。每個 server 少兩個執行緒、少兩種 report。

### 6. 顯示管道

- **現在**：
  - `Report` 有 22 個變體，同時當線上的格式和顯示事件，其中 4 個從來不上線（`report.rs:103`）。
  - 一條 `bounded(16)` 的 channel，有六種送法（`follow.rs:246`、`:275`、`:288-296`，`monitor/client.rs:446-452`、`:810-832`）。
  - 負責偵測「60 s 沒動靜」的 control 執行緒，自己會卡在送出上（`follow.rs:349`、`:381`、`:419`、`:421`）。
  - 缺陷 9。
- **提案**：
  - 線上的 `Report` 和 client 內部的顯示事件分成兩個型別；
  - 可以合併的狀態放進共用的快照，由畫面每一格去讀，送的一方永遠不等，例如 counts、levels、transferring；
  - 有順序的內容走另一路：`Listed` 保留背壓；`Said` 在顯示壞掉時改寫到 stderr。
- **證據**：缺陷 9，以及 fe1c23f（relay 在滿的顯示佇列上睡 10 ms）。**中強。**

### 7. 主機輸出的標記

- **現在**：
  - 掃描 `\0ferry:` 標記的程式有三份（`start.rs:60-99`、`:289-364`，`lifeline.rs:91-162`）。
  - DONE 的計數兩份（`start.rs:369-396`、`transport/mod.rs:819-862`）。
- **提案**：一個不做 I/O 的 scanner，位元組進、事件出，三處共用，並納入 fuzz。
- **強制規則**：所有主機輸出都經過它，testkit 也一樣。
- **證據**：「輸出放在標記之間」修了三次（95841f9、c709beb、0471a76），原因是 testkit 沒走同一條路。**中。**

### 8. 檔案系統層：`Name`、`Meta`、`TempFile`

- **現在**：
  - 名稱規則兩份：`one_name`（`fsys.rs:173-181`）與 `manifest::Check`（`manifest.rs:381-390`）。`fsys::stat`、`read_link` 不檢查，靠呼叫者記得。
  - stat 的欄位到處自己解：10 個檔案裡有 26 處 `allow(clippy::unnecessary_cast)`；`fsys::entry`（`fsys.rs:764-804`）與 `compare::verdict`（`compare.rs:743-810`）各解一次。
  - 「父目錄與名稱」的拆分寫了 7 份；getdents 的迴圈兩份（`fsys.rs:647-680`、`:730-746`）。
  - 暫存檔的處理三處（`apply.rs:895-943`、`write.rs:307-405`、`rename.rs:147-197`）；`fsys.rs:866-984` 與 `sorted.rs:920-967` 又重複一次。屬性設定的 `Setters` 組了四次。
- **提案**：
  - `Name`：列目錄與檢查 manifest 時各建一次，由編譯器保證；
  - `Meta`：stat 只在這裡解一次，也正好是 statx 的接入點；
  - `TempFile`：建立、登記、失敗時刪除，只有一個地方負責。
- **證據**：`fsys.rs` 的 34 個 commit 裡有 11 個是修正，大多是「規則漏了一處」（afc00b6、ada3b00、aa6edf0、41857ba）。**中。**

## 三、架構上的修改

1. **測試改走真的 pipes，刪掉模擬器。**
   - peer 的單元測試和 `destination_stream` 的 fuzz，現在走 `source::run`、`destination::run` 這個「單一 packet 流」的模擬器。它沒有 window，也沒有 `Stopper`；兩邊 channel 的上限不同；錯誤字串也不一樣（`destination/mod.rs:327`）。
   - 大約 300 行正式程式碼只為測試存在：`source/mod.rs:122-216`、`destination/mod.rs:288-392`（手寫的 varint 和 `protocol.rs:458-467` 重複）、`mux.rs:43-147`、PacketType 1/2/3、`block.rs:224` 的 `ByteBuf`。
   - 改在 `UnixStream::pair()` 上跑真的 `pipes::Connection`，`pipes/tests.rs:12-41` 已經這樣做。這樣測試就測得到背壓與關閉的語意，而過去卡住的問題正是出在這裡。
   - 代價：二十多個測試的 fixture 要改，fuzz 會慢一點。
2. **一次傳輸，當成只有一輪的 watch。**
   - `Board`（`cli.rs:805-962`）與 `run_round`（`cli.rs:1133-1294`）把同一個流程寫了兩次，約 100 行，行為已經不一致（缺陷 11）。
   - monitor 已經這樣做了（`Core::client` 與 `Core::watch`）。
3. **pipes 的內部。** 放在最後做，因為 transport 最容易出平台問題。
   - **依狀態叫醒。** 現在要叫醒誰，分散在 18 處 `Woke` 旗標，加上約 10 處臨時的 `notify`，用到 8 種 condvar。cf37385 就是漏設一個旗標，大約每百次永遠卡住一次。改成每次改動之後，由一個 `fn due(&Core) -> Woke` 依狀態算出該叫醒誰。fuzz 每一步也檢查：等待者的條件成立，就一定有人叫醒它。
   - **結束狀態改用 enum。** 現在每個 pipe 方向有好幾個 bool 和 7 個判斷函式。a63d069 與 eaab0a4 相隔約 30 小時，修的方向卻相反。
   - **讀寫迴圈只留一組。** 準備階段與 ready 之後各有一組，處理結束和錯誤的規則不一樣（`wire.rs:328-353`、`:405-424` 對照 `:533-567`、`:647-673`）。0b75bdd 修的那種丟資料，只在其中一組修了。
4. **manifest 的閘門。**
   - 寫 manifest 的執行緒每一批都和 owner 來回一次（`source/mod.rs:761-779`）。一百萬個空目錄的 dry run，21 s 裡等了 13.5 s（`future.md`）。
   - 改成 owner 在 `needed` 越過門檻時發出 `Command::Gate`，scan 只在閘門關著時才等。決定仍在 owner，owner 也不等任何東西。
5. **結束碼集中在一個地方決定。**
   - 現在分在 `main.rs:13-40` 與 `cli.rs:1296-1359`。
   - 顯示的執行緒遇到 EPIPE，直接 `interrupt::stop(0)`（`progress.rs:333`、`:365`），繞過 monitor。
   - 130 這條路修過三次（cdfb4a5、dcc05ae、d589cf6）。缺陷 13 也在這裡。
6. **`interrupt::on_exit` 只有一格，卻有三個擁有者。** dashboard、`status`、sudo 各自設定、清除這一格（`interrupt.rs:105`、`:427`）。改成 RAII 的登記表，沿用 `CHILDREN`、`PATHS` 已經在用的寫法。
7. **刪掉沒有人用的程式碼。**
   - `PipeWriter::sync` 與相關狀態約 100 行，自 25af573 起沒有呼叫者；
   - `prepare::prepare`；
   - interrupt 裡只給 testkit 用的第二套 Ctrl-C，約 60 行。
8. **decider 內部拆成幾個小狀態機，仍然只有一個 owner。**
   - decider 的程式約 2,300 行，`Decider` 有 46 個方法；同一個在途的檔案，狀態分散在四張表和好幾個 bool 裡。
   - 但 10-24 之後沒有修正，所以這是預防性的，優先度低。
9. **刪除只留一個走訪器。** `remove::trees` 與 dry run 用的 `remove::count`（`remove.rs:700-802`）各自實作保留、其他檔案系統、規則與深度的判斷。價值中低。

## 四、共用的執行緒資源

![a, today: each stage on the Destination has its own fixed threads: stat 8, checkers 8, makers 16, deleters 16, finishers 16, writers up to 16, renamers 4, and the transport's decompressing 8, about 90 workers, most of the time blocked in filesystem calls; beside them the single threads: the decider, the feeder, compare, compared, the manifest's and the data's readers, the pumps, the stream readers and the lifeline. b, proposed: the decider's commands go to one pool of N threads in all, N to be measured, where each stage keeps a cap, renamers 4 and stat 8 having been measured; the pool checks the stop before each piece, marks the profile and gauges, and says what became of each piece as an event; a piece waits only on I/O, never on another piece. Stat and the checkers take their work from compare and the data's reader, as now. The single threads, and the transport's 8 decompressing threads, stay their own. A free thread takes the stage furthest downstream first. The risk: on NFS, fewer I/Os at once is slower; with 8 workers a copy took a tenth longer.](assets/redesign-threads.svg)

使用者的想法：與其每個 stage 開固定數量的執行緒，不如設一個總數，再把執行緒動態分給需要的 stage。

- **範圍**：一個 peer 的 process 之內。Source 與 Destination 是不同的 process。
- **能共用的**：做完一件就拿下一件的 worker，包括 stat、checkers、makers、deleters、finishers、writers、renamers、readers。
- **不能共用的**：
  - 一直等著的單一執行緒，包括 owner、decider、data sender、feeder、compare、compared、pump、讀 pipe 的、lifeline。它們會一直佔著一個執行緒。
  - transport 的壓縮與解壓縮。它們在 `Core` 的鎖裡交錯預扣、退還 window（`pipes.rs:978-998`、`:1056-1078`），transport 那份清點評估過，併進來反而更複雜。
- **好處**：
  - 只要設一個總數；有工作的 stage 就拿得到執行緒。
  - 手寫的 worker 迴圈都不見了：每件之前查 stop（缺陷 10）、標 profile、失敗就停掉全部，都由 pool 做。
  - 和 decider 的「命令出、事件進」相合：每個命令是一件工作，做完的結果是一個事件。
- **規則**：
  - pool 裡的一件工作只能等 I/O，不能等另一件工作，否則會互相等待而卡死。
  - 每個 stage 仍有自己的上限。renamers 4（2、4、8、16 在 WekaFS 與 NFS 上一樣快）和 stat 8（同時最多問檔案系統 9 件）是量出來的。
  - 空出來的執行緒先給最下游的 stage，先把記憶體裡的資料寫掉，再讀更多。
- **沒有現成的函式庫剛好合用。** rayon 是給 CPU 計算用的，在裡面做會卡住的 I/O 會餓死其他工作；tokio 的 `spawn_blocking` 沒有每個 stage 的上限。要自己寫。
- **風險**：這些 worker 大部分時間卡在檔案系統的呼叫裡。NFS 上的速度取決於同時有幾件 I/O 在路上，只用 8 個 worker 就慢了一成（`pipeline.md:55-57`）。總數 N 要在 fatcat 的 NFS、nano4 的 WekaFS、本機上量過才定，也可能要依檔案系統調整。
- **證據**：歷史上 stage 機制相關的 bug，它大約擋得住 2 到 3 個。價值主要在精簡與一致。

## 五、不建議的替換

- **async（tokio）。** 這一點是我自己的判斷，清點沒有評估。
  - 檔案系統的呼叫一樣會卡住，最後還是丟到 `spawn_blocking`；
  - 要整個重寫，正好撞上「bug 跟著重寫走」。
- **client 改用事件迴圈的套件（mio、polling）。** monitor 的核心本來就是不做 I/O、事件驅動的，問題出在顯示管道，不在執行緒模型。
- **用 io_uring 做 statx。** 在 NFS 上照樣經 io-wq 阻塞執行，還會打破「同時最多 8 件」的上限。
- **用 extsort 取代 `sorted.rs`。** extsort 用有名字的暫存檔，當掉時會留下檔案，也不能依鍵查詢。
- **transport 的壓縮執行緒併進 `pool.rs`**：理由見第四節。
- **data pool。** 和 channel 沒有本質上的差別，討論過了。
- **signal-hook、indicatif。** 解決不了現有的問題，或者省下的比加上的少。
- **gRPC 等 RPC 框架。** 已決定不用。

## 六、效能（都沒量）

照「改進時才量」，這裡只記證據：

- **transport 在本機，每個 byte 大約要複製 6 次。**
  - 接收端在 `Core` 的鎖裡複製（`pipes.rs:1393`、`:1455`，在 `:1756-1761` 的鎖裡）；
  - 取得這把鎖沒有包在 `profile::wait` 裡，所以 profile 看不到搶鎖。
- **先 stat 再建立或改名。** `make_directory` 對每個新目錄先 stat（`apply.rs:1071`），`clear_the_way` 改名前也先 stat（`apply.rs:1102`）。這和「在使用點偵測」的原則相反，NFS 上每次多一個來回。`rename.rs:151-160` 則是先做，遇到 EEXIST 才處理。
- **檔案系統不給檔案類型時**（例如 NFSv3 沒有 READDIRPLUS、XFS 的 ftype=0），lister 會一個一個 stat（`scan.rs:656`、`:702`）。
- **遠端每收一個封包，就送一個 monitor 事件**（`follow.rs:242` 到 `monitor/client.rs:309`）。遠端的 dry run 每列一條路徑就是一個事件。
- **NFS 拒絕 O_TMPFILE 之後**，每個檔都要鎖一次全域的 Mutex（`fsys.rs:908-912`）。
- `future.md` 已列的幾項照舊。

## 七、該保留的

- **兩個決策者**：不做 I/O 的 decider 與 owner，命令出、事件進，用假造的事件測試。
- **`Stop` 與 `Stages`**：第一個失敗是原因，`Stopped` 只是後果。`pool.rs` 有上限、保序的 pool。
- **集中判斷連線存活**：monitor 的 `Core` 是不做 I/O 的狀態機，加上 scope。每個等待都掛在 scope 上，取消時一次 poll 就醒，不需要計時器。
- **transport**：
  - pipes 的位元組 window：讀取端永不等待，三條防止死結的規則都有記載（`architecture.md:325-348`）；
  - 不做 I/O 的 `Core` 與 `Inbound`，以及它們的 fuzz；
  - 自己的多工；不採用 gRPC、QUIC 的理由也有記載。
- **檔案**：
  - 暫存檔由登記物件擁有，用 RAII 的 `disarm`（b4f9c7e 之後，這類只剩一個修正）；
  - O_TMPFILE、批次改名與 gate 的組合；DST-18 的 alias 防護。
- **讀檔與比對**：
  - fd 相對、`NOFOLLOW`、`one_name` 的紀律；
  - `sorted.rs` 的 `Keyed`，7 處共用；
  - `manifest::Check` 作為驗證的邊界。
- **其他**：
  - filter 照 rsync 移植，附 2,078 行和 rsync 對照的案例；
  - 測試的注入點（`inject.rs`）、`test_support::within`、5 個 fuzz target、每次 push 都和真的 rsync 比對。

## 順序

每一步一個 commit，`make check` 通過，並和上一步輪流量時間。動到 transport 或 monitor 的步驟，還要在 macOS CI 與真實主機上驗收，包括 fatcat 與 nano4。

0. **基準。** 本機：200,000 個 4 KiB 的小檔；4 GiB 的大檔，結果是雙峰，比較最小值。fatcat：NFS 上的小檔。
1. **修缺陷，每個附測試。**
   - 先修 1、2、3、5、6；
   - 7 要先實跑 rsync，8 要先在 LDAP 主機上確認。
2. **輸出出口與路徑型別**（模組 1），**錯誤分類**（模組 2）。
3. **第一個原因、`spawn(role)`、定時與 telemetry、顯示管道**（模組 3 到 6）。同一步裡把一次傳輸改成一輪的 watch，結束碼集中，`on_exit` 改成登記表。
4. **測試改走真 pipes，刪掉模擬器**；之後才改 pipes 的內部，以及主機輸出的標記（模組 7）。
5. **decider 的拆分**，接著**執行者改用共用的執行緒池**（第四節，也修掉缺陷 10），以及**檔案系統層**（模組 8）。
6. **效能項目**，每一項都先量。

## 風險

1. **重寫本身會帶來 bug。** 這是證據最強的一條（圖 1）。所以一次只動一區，每一步都能單獨退回。
2. **平台與環境的問題約占三成，任何模組都擋不住。**
   - 例如 macOS 上靠 Unix socket 遞送 fd、用訊號協調、`shutdown(read)` 會丟掉沒讀的資料。
   - 能做的是：少用作業系統的邊角語意，保留 macOS CI，並考慮加一個跑真 sshd 的容器測試。`tests/remote.rs` 裡手寫了 6 種假 ssh，行為和 sshd 不同。
3. **執行緒池在 NFS 上可能變慢。** 見第四節。
4. **`Report` 改成帶位元組，會牽動 client、server 與 dashboard。** client 與 server 本來就要是同一個 build（先比對 `hello_build` 才解碼），所以不必相容舊版。不過這是協定上的改動。
5. **證據強弱不一。** 模組 1、2 有實際的缺陷支撐；模組 4 的執行緒部分，歷史上只擋得住 2 到 3 個。
6. **這份是讀程式碼得到的。** 缺陷都沒有實際重現，修的時候要先寫出會失敗的測試。

## 已決定（2026-11-20）

1. **缺陷先修**，排在任何模組之前，每個都先寫出會失敗的測試。1 最優先，再來是 2、3、5、6；7 先實跑 rsync，8 先在 LDAP 主機上確認。
2. **EMFILE、ENFILE 照 rsync，逐件失敗**，不當成 Stop。現在就是這樣，所以不必改。
3. **「not deleting」這類警告照表格，每類一條、結尾彙總**，在模組 2 時改。
4. **測試改走真 pipes，和「不在同一個 process 裡跑兩個 peer」不衝突。** 那條原則說的是正式的設計。
5. **做共用的執行緒池**，在第 5 步。總數 N 在 fatcat、nano4 與本機上量過再定。
6. **刪掉沒有人用的程式碼**：`sync`、`prepare::prepare`、testkit 專用的第二套 Ctrl-C。需要時再加回來。
7. **效能的標準**：輪流量測，中位數（4 GiB 看最小值）比上一步慢、而且慢的幅度超過量測本身的離散程度，就不通過，並且要找出原因。
8. **decider 的拆分這次就做**，使用者說「如果值得就來做」。判斷是值得：
   - 接下來的步驟會一再改到 decider：缺陷 1、2，路徑型別，錯誤分類，執行緒池的派工。結構先整理好，這些改動都比較容易。
   - 每個檔案的狀態改成一個 enum，非法的組合就寫不出來，像缺陷 2 那種「某個狀態忘了釋放」也比較難發生。
   - 時機：在第 5 步、執行緒池之前。`Dispatch`（佇列、在途數與上限）正是 decider 接上 pool 的地方。
   - 不和缺陷 2 的修正混在一起：修缺陷用最小的改動，重構另成一步。
   - 它的 35 個測試留下來，當作拆分之後的整合測試。

## 進度

**第 1 步，2026-11-20：** 缺陷 1、2、3、5、6、7、8 都修了。每個都先寫出會失敗的測試，`make check` 通過，各一個 commit，CHANGELOG 也寫了。都沒有 push。

| 缺陷 | commit | 怎麼確認的 |
|---|---|---|
| 1 sync 刪掉錨定規則保護的 | 335c0ce | 先實跑重現：rsync 保留、23，Ferry 刪掉、0；加了和 rsync 對照的測試 |
| 2 分段狀態不釋放 | c9677cf | decider 的測試看到什麼都沒留下 |
| 3 控制字元到終端機 | 8ad6a1c | 測試：不存在、名字帶 ESC 的 SOURCE，最後一行印成 `\#033`，和 rsync 一樣 |
| 5 前綴重複 | 3100120 | 把測試改成比對整句，先看到 `the stream broke: the stream broke: …` |
| 6 SOURCE 是檔案時跟隨 symlink | fbfdcb9 | 兩個舊測試寫的是舊行為，在 ed7291b、acae230 之前寫的，跟著改了 |
| 7 消失的檔案說成讀不到 | 2a6a647 | rsync 用讓 `open`、`opendir` 回 ENOENT 的 shim 實跑：vanished、24、DEST 的保留；Ferry 用延後 readers、中途刪檔實跑 |
| 8 家目錄沒走 getent | 4b800bb | 在 fatcat 確認前提：帳號只在 SSSD、不在 `/etc/passwd`、沒有 nscd，`getent` 找得到家目錄；沒有在 musl build 上實跑 |

和計畫不同的地方：

- **缺陷 6 只修了前半。** 根目錄先用路徑 stat、再用路徑打開，兩次之間沒有比對 dev/ino，這一半沒做。範圍更窄，rsync 也不查，而且要可靠地測，需要一個新的注入點。
- **第 0 步的基準還沒量。** 這些修正都不在熱路徑上，所以延到第 2 步之前。
- 缺陷 4、9 到 13 照計畫，在後面的步驟由模組一起處理。

**一個額外的缺陷，2026-11-20：** 推送第 1 步之後，CI 的 Ubuntu 又出現一次那個 watch 測試的失敗，結束碼是 1，不是 23；11-13 在 macOS 上也發生過。這次 f73b24a 印出了 Ferry 說的話，原因找到了：

- 連線準備好之前，client 讀資料的迴圈（`Input::more`）會先等 scope；scope 結束了，就直接回報錯誤，不讀已經到的 server 回覆。0b75bdd 只修了準備好之後的那一組。
- 修在 0561e5a，有一個不靠時序的測試。CI 44690629797 全部通過。

**第 0 步，基準：** 5642744 的 release build，本機輪流量測。

- 起初的腳本每輪都先跑 A 再跑 B，B 總是接在一次刪掉 20 萬個檔案之後，系統性地偏慢。改成每輪交換先後，十輪。
- 主機上別的容器讓負載在 4 到 10 之間，這是離散程度的來源。
- 同一個 build 對自己：20 萬個小檔的中位數差約 2.5%；4 GiB 的最小值差 0% 到 1.5%，中位數最多差 7%，因為結果是雙峰。

**第 2 步，2026-11-20：** 模組 1 與模組 2 做完。

| commit | 內容 |
|---|---|
| 8e64f4a | 名稱在放進訊息時就跳脫，修好缺陷 4 |
| b87acfa | 單一輸出出口：`status::say`、`status::write`，`clippy.toml` 禁止其他地方用 `eprintln!` |
| bfed92e | `ShownPath` 型別；`clear_the_way` 只收相對位置；makers 顯示的路徑補上 SOURCE 的名稱 |
| 710cc00 | `errors`：錯誤的意思只在一處判斷，Source 與 Destination 都問它 |

和計畫不同的地方：

- **模組 1 沒有讓 `Report` 帶位元組，改成名稱在放進訊息時就跳脫**，和 rsync 一樣。錯誤訊息走的是 `anyhow` 的錯誤鏈，只能是 UTF-8 字串，帶位元組只顧得到回報，顧不到錯誤，會變成兩套。出口的 `printable::line` 因此改成只跳脫控制字元與 bidi 字元，已經跳脫過的名稱不會再跳一次。
- **第 3 題要重新決定。** 實跑 rsync 才發現：rsync 對含有被排除檔案的目錄，每個目錄立刻說一行「cannot delete non-empty directory: …」，以 0 結束；用 `-x` 時，另一個檔案系統上的目錄則什麼都不說。所以「每類一條、結尾彙總」和 rsync 兩種情況都不同。在使用者重新決定之前，維持原樣，也就是立刻說，並附原因。

量測：第 2 步對基準，交換先後、十輪。

- 小檔中位數 2.38 s 對 2.39 s；
- 大檔兩次，最小值分別是 2.83 s 對 2.71 s、2.66 s 對 2.69 s，中位數一次慢 5%、一次快 4%。

都在同一個 build 自己的離散範圍內，沒有變慢。
