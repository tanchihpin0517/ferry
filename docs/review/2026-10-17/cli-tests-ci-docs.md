# Ferry 審查交接：CLI/UX、測試、CI/release、文件

本文件給之後負責修正的 coding agent 使用。每一項都可以獨立閱讀，不需要看過原本的審查對話。

- 基準版本：`main` 的 HEAD `3c17a2f`（chore: version 0.14.4）。所有 `path:line` 都對照過這個版本。
- 實測環境：Linux x86_64，rustc 1.95.0，debug build（`CARGO_TARGET_DIR=.scratch/review-cli/target`）。所有測試目錄樹都放在 `.scratch/review-cli/` 底下，用完已刪除。
- 「fake ssh」指 `tests/remote.rs:15-24` 的做法：`FERRY_SSH` 指向一個 shell script，略過 ssh 的選項與 host，在本機以 `sh -c` 執行命令。搭配 `--remote-ferry <本 build>` 與 `--remote-join`，不需要真的 host 就能走完遠端流程。
- 不在本文件範圍：IPv6 `[::1]:path` 的解析，以及 release 的簽章與 provenance。這兩項由 transport 審查者負責，本文件只在 CI-07 留交叉參照。
- 用語依照 `docs/design/*.zh-TW.md`：client、server、peer、Source、Destination、manifest、record、frame、packet、thread、stage、connection、host，以及程式識別字、路徑與指令一律保留英文。handshake 寫作「交握」。

## 索引

| ID | 嚴重度 | 標題 |
|---|---|---|
| CLI-01 | 嚴重 | SOURCE 與 DESTINATION 重疊、以及危險的 DESTINATION，都沒有任何防護 |
| CLI-02 | 高 | 沒有終端機時，各種 SSH 失敗都被說成「needs a login」 |
| CLI-03 | 中 | 檔名原樣輸出：dry run 清單可被偽造，控制字元直通終端機 |
| CLI-04 | 中 | `host:~/path` 的 `~` 不會展開，複製直接失敗 |
| CLI-05 | 中 | `FERRY_SSH` 不接受參數，但文件說它「as rsync's RSYNC_RSH」 |
| CLI-06 | 中 | 結束狀態 2 同時代表用法錯誤與「有新版」，也分不出部分失敗 |
| CLI-07 | 低 | 顯示端的錯誤（EPIPE）蓋過傳輸結果；`\| head` 時印 Broken pipe 並 exit 1 |
| CLI-08 | 低 | 每次互動式執行結束都印出約 40 行「where the time went」 |
| CLI-09 | 低 | 儀表板的 raw mode 下，Ctrl-\（快照）與 Ctrl-Z 都失效 |
| CLI-10 | 低 | 參數驗證與錯誤訊息的零碎問題 |
| TST-01 | 高 | BLAKE3 雜湊不符的處理路徑沒有任何測試 |
| TST-02 | 高 | 需要 root 的行為（擁有者對應、chown、setuid、裝置、`--numeric-ids`）在 CI 從未執行 |
| TST-03 | 中 | 不受信任的 peer 可觸及的解碼器沒有 fuzz 與 property test |
| TST-04 | 中 | 會卡死或空洞通過的測試 |
| TST-05 | 低 | 「中斷後重跑只送缺的」沒有端到端驗證 |
| TST-06 | 低 | 沒有驗證 fsync 真的被呼叫 |
| TST-07 | 低 | 儀表板在極小終端機尺寸下沒有回歸測試 |
| CI-01 | 中 | 測試沒有逐項逾時：卡住就耗掉 30 分鐘的 job timeout，也不知道是哪個測試 |
| CI-02 | 中 | 沒有 cargo-deny（advisory、授權），release 也沒附第三方授權聲明 |
| CI-03 | 中 | GitHub Actions 以可變的 tag 固定，沒有用 commit SHA，也沒有 Dependabot |
| CI-04 | 低 | 沒有 MSRV，toolchain 浮動，release 不可重現 |
| CI-05 | 低 | 推 tag 就 release，不要求該 commit 的 CI 已通過 |
| CI-06 | 低 | macOS 二進位檔沒有簽章與公證（Gatekeeper） |
| CI-07 | 中 | （交叉參照）release 沒有簽章與 provenance |
| DOC-01 | 高 | overview 的「Guarding against a mistyped DESTINATION」仍寫「待決定」 |
| DOC-02 | 中 | TODO.md 仍描述已移除的 resend pass 與「最後才刪除」 |
| DOC-03 | 中 | 「讀取中被改的檔案下次一定會被發現」是過度承諾 |
| DOC-04 | 低 | overview 把不存在的 `--verify` 列在「Decided」 |
| DOC-05 | 低 | 結束狀態表不完整 |
| DOC-06 | 低 | 環境變數表缺 `FERRY_SUDO_PASSWORD`、`NO_COLOR`，`FERRY_SSH` 的描述會誤導 |
| DOC-07 | 低 | README 的 `rsync --partial` 一列暗示可以續傳 |
| DOC-08 | 低 | `ferry test` 的 synopsis 缺 `--ssh-connections` |
| DOC-09 | 低 | 缺 CHANGELOG 與 SECURITY.md |

文末另有「其他小問題」。

---

## CLI/UX

### CLI-01：SOURCE 與 DESTINATION 重疊、以及危險的 DESTINATION，都沒有任何防護

- 嚴重度：嚴重
- 驗證狀態：已實測重現。在 `.scratch/review-cli/t/` 建 `a/f1`、`a/sub/f2`，以 debug build 執行下列各情境。`host:` 的情境用 fake ssh，並把 ferry 的工作目錄設為假的家目錄來重現。`--remote-sudo` 搭配 `host:/` 的情境是 Destination 審查者依程式碼推得，未實測。
- 位置：
  - `src/cli.rs:397-444`：`transfer()`。只檢查本機 Source 是否存在（405-408），之後直接 open transport、判斷種類、啟動 server。
  - `src/cli.rs:640-669`：`root_kind()`，只回報 directory、other 或 missing。
  - `src/cli.rs:684-718`：`place_destination()`，只比對兩邊的種類，不比對身分。
  - `src/cli.rs:765-768`：`remote_path()`，空的遠端路徑變成 `.`，也就是 host 的家目錄。
  - `src/cli.rs:585-627`：`start()`。交握後立刻 `transport.connect()`，peer 隨即開始執行。
  - `src/protocol.rs:48-55`：`Welcome` 只有 `token` 與 `socket`。
  - `src/protocol.rs:74-108`：`Hello` 不含另一側的任何資訊。
  - `src/peer.rs:36-50`：`serve()` 的交握。
  - `src/session.rs:60-90`：`server_handshake()`。
  - `src/destination.rs:1877-1890`（`clear_the_way`）、`src/destination.rs:1895-1940`（`delete_extras`）、`src/fsys.rs:531-550`（`remove_tree`）：刪除前不檢查被刪的是不是 Source。
  - `docs/design/overview.md:188-189`：保證「待實作刪除時再決定」，但刪除早已實作。
- 現象（實測）：
  1. `ferry sync a/sub a`（Source 在 Destination 裡）：`a/f1` 與 `a/sub`（也就是 Source 本身）都被刪掉，`a` 變成空目錄，`f2` 永久遺失。輸出只有 `ferry: cannot read f2: No such file or directory (os error 2)` 和 `ferry: 1 path in a/sub could not be read`，exit 1。使用者看不出資料是 Ferry 自己刪的。
  2. `ferry sync a a/sub`（Destination 在 Source 裡）：`a/sub/f2` 是 Source 的檔案，卻在 Source 讀到它之前就被當成 `a/sub` 的多餘項目刪除。輸出 `cannot read sub/f2`，exit 1。
  3. `ferry copy a a/sub`：產生 `a/sub/f1`、`a/sub/sub/f2`，exit 0。Ferry 把樹遞迴複製進自己。這次只多了一層，但掃描與寫入同時進行，樹越大、層數越不可預期（推測）。
  4. `ferry sync src host:`（以 fake ssh 重現）：假家目錄中的 `important/thesis.tex` 被刪，只剩 `new`。exit 0，沒有任何詢問。換成真的 host，被清空的就是使用者的家目錄。忘了在 `host:` 後面打路徑，是很常見的手誤。
  5. `ferry sync a a`：exit 0，什麼也沒做。這是無害的，但應該明確拒絕。
  6. `ferry sync --remote-sudo ./empty host:/`（依程式碼推得）：server 以 root 執行，會刪除 host 上整個 `/`。
- 根本原因：
  1. client 準備階段只看兩邊的「種類」：

     ```rust
     // src/cli.rs:435-444
     let source_kind = root_kind(&source_transport, &source)?;
     let destination_kind = root_kind(&destination_transport, &destination)?;
     let destination = place_destination(cli, delete, &source, source_kind,
         destination, destination_kind)?;
     ```

     `place_destination` 的 `match (delete, source_kind, destination_kind)` 只拒絕「目錄同步到檔案」之類的組合，從不比較兩個根目錄是否相同或互相包含。
  2. 遠端的空路徑被默默換成家目錄：

     ```rust
     // src/cli.rs:765-768
     fn remote_path(path: Vec<u8>) -> Vec<u8> {
         if path.is_empty() { b".".to_vec() } else { path }
     }
     ```

  3. 兩個 server 各自只知道自己的根目錄（`Hello.root`），client 也不知道它們在 host 上的真實身分：canonical path、`dev`/`ino`、是不是同一台機器。
  4. `sync` 依 commit ba3ed6e 一邊比較一邊刪除：Destination 一拿到某目錄完整的 manifest 就刪掉它多出來的項目（`delete_extras`）。所以當 Source 在 Destination 裡面時，Source 還沒讀到的子樹會先被當成多餘項目刪掉。
  5. `remove_tree` 與 `clear_the_way` 刪除目錄前，不檢查這個目錄是否就是 Source 的根目錄或它的祖先。
- 重現方式：

  ```sh
  F=$PWD/.scratch/review-cli/target/debug/ferry
  mkdir -p .scratch/review-cli/t && cd .scratch/review-cli/t
  rm -rf a && mkdir -p a/sub && echo one > a/f1 && echo two > a/sub/f2
  $F sync a/sub a </dev/null; echo "exit=$?"; find a     # 只剩 a
  rm -rf a && mkdir -p a/sub && echo one > a/f1 && echo two > a/sub/f2
  $F sync a a/sub </dev/null; echo "exit=$?"; find a     # a/sub/f2 不見
  rm -rf a && mkdir -p a/sub && echo one > a/f1 && echo two > a/sub/f2
  $F copy a a/sub </dev/null; find a                     # a/sub/sub/f2
  # host: 的情境（fake ssh；ferry 的工作目錄扮演 host 的家目錄）
  printf '#!/bin/sh\nwhile [ "$1" != -- ]; do shift; done\nshift 2\nexec sh -c "$1"\n' > fake-ssh
  chmod +x fake-ssh
  mkdir -p src fakehome/important ferryhome
  echo data > src/new; echo precious > fakehome/important/thesis.tex
  (cd fakehome && FERRY_SSH=../fake-ssh FERRY_HOME=$PWD/../ferryhome \
     $F sync --remote-ferry $F --remote-join -y ../src 'host:' </dev/null)
  find fakehome                                          # thesis.tex 被刪，exit 0
  ```

- 修法（建議分成四部分）：

  **一、server 在交握時回報根目錄的身分。**

  在 `src/protocol.rs` 新增：

  ```rust
  /// A directory or file, as one kernel knows it: comparable only between two
  /// servers whose `kernel` is the same.
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
  pub struct FileKey { pub dev: u64, pub ino: u64 }

  #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
  pub struct RootIdentity {
      /// The running kernel: Linux /proc/sys/kernel/random/boot_id,
      /// macOS sysctl kern.bootsessionuuid.
      pub kernel: String,
      /// The root's canonical path; for a root not there yet, its nearest
      /// existing ancestor's, joined with the rest.
      pub canonical: Vec<u8>,
      pub exists: bool,
      /// The root, if it exists, then each ancestor up to `/`, nearest first.
      pub chain: Vec<FileKey>,
      /// Canonical home directories of the users this server acts for.
      pub homes: Vec<Vec<u8>>,
      /// Network filesystem (NFS, SMB, Lustre, GPFS, Ceph…) and its f_fsid.
      pub network_fs: Option<u64>,
  }
  ```

  欄位說明：
  - `Welcome` 加上 `pub root: RootIdentity`。server 只接受完全相同 build 的 client（`session.rs:72-78`），所以改 protocol 不必考慮相容性。
  - 新增 `fsys::root_identity(root: &[u8]) -> io::Result<RootIdentity>`，由 `peer::serve` 中 `server_handshake` 的 `joinable` 閉包呼叫（`peer.rs:42-50`）。
  - `chain`：照現在的方式打開根目錄（跟隨根目錄的 symlink，與 `stat_root`、`open_root` 一致）。之後反覆 `openat(fd, "..", O_DIRECTORY)` 再 `fstat`，直到 `(dev, ino)` 不再變化。根目錄不存在時，從最近的既存祖先開始，並設 `exists = false`。
  - `canonical`：`std::fs::canonicalize`，不存在的根目錄則用「最近既存祖先的 canonical path」加上剩下的部分。這只用在錯誤訊息與家目錄比對，判斷包含關係一律用 `chain`。
  - `homes`：`$HOME`、`getpwuid(getuid()).pw_dir`，以及帶 `--sudo` 時 `getpwuid(SUDO_UID).pw_dir`。`join.rs:100` 已經讀過 `SUDO_UID`，可以參考。全部做 canonicalize，去掉重複。
  - `kernel`：Linux 讀 `/proc/sys/kernel/random/boot_id`。macOS 用 `sysctlbyname("kern.bootsessionuuid")`，需實測確認存在與格式。讀不到就填空字串，此時視為「無法判斷是否同一 kernel」。
  - `network_fs`：`statfs` 的 `f_type` 屬於網路檔案系統時，填入 `f_fsid`。

  **二、什麼算「同一台機器」。**

  `dev`/`ino` 只有在同一個執行中的 kernel 裡才有意義，所以比較的單位是 kernel，而不是 machine-id：
  - 本機對本機：兩個 server 都是 client 的子行程，`kernel` 必然相同。
  - 兩個指向同一台機器的 host 別名，或 `localhost:` 對本機路徑：`boot_id` 相同，可以偵測到。
  - 同一台 host 上的 container：共用 host 的 kernel，`boot_id` 相同。bind mount 的 `dev`/`ino` 也正確可比，這正是我們要的。
  - 從同一映像檔複製出來的 VM：machine-id 常常相同，但 `boot_id` 不同，不會被誤判。這就是不用 machine-id 的理由。
  - 不同 kernel：`chain` 不可比。例外是網路檔案系統，例如 NFS 共用的家目錄，或 HPC 的 Lustre、GPFS（如 `login1:/work/x` 對 `login2:/work/x/sub`）。兩邊的 `network_fs` 相同且 `chain` 的 inode 相交時，視為重疊。這一段是推測：需要在真的 NFS 上量測 `f_fsid` 與 `st_ino` 是否跨 client 一致。若不一致，就把這一點列為已知限制並寫進文件，不要用標記檔之類的寫入式偵測（dry run 不能寫）。

  **三、client 端的規則。**

  新增純函式模組 `src/guard.rs`：`pub fn check(source: &RootIdentity, destination: &RootIdentity, delete: bool, destination_location: &Location) -> Verdict`，其中 `enum Verdict { Ok, Refuse(String), Ask(String) }`。

  | 情況 | 判定方式 | `sync` | `copy` |
  |---|---|---|---|
  | 兩邊是同一個目錄或檔案 | 同一 kernel，`source.chain[0] == destination.chain[0]` | 拒絕 | 拒絕 |
  | Destination 在 Source 裡 | `source.chain[0]` 出現在 `destination.chain[1..]`；Destination 不存在時用其祖先的 chain | 拒絕 | 拒絕（會把自己遞迴複製進去） |
  | Source 在 Destination 裡 | `destination.chain[0]` 出現在 `source.chain[1..]` | 拒絕（會刪掉 Source） | 拒絕（Destination 會寫進 Source 正在讀的樹） |
  | Destination 是 `/` | `destination.chain` 長度為 1 | 詢問 | 允許 |
  | Destination 是某個家目錄，或是家目錄的祖先（`/home`、`/Users`） | 比對 `destination.canonical` 與 `destination.homes` | 詢問 | 允許 |
  | 遠端路徑為空（`host:`）或 `host:~`（見 CLI-04） | 文字判斷，訊息中特別點出「`web1:` 就是 web1 上的家目錄」 | 詢問（由上一列的家目錄規則觸發） | 允許 |
  | 網路檔案系統上推測為重疊 | 見第二部分最後一點 | 拒絕，可用旗標覆寫 | 拒絕，可用旗標覆寫 |
  | mount point 的根目錄（如 `/mnt/backup`） | 不檢查 | 允許 | 允許 |

  mount point 的根目錄不擋：把外接硬碟的根目錄當備份目標，是 `sync` 最正常的用法。`/` 雖然也是 mount point，但另外列出。

  處置方式：
  - 重疊（前三列）：一律拒絕，沒有覆寫旗標。結果從來不會是使用者想要的，而且會毀損資料。需要時可以先複製到不重疊的暫存位置。
  - 危險的 Destination（`sync` 進 `/`、家目錄或家目錄的祖先）：在終端機上用 `remote::confirm()` 詢問。沒有終端機時拒絕，除非給了新旗標 `--allow-dangerous-destination`。
  - 不要讓 `--yes` 涵蓋這一類。`--yes` 已經被 cron 與腳本拿來同意在 host 上安裝 Ferry（`cli.rs:96-99`）。一旦讓它也同意清空家目錄，就會把一個無害的旗標變成危險的旗標。
  - `--dry-run`：重疊照樣拒絕，讓使用者及早發現。危險的 Destination 不詢問，只多印一行 `ferry: warning: a real run would ask before deleting in <path>, the home directory on web1`。
  - 訊息範例：
    - `ferry: a/sub is inside a, the Source: syncing would delete the Source. Choose a DESTINATION outside it.`
    - `ferry: web1: is the home directory on web1; sync would delete everything in it that ./site does not have. Sync into it anyway?`

  **四、流程改為兩階段交握，讓檢查發生在任何改動之前。**

  目前 `start()` 交握後立刻 connect paths（`cli.rs:609-619`），server 的 peer 隨即開始執行。Destination 在 relay 開始前收不到 manifest，所以實際上還不會刪除任何東西，但這是靠時序保證的。建議改成明確的流程：
  1. `start()` 拆成 `handshake()`（回傳 control connection 與 `Welcome`）與 `connect()`。
  2. `transfer()` 先對兩邊 `handshake()`，取得兩份 `RootIdentity`，呼叫 `guard::check()`，處理 `Refuse`/`Ask`，最後才 `connect()`。
  3. 被拒絕時直接 drop control connection。server 在 control connection 關閉時就結束（`peer.rs:55-63`、`overview` 的 server 定義），而且還沒碰過任何檔案。
  4. 新增 `Control::Proceed(Guard)`：client 在 `connect()` 之前，於 control connection 上送給每個 server。server 在 `server_handshake` 之後、`joining.accept()` 之前，用 `protocol::expect::<Control>` 讀取它；讀到 EOF 就安靜結束。注意 `peer.rs:55-63` 起 stdin 被丟進 sink，所以必須在那之前讀。

  **五、傳輸中的防線（runtime guard）。**

  起點的檢查擋不住 bind mount：例如 `/data/b` 是 `/backup` 的 bind mount，而使用者執行 `ferry sync /data /backup`。此時 Destination 的 chain 是 `/backup`、`/`，不包含 `/data`。掃描到 `/data/b` 時，會開始讀 Destination 正在寫的樹。所以要加上兩道防線：

  ```rust
  /// What a peer must never go into or delete, as the client found it.
  #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
  pub struct Guard {
      /// For the Source: the Destination's root, when on the same kernel.
      pub avoid: Vec<FileKey>,
      /// For the Destination: the Source's root and its ancestors.
      pub protect: Vec<FileKey>,
  }
  ```

  - Source：`source::Options` 增加 `guard`。scan 每打開一個目錄（`scan.rs` 中列出目錄、取得 fd 之後）就 `fstat` 一次。若 `(dev, ino)` 屬於 `avoid`，整個傳輸以錯誤停止：`the Destination's root is inside the Source, at <path>`。不要選擇略過：略過會讓 `sync` 把對應的路徑當成多餘項目刪除。
  - Destination：`destination::Options` 增加 `guard`。`fsys::remove_tree` 遞迴到每個目錄時，以及 `clear_the_way`、`delete_extras` 刪除目錄前，都對該目錄 `fstat`。若屬於 `protect`，就拒絕刪除並讓傳輸失敗。另外，Destination 進入或建立要寫入的目錄時（`target_directory`、`open_under`），若 `(dev, ino)` 等於 Source 的根目錄，也停止傳輸。這些都只是在既有的 `stat` 之外多一次 `fstat`，成本很低。

  **六、（選做）`--max-delete N`。**

  仿照 rsync：`TransferArgs` 加上 `max_delete: Option<u64>`，`Hello` 加上 `max_delete`。Destination 的 `State` 加一個計數器。`remove_tree` 每刪一個項目就加一並檢查，超過時停止刪除，回報 `stopped deleting after N entries: --max-delete N`，並計入 failed，讓結束狀態非 0。dry run 也照樣計數並警告。注意 `remove_tree` 是遞迴的，額度要傳進遞迴，不能只算最上層的項目。

  邊界情況：
  - 根目錄是檔案（`copy file dir/file`）：`chain[0]` 是檔案本身，「同一檔案」也要拒絕。
  - 根目錄是 symlink：照現行語意跟隨，比對的是它指向的目錄，因此 `sync a link-to-a` 也會被拒絕。
  - 不分大小寫的檔案系統：用 `dev`/`ino` 比對，不用路徑文字，不受影響。
  - `sudo ferry` 在本機執行：`homes` 會包含 `/root`。
  - `ferry test`：fixture 的 `ferry-test-*-source` 與 `ferry-test-*-destination` 是兄弟目錄，不會觸發檢查。即使使用者給的 SOURCE 與 DESTINATION 是同一個目錄也一樣，要確認 `testkit` 的情境全部仍然通過。
- 要加的測試：
  - `src/guard.rs` 的單元測試：用手造的 `RootIdentity` 表格化測試上表每一列，包含 `exists = false` 與不同 `kernel`。
  - `tests/sync.rs`：
    - `a_destination_inside_the_source_is_refused_and_nothing_changes`：`sync a a/sub` 失敗，訊息含 `inside`，`a` 的內容與 mtime 完全不變。
    - `a_source_inside_the_destination_is_refused_and_nothing_changes`：`sync a/sub a`。
    - `the_same_directory_is_refused`：`sync a a`，以及 `sync a link-to-a`。
    - `a_destination_not_there_yet_inside_the_source_is_refused`：`sync a a/new`。
  - `tests/copy.rs`：`copying_into_itself_is_refused`（`copy a a/sub`）。
  - `tests/remote.rs`（fake ssh；子行程的 `current_dir` 與 `HOME` 都設為暫存的假家目錄）：
    - `syncing_into_a_hosts_home_is_refused_without_a_terminal`：`sync src host:`。stdin 為 null，執行前以 `pre_exec` 呼叫 `setsid()`，確保沒有 `/dev/tty`。斷言失敗、訊息含 `home directory`，假家目錄原封不動。
    - `allow_dangerous_destination_lets_it_through`。
    - `syncing_from_a_host_into_itself_is_refused`：`sync host:X host:X/sub`。兩邊都是 fake ssh，`boot_id` 相同。
  - root 專用（放進 TST-02 的 root job）：bind mount 的情境。`mount --bind /tmp/x/backup /tmp/x/data/b` 後執行 `sync /tmp/x/data /tmp/x/backup`，斷言以 `inside the Source` 失敗，且 backup 沒有遞迴長出子樹。
  - dry run：`sync -n src host:` 印出 warning 並以 0 結束。`sync -n a a/sub` 被拒絕。
- 驗收條件：
  - [ ] 上述六個重現情境全部在任何改動前被拒絕（或詢問），樹的內容與 mtime 完全不變。
  - [ ] 沒有終端機時，`sync` 進 `/` 或家目錄被拒絕；給了 `--allow-dangerous-destination` 才進行。`--yes` 單獨給時仍然拒絕。
  - [ ] `copy` 進家目錄（如 `ferry copy host:dotfiles ~`）不受影響。
  - [ ] 不同 kernel、非網路檔案系統的兩個路徑永遠不會被誤判為重疊。
  - [ ] bind mount 情境在傳輸中被偵測並停止，Destination 刪不到 Source。
  - [ ] `make check` 與 `ferry test`（本機，以及 `--ssh-connections 2`）全部通過。
- 文件要同步的地方：
  - `docs/cli.md` 與 `docs/cli.zh-TW.md`：
    - 「SOURCE and DESTINATION themselves are looked at first…」那一段之後，新增一節說明重疊一律拒絕、危險的 Destination 會詢問，以及 `--allow-dangerous-destination`。
    - Options 表加上新旗標（若做 `--max-delete` 也一併加上）。
    - Locations 表中 `host:` 一列註明 `sync` 會詢問。
  - `docs/design/overview.md` 與 `overview.zh-TW.md`：「Behaviour of `ferry sync`」整節，見 DOC-01。
  - `docs/design/architecture.md` 與 `architecture.zh-TW.md`：交握的描述加上 `RootIdentity` 與 `Control::Proceed` 的兩階段流程。
  - `docs/design/pipeline.md` 與 `pipeline.zh-TW.md`：Source 的 scan 與 Destination 的刪除各加一句 runtime guard 的說明。
  - `TODO.md`：新增一個步驟，例如「34. Guards」。
  - `README.md`：Usage 中 `ferry sync` 範例旁可加一句「a DESTINATION inside SOURCE, or the reverse, is refused」。
- 相關項目：DOC-01、CLI-04、TST-02、CLI-06（被拒絕時的結束狀態）。

### CLI-02：沒有終端機時，各種 SSH 失敗都被說成「needs a login」

- 嚴重度：高
- 驗證狀態：已實測重現。`FERRY_SSH=/nonexistent ferry copy -n src somehost:x </dev/null`，以及對無法解析的 `nohost.invalid` 執行時，都只印出 `ferry: somehost: SSH connection 1 of 1 needs a login, and there is no terminal to type it in`。
- 位置：
  - `src/transport/mod.rs:325-350`：`log_in()`。
  - `src/transport/mod.rs:352-379`：`hold()`。
  - `src/transport/ssh.rs:78-99`：`hold_quietly()` 與 `program()`。
- 現象：在 cron、CI 或 `ssh a 'ferry …'` 這類沒有終端機的情境下，只要第一次靜默登入（`BatchMode=yes`）失敗，訊息一律是「需要登入但沒有終端機」。實際原因可能是 DNS 解析失敗、網路不通、host key 改變（`REMOTE HOST IDENTIFICATION HAS CHANGED`）、`FERRY_SSH` 指向不存在的程式、port 錯誤或 `ProxyJump` 失敗。使用者會被誤導去設定金鑰。
- 根本原因：
  1. 靜默嘗試把 ssh 的 stderr 丟到 `Stdio::null()`，失敗原因被整個丟掉：

     ```rust
     // src/transport/mod.rs:331-346
     let quietly = { /* … */ hold(ssh, ssh.hold_quietly(), Stdio::null()) };
     if let Ok(holder) = quietly {
         return Ok(holder);
     }
     if std::fs::File::open("/dev/tty").is_err() {
         bail!("{}: {which} needs a login, and there is no terminal to type it in",
               ssh.host());
     }
     ```

  2. `quietly` 的 `Err` 內容（包括 `spawn` 的 ENOENT，以及 `could not log in (exit status: 255)`）完全沒被使用。
- 重現方式：

  ```sh
  cd .scratch/review-cli/t && mkdir -p src
  FERRY_SSH=/nonexistent $F copy -n src somehost:x </dev/null     # 說 needs a login
  $F copy -n src nohost.invalid:x </dev/null                      # 同上
  ```

- 修法：
  1. `hold()` 增加回傳 ssh 的 stderr。不要用 pipe：ssh 的 ControlMaster 加上 `ControlPersist` 時可能 fork 出背景 master，繼承 pipe 的寫端，讀到 EOF 會永遠卡住。改把 stderr 導到一個開好再 unlink 的檔案，例如 `/tmp/ferry-<uid>/ssh/` 底下（該目錄已存在，mode 0700），失敗後從頭讀出最後幾行。
  2. `log_in()` 依失敗類型給不同訊息：
     - `spawn` 失敗：`cannot run {program} (FERRY_SSH): No such file or directory`，其中 program 取自 `FERRY_SSH` 或 `ssh`。
     - ssh 結束且 stderr 含 `Permission denied`、`Authentication failed`、`keyboard-interactive`、`password`：維持現在的「needs a login」訊息，並附上 ssh 的那一行。
     - 其他：`{host}: {which} failed: {ssh 的最後一行 stderr} ({status})`。不要說需要登入。
  3. 有終端機時，第二次（互動）嘗試已經會把 ssh 的 stderr 直接顯示，維持不變。但若互動也失敗，錯誤訊息應附上靜默嘗試的原因。
  4. 不要用 ssh 的 exit code 判斷：255 同時代表連線錯誤與認證失敗。
- 要加的測試：在 `tests/remote.rs` 仿照 `an_ssh_connection_that_cannot_log_in_fails_before_anything_changes`（`tests/remote.rs:183-216`）：
  - `an_unreachable_host_says_why`：fake ssh 對任何呼叫都 `echo "ssh: Could not resolve hostname host: Name or service not known" >&2; exit 255`。斷言 ferry 的 stderr 含 `Could not resolve hostname`，且不含 `needs a login`。這個斷言不論有沒有 tty 都成立。
  - `a_missing_ssh_program_says_so`：`FERRY_SSH` 設為不存在的路徑，斷言訊息含 `FERRY_SSH` 與該路徑。
  - `a_login_that_needs_a_password_still_says_so`：fake ssh 印出 `Permission denied (publickey,password).` 並 exit 255，子行程以 `pre_exec(setsid)` 脫離 tty。斷言訊息含 `needs a login`。
- 驗收條件：
  - [ ] DNS、網路、host key、程式不存在這四類錯誤，訊息都包含 ssh（或作業系統）的原始原因。
  - [ ] 真的需要密碼時，訊息不變，仍然在任何改動之前停止。
  - [ ] ControlPersist 開啟時不會卡住（以設定了 `ControlPersist yes` 的 fake ssh 驗證）。
- 文件要同步的地方：`docs/cli.md` 的「SSH connections」中 `Without a terminal, or if a login fails, the run stops…` 一句，改為說明會附上 ssh 的原因。`docs/cli.zh-TW.md` 對應段落也要改。
- 相關項目：CLI-05。

> 2026-10-20 補記：WP-10 照修法做。不詢問的那一次登入，ssh 的 stderr 寫到 `/tmp/ferry-<uid>/` 下一個開好就刪掉的檔案（不是 pipe，背景的 master 繼承它也不會卡住），失敗時讀出最後三行。程式執行不了時說「cannot run X (FERRY_SSH)」，而且有終端機也不再詢問一次；沒有終端機時，ssh 的話裡有 `Permission denied`、`Authentication failed`、`keyboard-interactive` 或 `password` 才說需要登入，其餘說「failed」並附上 ssh 的原因。有終端機時照舊再詢問一次，若也失敗，錯誤附上不詢問那一次的原因。測試：`an_unreachable_host_says_why`、`a_missing_ssh_program_says_so`、`a_login_that_needs_a_password_still_says_so`（都以 `setsid` 脫離終端機）。驗收條件「ControlPersist 開啟時不會卡住」沒有以會 fork 的假 ssh 驗證，只依「用檔案而不是 pipe」推得。

### CLI-03：檔名原樣輸出：dry run 清單可被偽造，控制字元直通終端機

- 嚴重度：中
- 驗證狀態：已實測重現。
  - Source 有一個名為 `a\ndeleting important` 的檔案時，`ferry sync -n --no-ui nl nld` 的 stdout 出現獨立一行 `deleting important`。
  - 名為 `x\e[31mred` 的檔案，ESC 位元組原樣輸出。
  - 名為 `evil\nferry: all good`、權限 000 的檔案，`ferry copy --no-ui` 的 stderr 多出一行偽造的 `ferry: all good: Permission denied`。
  - 名為 `deleting important`（沒有換行）的檔案，在清單中與「刪除 important」無法區分。這一點已核對程式碼。
- 位置：
  - 清單在 Destination 產生：`src/destination.rs:813-825`（刪除行加上 `deleting ` 前綴）、`src/destination.rs:905-917`（一般行），以及 `src/source.rs:160-170`（`display()` 直接回傳原始位元組）。
  - 清單在 client 輸出：`src/progress.rs:188-200`（`stdout.write_all(&line)`）、`src/dashboard.rs:289-303`（寫入 stdout 與儀表板的最新清單面板）。
  - 警告在 server 組成：用 `String::from_utf8_lossy(&display(..))` 組成警告字串的地方有 `src/destination.rs:797`、`867`、`899`、`946`、`1408`、`1487`、`1806`、`1924`、`1980`，以及 `src/source.rs:530`、`684`。
  - 警告在 client 輸出：`src/progress.rs:201-207`（`writeln!(stderr, "ferry: {said}")`）、`src/cli.rs:532-534`（儀表板關閉後一次印出），以及 `src/cli.rs:543-548`（`cli.source.to_string_lossy()`）。
  - 報告格式：`src/report.rs:49`（`Listed(Vec<Vec<u8>>)`）。
- 現象：
  - 腳本若用 `ferry sync -n … | grep '^deleting '` 判斷會刪什麼，會被 Source 中精心命名的檔案誤導。
  - 檔名中的 ESC 序列能改變終端機狀態，例如設定標題、改顏色或游標移動。
  - 遠端 server 本身不受信任時（例如從一台被入侵的 host pull），它送來的 `Said` 字串可以直接控制使用者的終端機。
  - 儀表板經 ratatui 時會靜默丟掉控制字元（`ratatui-core-0.1.2/src/buffer/buffer.rs:351`），所以畫面上看到的名稱，與結束後 stderr 印出的名稱不一致。
- 根本原因：
  1. 路徑始終以原始位元組或 `from_utf8_lossy` 輸出，沒有任何跳脫。`docs/cli.md` 的 Output 節甚至明訂「a name that is not UTF-8 is printed as it is」。
  2. 清單格式是「每行一個路徑，刪除行加 `deleting ` 前綴」。由於路徑本身可以以 `deleting ` 開頭，也可以含換行，這個格式本身就有歧義。
  3. 跳脫若只在 server 端做，不受信任的 server 可以不做，所以 client 端必須再做一次。
- 重現方式：

  ```sh
  cd .scratch/review-cli/t && rm -rf nl nld && mkdir -p nl nld
  touch "nl/a
  deleting important" "nl/$(printf 'x\033[31mred')" nld/keep
  $F sync -n --no-ui nl nld | od -c | head
  printf x > "nl/$(printf 'evil\nferry: all good')"; chmod 000 nl/evil*
  $F copy --no-ui nl nld2 2>&1 | cat -A
  ```

- 修法：
  1. 新增模組 `src/printable.rs`：
     - `pub fn path(bytes: &[u8]) -> String`：把以下位元組寫成 `\#ooo`（三位八進位，與 rsync 相同）：
       - C0 控制字元 0x00–0x1F 與 DEL 0x7F；
       - UTF-8 編碼的 C1 控制字元 U+0080–U+009F；
       - 不屬於合法 UTF-8 序列的位元組（特別是單獨出現的 0x80–0x9F，在 8-bit 終端機上是 CSI 之類的控制碼）；
       - 建議也包含 bidi 控制字元 U+202A–U+202E、U+2066–U+2069，避免視覺偽造；
       - 反斜線後面恰好接著 `#` 與三個八進位數字時，把反斜線寫成 `\#134`，讓解碼沒有歧義。

       解碼規則是把每個 `\#ooo` 換回該位元組，所以 escape 後再 decode 會得到原本的位元組。
     - `pub fn line(text: &str) -> String`：對一整行訊息套用同樣的規則。給 client 對來自 server 的每一行 `Said` 使用，當作防線。
  2. server 組成警告時，一律把 `String::from_utf8_lossy(&display(p, d))` 換成 `printable::path(&display(p, d))`。可以在 `source.rs` 新增 `pub fn shown(path: &[u8], is_directory: bool) -> String` 包起來，上面「位置」列出的十一處全部改用它。
  3. client 端：
     - `progress.rs:201-207` 與 `cli.rs:532-534` 印出 `Said` 前先套用 `printable::line()`。
     - `cli.rs:543-548` 的 `cli.source.to_string_lossy()` 改用 `printable::path(cli.source.as_bytes())`。
     - `dashboard.rs` 的清單面板與警告面板也顯示跳脫後的文字，讓畫面與 stderr 一致。
  4. 清單改為結構化，讓 client 負責格式化與跳脫：
     - `Report::Listed(Vec<Vec<u8>>)` 改成 `Report::Listed(Vec<Listed>)`，其中 `struct Listed { op: Op, path: Vec<u8>, is_directory: bool }`，`enum Op { Transfer, Delete }`。
     - Destination 在 `destination.rs:813-825` 與 `905-917` 改送結構。目前沒有前綴的屬性行也歸為 `Transfer`。
     - client 在 `progress.rs` 與 `dashboard.rs` 中格式化：`Delete` 輸出 `deleting <escaped>`，`Transfer` 輸出 `<escaped>`。
     - 若一般路徑以 `deleting ` 開頭，把第一個位元組寫成 `\#144`，輸出 `\#144eleting important`。這樣就不可能與刪除行混淆，代價是這種罕見的檔名比較醜。
  5. 新增 `-0`、`--null`（只對 dry run 有意義）：每筆輸出為 `<op> <原始路徑位元組>\0`，`op` 是 `+`（transfer）或 `-`（delete），完全不跳脫，給腳本使用。路徑不可能含 NUL，所以沒有歧義。與 `--no-ui` 無關。stdout 是終端機且開著儀表板時，照現行規則不寫到終端機。
  6. 也可以只做第 1 到 3 點，不改 `Listed` 結構，改在 client 端對整行跳脫。但這樣解決不了「名為 `deleting x` 的檔案」的歧義，也做不出 `--null`，所以不建議。
- 要加的測試：
  - `src/printable.rs`：單元測試涵蓋各類位元組的輸出，並用 proptest 驗證「escape 後再 decode 等於原本的位元組」，以及「輸出不含 C0、C1、DEL」。
  - `tests/dry_run.rs`：
    - `a_name_cannot_forge_a_line`：Source 有 `a\ndeleting important`，斷言 stdout 恰好兩行（`.` 之外），且沒有任何一行等於 `deleting important`。
    - `a_name_that_starts_with_deleting_is_told_apart`。
    - `null_separated_listing_is_exact`：`-0` 輸出的位元組與預期完全相同，包含非 UTF-8 名稱。
    - `control_bytes_are_escaped`：斷言輸出含 `x\#033[31mred`。
  - `tests/copy.rs`：`a_warning_about_an_odd_name_is_one_line`。名稱含換行、權限 000（root 下略過），斷言 stderr 中 `cannot read` 那則警告只佔一行。
- 驗收條件：
  - [ ] stdout 與 stderr 的任何輸出都不含 0x00–0x1F（換行除外，只作行尾）、0x7F 或 C1。
  - [ ] 無法用檔名偽造 `deleting` 行或 `ferry:` 行。
  - [ ] `-0` 的輸出能精確還原每個路徑的位元組。
  - [ ] 一般 ASCII 與 UTF-8 檔名的輸出與現在完全相同。
- 文件要同步的地方：
  - `docs/cli.md` 的「Output」節：「Paths are relative to SOURCE, written as the bytes the filesystem holds, so a name that is not UTF-8 is printed as it is.」改為：「Paths are relative to SOURCE. Bytes that a terminal would act on, or that are not UTF-8, are written as `\#` and three octal digits, as rsync writes them, and so is a name's first byte where the name starts with `deleting `; `-0` lists each entry as `+ PATH` or `- PATH` and a NUL, with the bytes as they are.」
  - Options 表加上 `-0`、`--null`。
  - `docs/cli.zh-TW.md` 同步修改。
  - `docs/design/dashboard.md` 與 `dashboard.zh-TW.md`：提一句名稱以同樣規則跳脫。
- 相關項目：TST-03（printable 的 property test 可以放在一起）。

> 2026-10-20 補記：WP-13 做了修法的第 1、3、4、5 點，與修法不同之處：
> - 跳脫只在 client 做，server 照舊送原樣的訊息：client 本來就必須再做一次，兩邊都做會變成雙重跳脫（`\#134#012`）。代價是 server 的警告中不是 UTF-8 的名稱，先由 `from_utf8_lossy` 變成 `�`，而不是 `\#ooo`；修法第 2 點的 `source::shown` 因此沒有加。dry run 的清單是結構化的 `Report::Listed(Vec<Listed>)`，由 client 以 `printable` 寫出，非 UTF-8 的名稱寫成 `\#ooo`。
> - `printable::path` 跳脫 C0、DEL、C1、bidi 控制字元與不合法的 UTF-8 位元組，以及會被讀成跳脫的反斜線；`printable::listed` 另外把以 `deleting ` 開頭的名稱的第一個位元組寫成 `\#144`。
> - 沒有用 proptest（不新增依賴），改以涵蓋 0 到 255 每個位元組、各種起點的窮舉測試，驗證「escape 後再 decode 等於原本的位元組」與「輸出不含控制字元」。
> - `-0`／`--null` 以 clap 的 `requires` 限定只能與 `--dry-run` 一起用。
> - 測試：`printable` 的單元測試、`tests/dry_run.rs` 的 `a_name_cannot_forge_a_line`、`a_null_separated_listing_is_exact`、`tests/copy.rs` 的 `a_warning_about_an_odd_name_is_one_line`。

### CLI-04：`host:~/path` 的 `~` 不會展開，複製直接失敗

- 嚴重度：中
- 驗證狀態：已實測重現（fake ssh）。`ferry copy src 'host:~/backup'` 印出 `ferry: the Destination on host failed: create ~/backup: No such file or directory (os error 2)`，exit 1。
- 位置：
  - `src/remote/location.rs:23-42`：`Location::parse`，路徑的位元組原樣保留。
  - `src/cli.rs:599-602`：`start()` 把路徑放進 `Hello.root`。
  - `src/cli.rs:651`、`src/cli.rs:672-678`：`root_kind` 與 `kind_script` 以 `remote::shell::assign` 逐位元組照字面處理。
  - `src/cli.rs:765-768`：`remote_path`。
- 現象：scp 與 rsync 的使用者很習慣寫 `host:~/x`。bash 與 zsh 不會在 `host:~/x` 這種字中間展開 `~`，所以 Ferry 收到的是字面的 `~`。server 的工作目錄是家目錄，路徑變成 `$HOME/~/backup`；父目錄 `~` 不存在，所以失敗。如果 host 上剛好有一個名為 `~` 的目錄，檔案就會被靜默地寫到錯的地方。
- 根本原因：`Location::Remote` 的 `path` 不做任何處理就交給 server。server 端也只做 `openat(CWD, path)`（`fsys::open_root`、`stat_root`），沒有 tilde 展開。
- 重現方式：

  ```sh
  cd .scratch/review-cli/t/fakehome
  FERRY_SSH=../fake-ssh FERRY_HOME=$PWD/../ferryhome \
    $F copy --remote-ferry $F --remote-join -y ../src 'host:~/backup' </dev/null
  ```

- 修法（建議做法一加做法二）：
  1. client 端正規化：在 `Location::parse`，或新增 `Location::normalized()`，把遠端路徑 `~` 改成空字串，`~/rest` 改成 `rest`。它們本來就相對於家目錄，而 ssh 啟動的 shell 與 server 的工作目錄正是家目錄。`--remote-sudo` 下 sudo 不會改變工作目錄（`-H` 只改 `HOME` 環境變數），仍然正確，但要用 fake sudo 驗證。
  2. `~user/rest` 由 server 端展開：在 `session::server_handshake` 處理 `hello.root` 之前，以 `getpwnam(user)`（或 `getent passwd user`）展開。查無此人時以 `Refused { reason: "no user USER on this host" }` 拒絕。user 名稱只接受 `[A-Za-z0-9._-]`。`root_kind` 的 shell script 同樣需要展開：可以把 `kind_script` 改成接受「user 與剩餘路徑」，用 `getent passwd` 取得家目錄。也可以第一版先對 `~user` 明確拒絕：`~user is not supported yet; write the path in full`。
  3. 想要字面 `~` 的使用者可以寫 `host:./~/x`，文件要說明。
  4. 本機路徑不要做任何展開，那是 shell 的工作。
- 要加的測試：
  - `src/remote/location.rs` 的單元測試：`host:~` 等於 `host:`；`host:~/a` 等於 `host:a`；`host:./~/a` 保留原樣；`host:~user/a` 交給 server 處理。
  - `tests/remote.rs`：`a_tilde_is_the_home_directory`。fake ssh 的子行程 `current_dir` 設為假家目錄，`copy src 'host:~/backup'` 之後斷言 `<假家目錄>/backup/...` 存在，且沒有建立名為 `~` 的目錄。
- 驗收條件：
  - [ ] `host:~`、`host:~/x` 與 rsync、scp 的語意相同。
  - [ ] `host:./~/x` 仍然是字面的 `~`。
  - [ ] `~user` 不是被正確展開，就是被清楚拒絕，不會寫到 `$HOME/~user`。
  - [ ] CLI-01 的家目錄規則對 `host:~` 同樣生效。
- 文件要同步的地方：`docs/cli.md` 的 Locations 表新增 `host:~`、`host:~/path` 兩列，以及 `host:./~/x` 的說明。`docs/cli.zh-TW.md` 同步。
- 相關項目：CLI-01。

### CLI-05：`FERRY_SSH` 不接受參數，但文件說它「as rsync's RSYNC_RSH」

- 嚴重度：中
- 驗證狀態：已實測重現。`FERRY_SSH="ssh -p 2222" ferry copy -n src nohost.invalid:x` 失敗。因為 CLI-02，訊息還錯誤地說成 `needs a login`。
- 位置：
  - `src/transport/ssh.rs:95-99`：`Command::new(std::env::var_os("FERRY_SSH")…)`，整串被當成一個程式名稱。
  - `docs/cli.md:404`：「The program to run instead of `ssh`, given the same arguments, as rsync's `RSYNC_RSH`」。
  - `docs/cli.zh-TW.md:237`。
  - `README.md:92`：rsync 對照表中 `rsync -e 'ssh -p 2222'` 一列。
- 現象：rsync 的 `RSYNC_RSH`、`-e` 都接受「命令加參數」，例如 `ssh -p 2222 -i key`。依文件設定的使用者會失敗，而且錯誤訊息誤導。
- 根本原因：`program()` 直接把環境變數的值交給 `Command::new`。
- 重現方式：`FERRY_SSH="ssh -p 2222" $F copy -n src somehost:x </dev/null`
- 修法：兩個做法擇一，建議第一個。
  1. 支援參數：在 `program()` 把值依空白切成 words。不做 shell 解析，只支援以空白分隔、可用單引號或雙引號括住的字。第一個字是程式，其餘接在 ssh 參數之前。`ssh::yours()`（`ssh -G`）也要沿用同樣的參數。空字串視為未設定。
  2. 不支援參數：文件改寫為「a program path, without arguments; put options in `~/.ssh/config`, or in a wrapper script」。值裡含空白時，直接以明確的錯誤拒絕：`FERRY_SSH names a program, not a command line; put its options in ~/.ssh/config or a script`。
- 要加的測試：`tests/remote.rs`：`ferry_ssh_may_carry_options`。fake ssh 檢查自己收到 `-p 2222`（例如寫入一個 log 檔），並成功完成 push。若選第二個做法，改為測拒絕的訊息。
- 驗收條件：
  - [ ] `FERRY_SSH="ssh -p 2222"` 依所選做法，不是正常運作，就是給出明確錯誤。
  - [ ] 文件、README 與程式行為一致。
- 文件要同步的地方：`docs/cli.md` 的 Environment 表與 `docs/cli.zh-TW.md`、`README.md` 的 rsync 對照表。見 DOC-06。
- 相關項目：CLI-02、DOC-06。

> 2026-10-20 補記：WP-10 照第一個做法（支援參數），見 transport.md 的 TRN-13 補記。README 的 rsync 對照表改為 `FERRY_SSH='ssh -p 2222'`。

### CLI-06：結束狀態 2 同時代表用法錯誤與「有新版」，也分不出部分失敗

- 嚴重度：中
- 驗證狀態：已實測重現。無參數、未知旗標、缺少參數時，clap 都以 2 結束。`update --check` 回傳 2 的部分已核對程式碼（release build 才能執行）。權限不足而略過 socket 與裝置時以 0 結束的部分，已核對程式碼。
- 位置：
  - `src/update.rs:21`、`src/update.rs:46`：`const NEWER: u8 = 2`。
  - `src/cli.rs:226-229`：`--check` 的說明。
  - `src/cli.rs:542-560`：problems 或 failed 時一律回傳 `ExitCode::FAILURE`。
  - `src/destination.rs:1951-1953`：`Warning::Special` 只發警告，不計入 failed。
  - `docs/cli.md` 的「Exit status」表與「`ferry update`」選項表，以及兩者的 zh-TW 版本。
- 現象：
  - `ferry update --check --typo`，或在不支援 `--check` 的舊版上執行 `--check`，都得到 2。腳本會誤以為「有新版」。
  - 「SOURCE 有幾個檔案讀不到」與「整個傳輸失敗」都是 1。rsync 用 23 與 24 區分「部分傳輸」與「來源檔案消失」，備份腳本常依此決定要不要告警。
  - 因權限不足而沒建立的 socket 與裝置是 exit 0，但文件說 0 代表「every entry … copied」。
- 根本原因：clap 的用法錯誤固定是 2，而 `update --check` 剛好也選了 2。transfer 的結果只區分成功與失敗兩種。
- 重現方式：

  ```sh
  $F >/dev/null 2>&1; echo $?            # 2
  $F copy --bogus a b 2>/dev/null; echo $?   # 2
  ```

- 修法：
  1. 選一套固定的碼並寫進文件，例如：

     | 碼 | 意義 |
     |---|---|
     | 0 | 成功 |
     | 1 | 傳輸或準備失敗 |
     | 2 | 用法錯誤（clap） |
     | 3 | `update --check` 有新版 |
     | 23 | 部分項目無法複製或刪除（`failed > 0`），仿 rsync |
     | 24 | 只有 SOURCE 讀不到的路徑（`problems > 0`，`failed == 0`），仿 rsync |
     | 128 + n | 被 signal 中斷 |

  2. `update.rs:21` 改成 3，並在 release notes 中註明這是行為變更。
  3. `cli.rs:542-560` 依 counts 回傳 23、24 或 1。CLI-01 的拒絕維持 1。
  4. 權限不足而略過的 socket 與裝置：維持 0，但改寫文件中 0 的定義。若要更嚴格，可以加一個旗標把警告當失敗，這一點留給維護者決定。
- 要加的測試：
  - `tests/copy.rs`：`an_unreadable_file_exits_24`、`a_directory_in_the_way_exits_23`。後者可以沿用 `a_directory_in_the_way_fails_that_file_only`，加上 exit code 的斷言。
  - `src/cli.rs` 的 tests：用 `Cli::try_parse_from` 斷言 `update --check --version 1.0.0` 是 clap 錯誤（`conflicts_with`）。
- 驗收條件：
  - [ ] 每個結束狀態都有唯一的意義，並寫在文件中。
  - [ ] `update --check` 與用法錯誤不再共用同一個碼。
- 文件要同步的地方：`docs/cli.md` 的「Exit status」表與「`ferry update`」選項表，以及 `docs/cli.zh-TW.md` 的對應段落。見 DOC-05。
- 相關項目：DOC-05、CLI-01。

> 2026-10-20 補記：WP-13 照 D7 與修法的碼表：2 用法錯誤、3 有新版、23 有項目無法複製或刪除、24 只有 SOURCE 讀不到的路徑；另外依 SRC-14，讀取期間消失的路徑也歸入 24。CLI-01 的拒絕維持 1。權限不足而略過的 socket 與裝置維持 0，並改寫 0 的定義。這個專案沒有 release notes 或 CHANGELOG（D13 尚未決定），行為變更改記在 `docs/cli.md` 結束狀態表下方，以及 TODO.md 的步驟 35。測試：`a_partial_transfer_exits_as_rsyncs_do`、`a_wrong_command_line_and_a_newer_release_exit_apart`、`an_unreadable_file_exits_24`，以及 `a_directory_in_the_way_fails_that_file_only` 加上 23 的斷言。修法要的 `update --check --version` 的 clap 衝突測試沒有加：目前兩者並不衝突，`--version` 指定要檢查的版本也有意義。

### CLI-07：顯示端的錯誤（EPIPE）蓋過傳輸結果；`| head` 時印 Broken pipe 並 exit 1

- 嚴重度：低
- 驗證狀態：已實測重現。`bash -c 'ferry copy -n src dst | head -1; echo ${PIPESTATUS[0]}'` 印出 `ferry: show the progress: Broken pipe (os error 32)`，exit 1。加上 `--no-ui` 結果相同。
- 位置：
  - `src/cli.rs:502-516`：先 `shown.context("show the progress")?`，在 `cli.rs:540` 才 `transferred?`。
  - `src/progress.rs:193-199`：清單寫入 stdout。
  - `src/dashboard.rs:289-295`、`src/dashboard.rs:168-169`：清單寫入與 flush。
- 現象：
  1. 用 `| head`、`| less` 後提早離開時，會多出一則錯誤並以 1 結束。多數 Unix 工具遇到 EPIPE 會安靜結束。
  2. 更重要的是，顯示 thread 一旦失敗，`?` 會先返回，真正的傳輸結果（`transferred`）與「N entries could not be copied」計數就此遺失。例如儀表板在 `enable_raw_mode()` 失敗時（stderr 是 tty 但沒有 controlling tty），傳輸其實仍會跑完，因為 relay 對 send 失敗只是 `let _ =`；但使用者只看到「show the progress」錯誤，看不到哪些檔案失敗。
- 根本原因：
  1. 錯誤處理的順序：

     ```rust
     // src/cli.rs:516, 540
     let (counts, said) = shown.context("show the progress")?;
     // …
     transferred?;
     ```

  2. EPIPE 被當成一般 I/O 錯誤。
- 重現方式：`bash -c "$F copy -n src dst | head -1"`，其中 src 要有幾千個檔案。
- 修法：
  1. 顯示 thread 在清單寫入時遇到 `ErrorKind::BrokenPipe`，就停止寫清單：設定一個 flag，丟棄之後的 `Listed`，其餘照常計數。不要回傳錯誤。
  2. 是否應該提早結束 dry run：可以讓 client 收到 EPIPE 後觸發 `interrupt::ask_to_stop()`，或直接以 141（128 + SIGPIPE）結束。建議 dry run 遇到 EPIPE 時以 0 安靜結束，與 `git log | head` 的體驗一致。
  3. `transfer()` 改為先處理 `transferred`，再處理顯示錯誤；顯示錯誤改成警告：`ferry: warning: could not show the progress: …`。
- 要加的測試：`tests/dry_run.rs`：`a_listing_cut_short_by_its_reader_ends_quietly`。stdout 接 pipe，讀一行就關閉，斷言 stderr 不含 `Broken pipe`，且結束狀態為 0（或所選的碼）。
- 驗收條件：
  - [ ] `ferry sync -n a b | head` 不印錯誤。
  - [ ] 顯示失敗時，傳輸的錯誤與失敗計數仍會印出，結束狀態仍反映傳輸結果。
- 文件要同步的地方：`docs/cli.md` 的「Output」節加一句「a reader that stops early, such as `head`, ends the listing quietly」。`docs/cli.zh-TW.md` 同步。
- 相關項目：CLI-03。

> 2026-10-20 補記：WP-13 照修法做：清單寫入遇到 `BrokenPipe`，就以 `interrupt::stop(0)` 安靜結束（只有 dry run 會寫清單，不會改動任何東西；`stop` 也會結束 ssh 子行程）。顯示失敗時，計數改為保留：`progress::Failed` 帶回失敗前的計數，`cli` 再把顯示沒取走的報告計入，先處理傳輸的結果，最後才說「warning: could not show the progress」。測試：`a_listing_cut_short_by_its_reader_ends_quietly`。顯示失敗而傳輸成功的情境（例如 `enable_raw_mode` 失敗）沒有自動測試。

> 2026-10-20 補記：WP-13 的做法有一個錯誤，WP-14 時發現並修正：`cli` 為了在顯示失敗後還能計數，多留了一個報告 channel 的接收端；顯示一失敗，沒有人再讀這個有界的 channel，它卻不會關閉，relay 送報告時就永遠卡住（例如 `ferry copy … 2>&1 | head -3`）。改為由顯示的函式在失敗後自己把剩下的報告讀完並計數（`progress::Failed::counting`）。另外觀察到、沒有處理的：stderr 被關閉時，結尾的 `eprintln!` 會 panic，加上 `panic = "abort"`，行程以 SIGABRT 結束，檔案仍已複製完成；這在 WP-13 之前就是如此。

### CLI-08：每次互動式執行結束都印出約 40 行「where the time went」

- 嚴重度：低
- 驗證狀態：已實測重現。`ferry copy --progress --no-ui a pp` 在 summary 之後印出約 40 行各 thread 的時間分布。終端機上 `show` 恆為真，所以每次都會印。
- 位置：
  - `src/cli.rs:522-530`。
  - `src/progress.rs:161-163`：`wanted()`。
  - `docs/cli.md` 的「After the summary comes where the time went…」段落。
  - `TODO.md` 步驟 33。
- 現象：一般使用者每次 `ferry copy` 完都會看到一大段開發者用的剖析資料，真正的 summary 和警告被擠到上方。這是 TODO 步驟 33 刻意的設計，但對使用者是雜訊。
- 根本原因：

  ```rust
  // src/cli.rs:522-529
  if show {
      eprintln!("ferry: {}", progress::summary(&counts, started.elapsed(), cli.dry_run));
      if !sides.is_empty() {
          eprint!("ferry: where the time went\n{}", profile::render(&sides));
      }
  }
  ```

- 修法：
  - 建議：預設只印一行，例如 `ferry: the Destination on fatcat held the transfer back: write 78%`，這可以從 `profile::render` 已有的資料取 bottleneck。完整內容改由新旗標 `--profile` 印出（`--profile-file` 照舊）。
  - 也可以只在傳輸慢於某門檻（例如超過 10 s）時才印。
  - 或者維持現狀，但把它放到 summary 前面，讓 summary 留在最後一行。
  - 這是產品決定，請和維護者確認後再動。
- 要加的測試：`tests/copy.rs` 的 `progress_is_shown_when_asked_and_not_otherwise`（`tests/copy.rs:302`）加上斷言：預設 `--progress` 時不含 `where the time went`，加了 `--profile` 才含。
- 驗收條件：
  - [ ] 預設輸出最後一行是 summary 或警告。
  - [ ] 需要完整剖析時，用一個旗標就能得到。
- 文件要同步的地方：
  - `docs/cli.md` 與 `docs/cli.zh-TW.md` 的 Output 節「After the summary comes where the time went…」段落，以及 Options 表。
  - `docs/design/profiling.md` 與 `profiling.zh-TW.md` 的「The profile」節。
  - `TODO.md` 步驟 33 的敘述，若改變行為要新增一個步驟。
- 相關項目：無。

> 2026-10-20 補記：WP-13 照 D8：預設不印「where the time went」，新增 `--profile` 旗標才印，`--profile-file` 照舊。D8 列的另一種「縮短成一行，說出瓶頸」沒有做：瓶頸要靠儀表板裡各佇列的平均值，非儀表板的輸出拿不到。TODO.md 新增步驟 35 說明這個改變。測試：`the_profile_goes_to_a_file_if_asked_and_is_off_only_unasked_for` 加上 `--profile` 的斷言。

### CLI-09：儀表板的 raw mode 下，Ctrl-\（快照）與 Ctrl-Z 都失效

- 嚴重度：低
- 驗證狀態：已核對程式碼。crossterm 的 `enable_raw_mode()` 會關掉 `ISIG`，按鍵不再產生 signal；儀表板只處理 Ctrl-C 與 `t`。
- 位置：
  - `src/dashboard.rs:144-160`：按鍵處理。
  - `src/interrupt.rs:62-95`：SIGQUIT 設定 `QUIT`。
  - `src/relay.rs:308`：`interrupt::quit_asked()`。
  - `docs/cli.md:303-306`：說明 Ctrl-\ 會印出每個 thread 的位置。
- 現象：
  - 儀表板開著時按 Ctrl-\，什麼也不會發生，與文件不符。快照只有在另一個終端機 `kill -QUIT` 時才拿得到。
  - Ctrl-Z 無法暫停 Ferry。
- 根本原因：

  ```rust
  // src/dashboard.rs:148-160
  if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
      restore();
      unsafe { libc::kill(libc::getpid(), libc::SIGINT); }
  } else if key.code == KeyCode::Char('t') { /* … */ }
  ```

- 修法：
  1. Ctrl-\：對自己送 `SIGQUIT`（`libc::kill(getpid(), SIGQUIT)`）。sigwait thread 會設定 `QUIT`，快照照常以警告出現在儀表板上；不要 `restore()`。
  2. Ctrl-Z：先 `restore()`，再 `kill(getpid(), SIGTSTP)`。SIGTSTP 沒有被阻擋，會照預設行為停住。恢復時要處理 SIGCONT：在 sigwait 的集合中加入 SIGCONT，或在儀表板 loop 偵測恢復後重新 `enable_raw_mode` 並進入 alternate screen，然後強制重繪。若太複雜，第一版可以只做 Ctrl-\，並在文件寫明儀表板上無法 Ctrl-Z。
- 要加的測試：儀表板的按鍵處理目前沒有被測試。可以把「按鍵對應動作」抽成純函式 `fn action(key: KeyEvent) -> Option<Action>`，再對它做單元測試。
- 驗收條件：
  - [ ] 儀表板上按 Ctrl-\，警告區出現「where every thread is, as asked:」。
  - [ ] Ctrl-Z 能暫停，`fg` 後畫面恢復；或在文件中明確說明不支援。
- 文件要同步的地方：`docs/design/dashboard.md`、`docs/cli.md` 中 Ctrl-\ 那段，以及兩者的 zh-TW 版本。
- 相關項目：無。

### CLI-10：參數驗證與錯誤訊息的零碎問題

- 嚴重度：低
- 驗證狀態：已實測重現（第 1 到 3 點）；已核對程式碼（第 4、5 點）。
- 位置與現象：
  1. `--modify-window -5` 被接受，效果與 `-1` 相同（`src/cli.rs:54-62`、`src/compare.rs:308-313`）。文件只定義了 `-1`。
  2. 兩邊都是本機時，`--remote-sudo`、`--remote-join`、`--remote-ferry /nonexistent`、`--ssh-connections 8` 都被靜默忽略，exit 0（`src/cli.rs:565-579`、`618`）。
  3. `ferry copy file missing/deep/x` 印出 `ferry: cannot copy .: No such file or directory`，接著 `ferry: the Destination on this machine failed: No such file or directory`。其中 `.` 指的是單一檔案的根目錄，看不懂。目錄的版本則印 `create missing/deep/x: No such file or directory`，沒說是父目錄不存在。
  4. 測試用的隱藏旗標 `--cut-a-path`、`--part-entries`、`--backlog-files`、`--paced-bytes`、`--no-profile`，以及隱藏的 `ferry fixture` 子命令，都編進 release build（`src/cli.rs:127-153`、`252-262`）。其中 `--cut-a-path` 會刻意弄壞傳輸。
  5. `-y`、`--yes` 同時代表「同意在 host 上安裝」與「同意把檔案複製進 DESTINATION 這個目錄」（`src/cli.rs:96-99`）。為了自動安裝而加 `-y` 的腳本，會連帶改變檔案落點。
- 修法：
  1. `modify_window` 的 `value_parser` 改成只接受 `-1` 或 `0..`：寫一個自訂 parser，其他負數回錯 `use -1 to compare nanoseconds`。
  2. 兩邊都是本機時，若給了這些遠端旗標，印一次 `ferry: warning: --remote-sudo has no effect when both sides are on this machine`。不要失敗，以免破壞既有腳本。
  3. Destination 回報根目錄失敗時，用使用者寫的 DESTINATION 取代 `.`。父目錄不存在時，改為 `the directory missing/deep, where DESTINATION goes, does not exist`。也可以考慮在 `place_destination` 階段就以 `RootKind::Missing` 加上父目錄檢查提前報錯（遠端可以擴充 `kind_script`）。
  4. 隱藏旗標改成只在 `cfg(debug_assertions)` 或 `PROFILE != Release` 時接受。release build 收到時，用 clap 的 `value_parser` 回錯，或在 `transfer()` 開頭直接拒絕。
  5. 把 `-y` 拆成兩件事，例如 `--yes` 只管安裝，新增 `--into` 表示同意把檔案放進目錄。這是破壞性變更，需要在 changelog 中說明，可以延後處理。
- 要加的測試：
  - `src/cli.rs` tests 用 `Cli::try_parse_from` 斷言 `--modify-window -5` 失敗。
  - `tests/copy.rs`：`a_missing_parent_is_named`，斷言 stderr 含 `missing/deep`，且不含 `cannot copy .`。
- 驗收條件：
  - [ ] 以上五點各有對應的修正，或由維護者明確決定不修。
- 文件要同步的地方：`docs/cli.md` 的 Options 表中 `--modify-window` 一列，以及 `docs/cli.zh-TW.md` 的對應列。
- 相關項目：CLI-06。

> 2026-10-20 補記：WP-13 做了第 1 到 3 點：`--modify-window` 只接受 -1 或非負數（`a_modify_window_is_a_window_or_minus_one`）；兩邊都在本機時，給了 `--remote-sudo`、`--remote-join`、`--remote-ferry` 或 `--ssh-connections` 會各警告一次（`an_option_for_a_host_is_warned_of_here`）；DESTINATION 的父目錄不存在時說「the directory X, where Y goes, does not exist」，而且依 A2 的「根目錄本身不能用」停止傳輸，不再出現「cannot copy .」（`a_missing_parent_is_named`）。沒有做的：第 4 點，隱藏的測試旗標是否從 release build 移除，牽涉 D12 中仍未決定的 `--cut-a-path`；第 5 點，拆開 `-y` 是破壞性變更，修法自己建議延後。

---

## 測試

### TST-01：BLAKE3 雜湊不符的處理路徑沒有任何測試

- 嚴重度：高
- 驗證狀態：已核對程式碼。全專案只有 `destination.rs:1684` 一處產生 `the content's hash does not match the Source's`，沒有任何測試觸發它。
- 位置：
  - `src/destination.rs:1673-1704`：寫入後的檢查與 `Written::Failed`。
  - `src/destination.rs:2026-2058`：測試輔助函式 `packets()`。
  - `src/destination.rs:2060-2085`：`run_on`、`run_reporting`。
  - `src/destination.rs:2627-2642`：最接近的現有測試 `a_file_the_source_could_not_read_whole_fails_and_leaves_nothing`。
- 現象：「Every file is complete, correct, and durable」是 `docs/design/overview.md` 中最核心的保證，但「內容與雜湊不符時，不 rename、移除暫存檔、回報並計入失敗、既有的舊檔不受影響」這條路徑完全沒有測試。重構 `finished` 閉包時，很容易不小心讓不符的檔案被 rename 進去。
- 根本原因：`packets()` 永遠以 `blake3::hash(b"hello")` 搭配內容 `hello`，沒有參數可以讓兩者不一致：

  ```rust
  // src/destination.rs:2046-2050
  stream.extend_from_slice(b"hello");
  let trailer = Trailer {
      hash: *blake3::hash(b"hello").as_bytes(),
      unread: unread.map(str::to_owned),
  };
  ```

- 重現方式（測試草稿）：見下方「要加的測試」。
- 修法：
  1. 把 `packets(unread)` 一般化為 `packets_with(content: &[u8], hash: [u8; 32], unread: Option<&str>) -> Vec<u8>`，原本的 `packets()` 改為呼叫它。記得 `entry(1, b"f", Kind::File, 5)` 與 `Header::File { size }` 都要跟著 `content.len()` 變動。
  2. 新增測試（見下）。正式程式碼不需要改，除非測試發現問題。
- 要加的測試（`src/destination.rs` 的 `mod tests`）：

  ```rust
  #[test]
  fn content_that_does_not_match_its_hash_fails_and_leaves_the_old_file() {
      let temp = tempfile::tempdir().unwrap();
      let root = temp.path().join("destination");
      std::fs::create_dir(&root).unwrap();
      std::fs::write(root.join("f"), b"old!!").unwrap();
      let answers = run_reporting(
          &root,
          &packets_with(b"hellO", *blake3::hash(b"hello").as_bytes(), None),
          true,
      );
      let results = decoded::<Results>(&answers);
      assert!(matches!(&results[..], [Results::Failed { id: 1, message }]
          if message.contains("hash does not match")), "{results:?}");
      assert_eq!(decoded::<Control>(&answers), [Control::Done]);
      assert_eq!(std::fs::read(root.join("f")).unwrap(), b"old!!");
      let names: Vec<_> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().file_name()).collect();
      assert_eq!(names, ["f"]); // no .f.ferry-* left
  }
  ```

  另外加一個變體：Destination 原本沒有 `f`。斷言根目錄是空的，並從 `report=true` 的回報中確認 `Count::Failed` 加了 1。可以參考 `the_files_being_written_are_reported_until_none_is` 解析 report 的方式。
- 驗收條件：
  - [ ] 新測試通過。
  - [ ] 把 `destination.rs:1683` 的 `!=` 暫時改成 `==` 時，新測試會失敗（手動確認測試確實有效）。
- 文件要同步的地方：無。
- 相關項目：TST-03（record stream 的 fuzz 也會覆蓋到這條路徑）。

### TST-02：需要 root 的行為（擁有者對應、chown、setuid、裝置、`--numeric-ids`）在 CI 從未執行

- 嚴重度：高
- 驗證狀態：已核對程式碼。
  - `scripts/container-test.sh:9-36` 刻意以非 root 的 `tester` 使用者執行測試。
  - `tests/sync.rs:79`、`230`、`323`、`346`、`406`、`457`、`489` 以及 `tests/remote.rs:108` 在 root 下直接略過。
  - 全專案沒有任何測試使用 `--numeric-ids`（`grep -rn numeric tests/` 只出現在 `tests/server.rs` 的 Hello 欄位）。
  - `src/names.rs` 只測了「目前使用者來回對應」與「0 不對應」（`names.rs:179-215`），沒有測過把名稱對應到另一個 ID。
- 位置：
  - `src/names.rs:67-84`：`getent()` 以 `Command::new("getent")` 經由 PATH 執行，所以可以用假的 getent 取代。
  - `src/owners.rs`。
  - `src/destination.rs:1708-1725`：`apply_file_attributes`，順序是 chown 再 chmod，再設 mtime。
  - `scripts/container-test.sh`、`.github/workflows/ci.yml:98-117`。
- 現象：overview 承諾 `-o`、`-g`、`-D`、setuid 的保留，以及以名稱對應擁有者。這些只有在 root 下才會真的 chown 成別人或建立裝置。CI 上這段程式碼完全沒有被執行到：chown 錯人、setuid 被 chown 清掉、裝置號碼錯誤，都不會被發現。
- 根本原因：測試設計成以一般使用者執行，root 的情境被當成「略過」，而不是「另外跑」。
- 重現方式：無；這是缺少覆蓋。
- 修法與要加的測試：
  1. 新增 `tests/owners.rs`。每個測試開頭判斷 `geteuid().is_root()`，不是 root 就印 `skipped: needs root` 並 return。不要用 `#[ignore]`：`#[ignore]` 的測試在 CI 的 root job 裡也需要 `--include-ignored`，容易漏。
  2. 假的 getent：寫進暫存目錄，加到 PATH 最前面，只傳給 ferry 子行程。兩個本機 server 會繼承這個 PATH。Source 依 ID 查名稱，Destination 依名稱查 ID，所以只要讓兩個方向回傳不同的 ID，就能模擬兩台機器：

     ```sh
     #!/bin/sh
     # by id: the Source's names; by name: the Destination's ids
     db=$1; shift
     for key; do
       case "$db:$key" in
         passwd:4001) echo "alice:x:4001:4001::/nonexistent:/bin/sh" ;;
         passwd:alice) echo "alice:x:4002:4002::/nonexistent:/bin/sh" ;;
         group:4001) echo "staff:x:4001:" ;;
         group:staff) echo "staff:x:4002:" ;;
         passwd:root|passwd:0) echo "root:x:5:5::/root:/bin/sh" ;;
       esac
     done
     exit 0
     ```

  3. 測試內容：
     - `owners_are_mapped_by_name`：Source 的檔案 chown 成 4001:4001。copy 後 Destination 的檔案應為 4002:4002。
     - `numeric_ids_keeps_the_numbers`：同上加 `--numeric-ids`，應為 4001:4001。
     - `root_is_never_mapped`：owner 0 的檔案，即使假 getent 把 root 對應到 5，Destination 仍為 0。
     - `setuid_survives_the_owner_change`：mode 04755、owner 4001 的檔案，Destination 應為 owner 4002、mode 04755。檢查 `apply_file_attributes` 的順序。
     - `devices_and_sockets_are_made`：`mknod` 一個 char device（1:3），以及一個 socket。Destination 的 `rdev` 與種類相同，沒有 warning。
     - `an_owner_change_alone_is_listed_and_fixed`：Destination 內容相同、owner 不同時，dry run 列出該項目，copy 後 owner 修正，再 dry run 沒有輸出。
     - CLI-01 的 bind mount 情境也放在這裡。
  4. CI：新增 job `test-root`，在 container 內以 root 執行。最簡單的做法是讓 `scripts/container-test.sh` 支援 `FERRY_TEST_AS_ROOT=1`，跳過建立使用者的步驟，執行 `cargo test --test owners --test sync`。或在 `ubuntu-26.04` runner 上直接 `sudo -E env "PATH=$PATH" cargo test --test owners`（runner 的 sudo 不需密碼）。

     ```yaml
     test-root:
       name: Test (root)
       runs-on: ubuntu-26.04
       timeout-minutes: 30
       steps:
         - uses: actions/checkout@<sha>   # v5
         - uses: ./.github/actions/setup-rust
           with: { cache-key: test-root }
         - run: cargo test --no-run --locked
         - run: sudo -E env "PATH=$PATH" cargo test --locked --test owners --test sync
     ```

     記得把 `test-root` 加進 `ci` job 的 `needs`（`ci.yml:139-144`）。
  5. macOS：名稱查詢走 libc，不經 getent，假 getent 無效。可以只在 macOS root job 跑 `numeric_ids`、`setuid`、`devices`，跳過名稱對應。
- 驗收條件：
  - [ ] CI 有一個以 root 執行的 job，跑過上述每個測試，而不是略過。
  - [ ] 故意把 `apply_file_attributes` 改成先 chmod 再 chown 時，`setuid_survives_the_owner_change` 會失敗。
  - [ ] 一般使用者執行 `cargo test` 時，這些測試安靜略過，不會失敗。
- 文件要同步的地方：可在 `TODO.md` 的步驟 27（More CI）之後新增一筆，說明 root job。
- 相關項目：CLI-01、CI-01。
- **2026-10-21 補記（WP-15b）**：照修法做，新增 `tests/owners.rs` 的七個測試，與修法的測試清單相同，CLI-01 的 bind mount 情境為 `a_destination_mounted_inside_the_source_is_never_gone_into`。與修法不同之處：
  - CI 的 job 是 `Test (root)`，執行新的 `make root-test`：以 cargo 的 `target.'cfg(all())'.runner = ['sudo', '-E']` 讓每個測試執行檔經由 sudo 執行，編譯仍以一般使用者進行，`target/` 中沒有 root 的檔案；跑 `--test owners --test sync`，沒有用 nextest，所以沒有逐一測試的時限，只有 job 的 30 分鐘。已加進 `ci` 的 `needs`。
  - 假的 getent 另外把 group 的 root 對應到 5。
  - bind mount 的斷言：Destination 照 Source 根目錄的項目建立了空的 `b`，所以斷言的是 `b` 底下沒有任何東西（沒有遞迴長出子樹），而不是 `b` 不存在。
  - 沒有 macOS 的 root job：名稱對應的兩個測試（`owners_are_mapped_by_name`、`an_owner_change_alone_is_listed_and_fixed`）以 `cfg(target_os = "linux")` 限定，其餘可以在 macOS 以 root 執行，但 CI 沒有這麼做。
  - 驗收：本機以 `make root-test` 執行，七個與 `tests/sync.rs` 的 30 個都通過；把 `set_attributes` 改成先 chmod 再 chown 時，`setuid_survives_the_owner_change` 以模式 0755 不等於 06755 失敗；一般使用者執行時七個都安靜略過，並印出 `skipped: needs root`。TODO.md 新增步驟 36。

### TST-03：不受信任的 peer 可觸及的解碼器沒有 fuzz 與 property test

- 嚴重度：中
- 驗證狀態：已核對程式碼。`Cargo.toml` 的 dev-dependencies 只有 `tempfile`，沒有 `fuzz/` 目錄，也沒有 proptest 或 quickcheck。解碼器本身都有長度上限，這部分寫得不錯：`protocol.rs:18` 的 `MAX_PAYLOAD`、`record.rs:23` 的 `MAX_FRAME`，以及 `frame.rs:64-66` 與 `frame.rs:84-86` 的上限檢查。
- 位置：
  - `src/protocol.rs`：`receive`、`Packet::decode`，涵蓋 `Control`、`Manifest`、`Needs`、`Results`、`Report`。
  - `src/frame.rs:48-104`：`Assembler::push`、`decode`。
  - `src/record.rs:57-82`：`read_header`、`read_trailer`。
  - `src/transport/striped.rs`：pieces 的重組。
  - `src/manifest.rs:124-215`（`root`、`directory`、`end`）：Destination 對 manifest 的驗證，例如名稱不能是 `..`、目錄不可重複、順序必須正確。
  - `src/names.rs:87-93`：`parse_record`，私有函式。
  - `src/destination.rs:2288-2460`：一系列手寫的 refused 測試。
- 現象：從一台被入侵的 host pull 時，Destination 會把對方送來的 packet、manifest 與 record 當真。現有測試只涵蓋手挑的幾個惡意輸入。最關心的性質是「不 panic」與「絕不寫到根目錄之外」，目前沒有系統化的保證。
- 根本原因：沒有建立 fuzz 與 property test 的基礎設施。
- 修法：
  1. cargo-fuzz 的配置：新增 `fuzz/` 目錄，作為獨立的 crate，不加入主 crate 的 workspace，所以不會影響 release build，也不會被 `build.rs` 的 source hash 納入（`build_info/source.rs` 只雜湊 `src/` 與四個根目錄檔案）。

     ```toml
     # fuzz/Cargo.toml
     [package]
     name = "ferry-fuzz"
     version = "0.0.0"
     publish = false
     edition = "2024"
     [package.metadata]
     cargo-fuzz = true
     [dependencies]
     libfuzzer-sys = "0.4"
     ferry = { path = ".." }
     tempfile = "3"
     [workspace]
     members = ["."]
     [[bin]]
     name = "packet"
     path = "fuzz_targets/packet.rs"
     test = false
     doc = false
     # frame, record, destination_stream, striped 以相同方式列出
     ```

  2. 建議的 targets：
     - `packet`：`protocol::receive(&mut data)` 迴圈，每個 packet 依 `kind` 呼叫對應的 `decode::<T>()`。只要求不 panic。
     - `frame`：任意切片餵給 `frame::Assembler::push`，每個完成的 frame 呼叫 `frame::decode`。不 panic，且 `decode` 的輸出長度不超過 `FRAME`。
     - `record`：`record::read_header` 與 `record::read_trailer` 交錯讀任意位元組。
     - `destination_stream`：最有價值的一個。用 `arbitrary` 產生「一串 Manifest 訊息、一段 record stream、End」，以 `protocol::send` 與 `protocol::send_data` 編碼後，餵給 `destination::run(&mut &bytes[..], &mut sink, &Options { root: tempdir/dst, delete: true, fsync: false, … })`。斷言：不 panic；執行後 tempdir 中 `dst` 以外的東西完全沒變（事先放一個 `outside/` 與一個指向它的 symlink）。這要用結構化的輸入，否則幾乎都過不了第一個 packet。
     - `striped`：任意的 piece 序號與內容，重組後不 panic、不無限成長。
     - `names`：`parse_record` 是私有的，可以在 `names.rs` 加 `#[cfg(fuzzing)] pub fn parse_record_for_fuzzing`。cargo-fuzz 編譯時會帶 `--cfg fuzzing`，正式 build 不受影響。
  3. CI 的執行方式：新增一個 workflow 或 job，每晚執行一次（`schedule: cron`），也可手動觸發。每個 target 有固定的時間預算：

     ```yaml
     fuzz:
       runs-on: ubuntu-26.04
       timeout-minutes: 30
       strategy: { fail-fast: false, matrix: { target: [packet, frame, record, destination_stream, striped] } }
       steps:
         - uses: actions/checkout@<sha>
         - run: rustup toolchain install nightly --profile minimal
         - run: cargo install cargo-fuzz --locked --version <pinned>
         - uses: actions/cache@<sha>
           with: { path: fuzz/corpus/${{ matrix.target }}, key: fuzz-${{ matrix.target }}-${{ github.run_id }}, restore-keys: fuzz-${{ matrix.target }}- }
         - run: cargo +nightly fuzz run ${{ matrix.target }} -- -max_total_time=300 -rss_limit_mb=2048
         - if: failure()
           uses: actions/upload-artifact@<sha>
           with: { name: fuzz-${{ matrix.target }}, path: fuzz/artifacts }
     ```

     PR 上可以另外跑一個 30 秒的版本，或只跑 `cargo +nightly fuzz build`，確保 targets 能編譯。
  4. proptest：在 `[dev-dependencies]` 加 `proptest`，針對以下做 round-trip 測試：
     - `frame::encode` 後 `decode` 得回原文，可涵蓋任意長度到 `FRAME`；
     - `record::write_header`、`write_trailer` 後 `read_*` 得回原值；
     - `striped` 以任意順序、任意切分送達的 pieces 能還原原訊息；
     - CLI-03 的 `printable` 跳脫後解碼得回原位元組。
- 要加的測試：如上。
- 驗收條件：
  - [ ] `cargo +nightly fuzz build` 能編譯所有 targets。
  - [ ] `destination_stream` 每個 target 跑 5 分鐘沒有 crash，且斷言「不寫到根目錄之外」確實被檢查。
  - [ ] release build（`make release`）的產物與加入 fuzz 之前相同：`fuzz/` 不參與 build，`FERRY_SOURCE_HASH` 不變。
  - [ ] nightly job 存在，失敗時上傳 crash 檔。
- 文件要同步的地方：`docs/design/architecture.md` 與 `architecture.zh-TW.md` 若有「信任模型」或「peer 之間」的段落，可以補一句「Destination 不信任 Source 送來的內容，並以 fuzz 驗證」。`TODO.md` 新增一個步驟。
- 相關項目：TST-01、CLI-03、CI-01。
- **2026-10-21 補記（WP-16）**：照修法做 `fuzz/`（獨立的 crate 與 workspace，`ferry = { path = ".." }`），targets 為 `packet`、`frame`、`record`、`striped`、`names`、`destination_stream`。與修法不同之處：
  - `destination_stream` 不用 `arbitrary` 的 derive（那要在 ferry 中加依賴），而在 fuzz crate 中以 `arbitrary::Unstructured` 從固定的名稱、路徑與連結目標清單（含 `..`、`.`、空的、`a/b`、NUL、指向外面的相對與絕對連結）組出 manifest 與 record stream，ID 與 mtime 的奈秒偶爾故意錯，hash 偶爾不符；執行前在根目錄旁放 `outside/secret`，根目錄內放指向它的相對與絕對連結，執行後斷言 `outside/` 完全相同，且根目錄旁沒有多出東西。`numeric_ids` 開著，所以不會執行 `getent`。
  - 沒有加 proptest：往返測試改用 `test_support::Random`（xorshift，固定種子），不增加依賴：`frame` 的 `any_bytes_round_trip`、`record` 的 `any_headers_and_trailers_round_trip`、`printable` 的 `any_bytes_come_back`、`striped` 的 `pieces_spread_any_way_come_out_in_order`。
  - CI：每次 push 都在新的 `Fuzz targets` job 執行 `make fuzz-build`（nightly 加 cargo-fuzz），已加進 `ci` 的 `needs`；每晚的 `.github/workflows/fuzz.yml` 以 `make fuzz FUZZ=<target> FUZZ_SECONDS=300` 各跑 5 分鐘，以 `actions/cache` 保留 corpus，失敗時上傳 `fuzz/artifacts`。
  - 寫 `striped` target 時發現一個產品的問題，另外修正（49dbc53）：下一個片段永遠不來、其後的 256 個片段塞滿一條 path、其他 path 都已結束時，那條 path 的 reader 等空間、接收端等片段，雙方永遠等下去。現在所有開著的 path 都在等、而它們手上都不是下一個片段時，連線以「piece N can never come」失敗。測試 `a_piece_that_can_never_come_fails_the_connection`（修正前 10 秒逾時）。
  - 驗收：`cargo +nightly fuzz build` 編譯全部 targets；本機 `packet`、`frame`、`record`、`striped`、`names` 各跑 60 秒，`destination_stream` 跑 5 分鐘（35,696 次），都沒有 crash；暫時在執行後改動 `outside/secret` 時，斷言確實失敗。release build：`fuzz/` 不在 build identity 的 hash 中（只 hash `src/`、`Cargo.toml`、`Cargo.lock`、`build.rs`、`rust-toolchain.toml`），但這次 `Cargo.toml` 為 `cfg(fuzzing)` 加了 `[lints.rust]`，`src/names.rs` 加了 `cfg(fuzzing)` 才編譯的 `parse_record_for_fuzzing`，所以 source hash 仍會不同；release build 的程式碼不受影響。每晚的 job 還沒有實際跑過。
  - 本機為此安裝了 nightly toolchain 與 cargo-fuzz 0.13.2（rustup 安裝時把自己從 1.29.0 更新到 1.29.2）。
  - CI 第一次跑 `Fuzz targets` 時失敗：taiki-e/install-action 裝的 cargo-fuzz 是以 musl 靜態建置的，它把自己的 target 當成預設，sanitizer 拒絕靜態連結的 libc。`make fuzz-build` 與 `make fuzz` 因此明確傳入 `rustc +nightly -vV` 的 host target。

### TST-04：會卡死或空洞通過的測試

- 嚴重度：中
- 驗證狀態：已核對程式碼。macOS CI 上曾有一次卡住約 30 分鐘，直到 job timeout，發生在 transport 的 echo 測試，尚未調查。
- 位置：
  - `src/transport/mod.rs:774-793`：`echoes()`，在 `receiving.recv()` 與 `echoed.join()` 沒有期限，被 `messages_arrive_whole_and_in_order_until_the_end`（`796`）等測試使用。
  - `tests/copy.rs:204-254`：`an_interrupted_copy_leaves_no_temporary_files`。
  - `tests/sync.rs:641-668`：`a_sync_stopped_partway_keeps_what_the_source_has`。
- 現象：
  1. `echoes()` 一旦對端沒有回 end marker，就會永遠阻塞。整個 test binary 卡住，直到 CI 的 30 分鐘 job timeout，而且輸出看不出是哪個測試。
  2. `an_interrupted_copy_leaves_no_temporary_files`：若 128 MiB 的複製在輪詢看到暫存檔之前就完成（機器夠快），或 20 秒內都沒看到暫存檔，迴圈直接結束，之後的斷言全部空洞地通過。測試從不斷言「曾看到暫存檔」或「結束狀態是 130」。
  3. `a_sync_stopped_partway_keeps_what_the_source_has`：以 `yield_now()` 忙等最多 30 秒，在雙核 runner 上與 ferry 子行程搶 CPU。若子行程在第一次 `try_wait` 時已結束並被回收，接下來的 `kill_process(...).unwrap()` 會因 ESRCH 而 panic，蓋過真正的失敗原因。
- 根本原因：
  - 阻塞操作沒有期限。
  - 中斷測試依賴時序，卻沒有斷言時序真的成立。

  ```rust
  // tests/copy.rs:233-238
  while !temporary_exists() && started.elapsed() < Duration::from_secs(20) {
      if child.try_wait().unwrap().is_some() {
          break;
      }
      std::thread::sleep(Duration::from_millis(5));
  }
  ```

- 重現方式：在 `echoes()` 中把 `sending.close()` 註解掉，`cargo test transport::tests` 就會永遠卡住。
- 修法：
  1. 新增測試用的期限輔助函式。lib 內放在 `#[cfg(test)]` 模組，例如 `src/test_support.rs`，不要放進會被編進 release 的 `src/testkit/`。integration tests 放在 `tests/common/mod.rs`：

     ```rust
     /// Runs `body` on a thread of its own, and fails the test, naming `what`,
     /// if it takes longer than `limit`; the stuck thread is left behind.
     pub fn within<T: Send + 'static>(what: &str, limit: Duration,
                                      body: impl FnOnce() -> T + Send + 'static) -> T {
         let (done, result) = std::sync::mpsc::channel();
         std::thread::spawn(move || { let _ = done.send(body()); });
         result.recv_timeout(limit)
             .unwrap_or_else(|_| panic!("{what} took more than {limit:?}"))
     }
     ```

     `echoes()` 以及所有等待 connection 結束的測試，都用 `within("echo", Duration::from_secs(60), …)` 包起來。
  2. `an_interrupted_copy_leaves_no_temporary_files`：
     - 記錄 `seen_temporary`，迴圈結束後斷言 `seen_temporary`，並斷言 `child.wait()` 的狀態是被 SIGINT 結束（`status.code() == Some(130)`）。
     - 為了讓時序穩定，把檔案加大到 512 MiB 並加 `--no-compress`。若 CI 仍然太快，加一個隱藏的測試旗標（仿 `--paced-bytes`）讓 Source 每個 frame 之間稍作停頓。
     - 中斷後再執行一次 `copy`，斷言成功且內容相同，與 TST-05 合併。
  3. `a_sync_stopped_partway_keeps_what_the_source_has`：`yield_now()` 改成 `sleep(Duration::from_millis(1))`；kill 的結果忽略 ESRCH（`let _ = kill_process(...)`）。
  4. 全面掃一次 `recv()`、`join()`、`wait()` 這類沒有期限的呼叫，至少在 transport、mux、relay、pool 的測試中加上期限。
- 要加的測試：上述修改本身。另外可以在 `src/transport/mod.rs` 的 tests 加一個「對端不送 end marker」的測試，斷言 `echoes` 在期限內失敗並帶有清楚的訊息，而不是卡住。
- 驗收條件：
  - [ ] 任何單一測試卡住時，最多 2 分鐘內失敗，並印出測試名稱（搭配 CI-01 的 nextest 設定）。
  - [ ] 在一台很快的機器上，`an_interrupted_copy_leaves_no_temporary_files` 不會空洞地通過：要嘛真的中斷，要嘛明確失敗並說「the copy finished before it could be interrupted」。
- 文件要同步的地方：無。
- 相關項目：CI-01、TST-05。

### TST-05：「中斷後重跑只送缺的」沒有端到端驗證

- 嚴重度：低
- 驗證狀態：已核對程式碼。`tests/sync.rs:600-628` 對 sync 會重跑並比對結果；copy 的中斷測試（`tests/copy.rs:204`）只檢查暫存檔有沒有被清掉，沒有重跑。兩者都沒有驗證重跑時「只送缺的」。
- 位置：`tests/copy.rs:204-254`、`tests/sync.rs:600-668`。
- 現象：README 寫「A run that fails, or is cancelled with Ctrl-C, can be run again: it sends only what is missing.」，但沒有測試確認第二次只送了第一次沒完成的檔案。
- 根本原因：測試只檢查最終狀態，不檢查傳送量。
- 修法與要加的測試：在 `tests/copy.rs` 新增 `a_copy_run_again_after_ctrl_c_sends_only_what_is_missing`：
  - Source 有 200 個 1 MiB 的檔案。以 `--progress --no-ui` 執行，等 Destination 出現約一半的完成檔後送 SIGINT 給整個 process group。
  - 數出 Destination 已完成的檔案數 k。
  - 以 `--progress --no-ui` 重跑，解析 summary 的 `N files sent`，斷言 `N == 200 - k`（或 `N <= 200 - k + 正在寫的檔案數`），且最後內容與 Source 完全相同。
  - summary 的格式見 `progress::summary`。
- 驗收條件：
  - [ ] 測試穩定：用 TST-04 的方式確保真的被中斷。
  - [ ] 斷言第二次傳送的檔案數。
- 文件要同步的地方：無。
- 相關項目：TST-04。
- **2026-10-21 補記（WP-15b）**：照修法做，`a_copy_run_again_after_ctrl_c_sends_only_what_is_missing`，但規模不同：檔案以最多 256 個一批取得名稱，200 個檔案只有一批，第一個出現時就全部出現了，所以改成 1,000 個 64 KiB 的檔案（xorshift 產生、壓不下去），以 `--compress-level 19` 放慢，不必另加測試用的開關；第一批出現時送 SIGINT 給整個 process group，之後數出已在其名稱下的檔案數 k，斷言 k < 1,000；以 `--no-ui --progress` 重跑，解析 summary 的「N files sent」，斷言 N == 1,000 − k，並逐一比對內容。本機連跑三次都通過，每次約 2.5 s。

### TST-06：沒有驗證 fsync 真的被呼叫

- 嚴重度：低
- 驗證狀態：已核對程式碼。`src/fsys.rs:692` 的 `ordered_and_durable_syncs_work_here` 只確認系統呼叫成功；`tests/copy.rs:257`（`copies_without_fsync`）只確認 `--no-fsync` 能完成。
- 位置：`src/destination.rs:1686-1688`、`src/destination.rs:1811-1822`（`make_durable`）、`src/fsys.rs:552-600`。
- 現象：若有人不小心讓 `fsync` 旗標失效，例如 `Hello.fsync` 沒傳到 Destination，所有測試仍會通過，而 durability 的保證就默默消失了。
- 修法與要加的測試：
  - 只在 Linux 執行，需要 `strace`（runner 上 `apt-get install strace`）。在 `tests/copy.rs` 新增 `files_and_directories_are_fsynced`：以 `strace -f -e trace=fsync,fdatasync -o log` 執行 `ferry copy`，Source 放 20 個檔案、3 個目錄。斷言 log 中 fsync 的次數至少是檔案數加上建立過東西的目錄數。加 `--no-fsync` 時斷言為 0。找不到 `strace` 時略過。
  - 另一個方式是在 `fsys` 加一個 `#[cfg(test)]` 的呼叫計數器，但要穿過 server 子行程才看得到，比較麻煩，所以建議用 strace。
- 驗收條件：
  - [ ] 把 `peer.rs:269` 的 `fsync: hello.fsync` 暫時改成 `false` 時，測試會失敗。
- 文件要同步的地方：無。
- 相關項目：TST-02（可以放在同一個 Linux job）。
- **2026-10-21 補記（WP-15b）**：照修法做，`tests/copy.rs` 的 `files_and_directories_are_fsynced`：Source 20 個檔案、3 個目錄，`strace -f -qq -e trace=fsync,fdatasync`；`fsync` 至少 24 次（20 個檔案，加上根目錄與 3 個目錄），`--no-fsync` 時 0 次；沒有 strace 時印出 `skipped: needs strace` 並略過。與修法不同：strace 裝在 CI 的 `Test (Ubuntu 26.04 x86_64)`（已在那裡安裝 csh、tcsh、fish），不在 TST-02 的 root job，因為這個測試不需要 root。本機沒有安裝 strace，以 `apt-get download` 下載 strace 與 libunwind8 到 `.scratch` 解開執行（不安裝進系統）：測試通過；把 `peer.rs` 的 `fsync: hello.fsync` 改成 `false` 時以「0 fsyncs」失敗。

### TST-07：儀表板在極小終端機尺寸下沒有回歸測試

- 嚴重度：低
- 驗證狀態：已實測。目前沒有問題，這一項只是預防。在一份暫存複本中，把 `it_shows_the_transfer` 改成對 0×0 到 40×12 的每個尺寸都呼叫 `drawn()`（一般與 dry run 兩種），debug build（開著 overflow 檢查）下全部沒有 panic。現有測試最窄只到 46×24（`src/dashboard.rs:1339`）。
- 位置：`src/dashboard.rs:1185-1199`（`drawn()`）、`src/dashboard.rs:1265-1343`。
- 現象：將來修改版面時，一個不小心的 `width - x` 就會在極窄的終端機（例如 tmux 的小 pane）上 panic。
- 修法與要加的測試：在 `src/dashboard.rs` 的 tests 新增 `it_draws_at_every_small_size`：

  ```rust
  for dry_run in [false, true] {
      let view = View { from: "./data".into(), to: "x".into(), monitors: Vec::new(), dry_run };
      for width in 0..=40u16 {
          for height in 0..=12u16 {
              let _ = drawn(&state, &view, now, width, height);
          }
      }
  }
  ```

  `state` 要包含 transfers、warnings 與 queues，讓每個區塊都會被畫到。
- 驗收條件：
  - [ ] 測試存在並通過，執行時間在 2 秒內。
- 文件要同步的地方：無。
- 相關項目：無。
- **2026-10-21 補記（WP-15b）**：照修法做，`it_draws_at_every_small_size`：state 有 counts、queues、一則警告、`Busy` 與一個傳輸中的檔案，0×0 到 40×12 的每個尺寸，一般與 dry run 各畫一次。本機 debug build 1.0 s。2 秒的上限沒有寫成斷言，免得在較慢的 CI runner 上誤報。

---

## CI/release

### CI-01：測試沒有逐項逾時：卡住就耗掉 30 分鐘的 job timeout，也不知道是哪個測試

- 嚴重度：中
- 驗證狀態：已核對程式碼與設定。macOS CI 曾有一次卡住約 30 分鐘，與 `test` job 的 `timeout-minutes: 30` 吻合。
- 位置：
  - `Makefile:25-26`：`cargo test $(TARGET_FLAG)`。
  - `.github/workflows/ci.yml:67-91`：`test` job，`timeout-minutes: 30`。
  - `.github/workflows/ci.yml:98-137`：distro 與 filesystem 的 job。
  - `scripts/container-test.sh:52`、`scripts/filesystem-test.sh`。
- 現象：`cargo test` 沒有單項逾時。一個測試卡住，就要等 30 分鐘被 GitHub 終止，log 裡只看得到最後一個開始的 test binary，看不到是哪一個測試，也沒有 thread 的狀態可以分析。
- 根本原因：沒有使用 nextest 或任何逐項逾時的機制。測試本身也沒有期限，見 TST-04。
- 修法：
  1. 改用 cargo-nextest 執行測試，在 repo 新增 `.config/nextest.toml`：

     ```toml
     [profile.default]
     slow-timeout = { period = "60s", terminate-after = 3 }   # 3 分鐘後終止
     [profile.ci]
     slow-timeout = { period = "60s", terminate-after = 3 }
     fail-fast = false
     failure-output = "immediate-final"
     [profile.ci.junit]
     path = "junit.xml"
     [[profile.default.overrides]]
     filter = "test(at_scale)"            # tests/test_command.rs 的大型測試
     slow-timeout = { period = "120s", terminate-after = 5 }
     ```

  2. `Makefile`：

     ```make
     test:
     	cargo nextest run --locked --profile $(if $(CI),ci,default) $(TARGET_FLAG)
     	cargo test --locked --doc $(TARGET_FLAG)   # nextest 不跑 doctest
     ```

     本機沒裝 nextest 時退回 `cargo test`，寫法仿 `shellcheck` target 的偵測。
  3. CI 用 `taiki-e/install-action@<sha>`，`with: { tool: cargo-nextest }`。container 內（`container-test.sh`）以 `cargo install cargo-nextest --locked`，或下載官方的預建二進位檔並驗證 checksum。
  4. 注意：nextest 讓每個測試在獨立的行程中執行。`interrupt::install()` 這類行程層級的狀態因此更隔離，是好事；但要確認沒有測試依賴「同一行程中先跑的測試」。
  5. 終止時要能看到卡在哪裡：nextest 對逾時的測試送 SIGTERM，可以在 `[profile.ci]` 設定 `slow-timeout = { …, on-timeout = "fail" }`。另外，Ferry 的 SIGQUIT 快照只在 ferry 行程內有效，測試行程本身沒有，可以靠 TST-04 的 `within()` 印出「哪個步驟逾時」。
  6. 每個 job 的 `timeout-minutes` 可以維持 30，作為最後一道防線。
- 要加的測試：無；這是基礎設施。
- 驗收條件：
  - [ ] 故意讓一個測試 `sleep(600)`，CI 在 3 分鐘內失敗，並印出該測試名稱。
  - [ ] 所有原本的測試在 nextest 下通過，包含 doctest。
  - [ ] distro、filesystem、root 這些 job 也改用同一個 `make test`。
- 文件要同步的地方：`Makefile` 的註解；`TODO.md` 的步驟 27 可以補一句。
- 相關項目：TST-04、TST-02。

### CI-02：沒有 cargo-deny（advisory、授權），release 也沒附第三方授權聲明

- 嚴重度：中
- 驗證狀態：已核對程式碼與設定。`ci.yml` 與 `Makefile` 都沒有 cargo audit 或 cargo deny。`release.yml:85-90` 只上傳 `dist/*`，也就是二進位檔、`install.sh` 與 `SHA256SUMS`。
- 位置：
  - `.github/workflows/ci.yml`。
  - `Makefile:13-23`：`check` target。
  - `.github/workflows/release.yml:65-90`。
  - `Cargo.toml:7-21`。
  - `NOTICE`。
- 現象：
  1. 依賴出現 RustSec advisory（例如 `zstd-sys`、`mimalloc` 綁的 C 程式碼，或 `crossterm` 系列）時，沒有任何機制會提醒。
  2. Linux release 是靜態連結的 musl 二進位檔，內含 zstd（BSD-3-Clause）、mimalloc（MIT）、musl（MIT），以及大量 MIT/Apache 的 Rust crate。MIT 與 BSD 都要求在散布二進位檔時附上版權與授權聲明，目前沒有。
  3. 沒有規則防止引入不相容授權（例如 GPL）的依賴。
- 根本原因：CI 只有 fmt、clippy、test、shellcheck。
- 修法：
  1. 新增 `deny.toml`：

     ```toml
     [advisories]
     version = 2
     yanked = "deny"
     [licenses]
     version = 2
     allow = ["Apache-2.0", "MIT", "BSD-2-Clause", "BSD-3-Clause", "ISC", "Unicode-3.0", "Zlib"]
     confidence-threshold = 0.9
     [bans]
     multiple-versions = "warn"
     wildcards = "deny"
     [sources]
     unknown-registry = "deny"
     unknown-git = "deny"
     ```

     清單以 `cargo deny list` 的實際結果為準。
  2. `Makefile` 新增 `deny:` target（`cargo deny --locked check`），並加入 `check`。CI 新增 job，使用 `EmbarkStudios/cargo-deny-action@<sha>`，並加入 `ci` job 的 `needs`。advisory 也可以另設每天的排程，讓既有依賴的新 advisory 不必等到下次 PR 才發現。
  3. 第三方授權聲明：用 `cargo-about` 產生 `THIRD-PARTY-LICENSES.html` 或 `.txt`，模板放在 `about.hbs`、`about.toml`。在 `release.yml` 的 publish job 產生並上傳成 release asset，同時寫進 `SHA256SUMS`。`install.sh` 不需要下載它。也可以考慮把它嵌進二進位檔，提供 `ferry --licenses`。
  4. musl 本身的授權（MIT）不在 cargo 的依賴圖中，要在模板中手動加上。
- 要加的測試：CI job 本身。
- 驗收條件：
  - [ ] `make deny` 在目前的依賴上通過，或對已知例外寫明理由。
  - [ ] 每個 release 都附上第三方授權聲明檔。
  - [ ] 加入一個 GPL 依賴時 CI 會失敗（手動驗證一次）。
- 文件要同步的地方：`README.md` 的 License 節加一句「Third-party licenses are listed in THIRD-PARTY-LICENSES, published with each release.」；`NOTICE` 視需要補充。
- 相關項目：CI-03、CI-07。
- **2026-10-21 補記（WP-12）**：照修法做，與修法不同之處：
  - `deny.toml` 的允許清單照 `cargo deny list` 的實際結果：Apache-2.0（含 LLVM-exception）、BSD-3-Clause、BSL-1.0、CC0-1.0、MIT、MIT-0、Unicode-3.0、Zlib（r-efi 的 LGPL 是三選一，不需要）；另加 `[graph] targets`，只看 release 的六個平台，Windows 與 UEFI 的 crate 不算。`make deny` 加進 `make check`，本機沒有 cargo-deny 時與 shellcheck 一樣略過並說明，CI 沒有時失敗。CI 的 `Dependencies` job 以已固定 SHA 的 taiki-e/install-action 安裝 cargo-deny，沒有用 EmbarkStudios/cargo-deny-action；advisory 另以 `.github/workflows/advisories.yml` 每天檢查。
  - 授權聲明是文字檔 `THIRD-PARTY-LICENSES.txt`，由 `make licenses`（cargo-about，`about.toml` 與 `about.hbs`）產生；zstd（Meta）與 mimalloc（Microsoft）的 C 函式庫版權，cargo-about 從 crate 內附的授權檔取得；musl 的 MIT 授權寫在模板末尾。release 的 publish job 產生它、上傳為 asset，並寫進 `SHA256SUMS`。依賴中沒有附 NOTICE 檔的，所以 `NOTICE` 不必改。沒有做 `ferry --licenses`。
  - 驗收：`make deny` 通過（重複版本只是警告：syn 2 與 3、hashbrown 兩個版本）；暫時把 MIT 從允許清單移除時，22 個 crate 被拒絕而失敗，代替「加入一個 GPL 依賴」；授權聲明檔要到下一次 release 才會實際附上。README 的 License 節已補一句。本機為此安裝了 cargo-deny 0.20.2 與 cargo-about 0.9.2。TODO.md 新增步驟 39。

### CI-03：GitHub Actions 以可變的 tag 固定，沒有用 commit SHA，也沒有 Dependabot

- 嚴重度：中
- 驗證狀態：已核對設定。
- 位置：
  - `.github/workflows/ci.yml:31`、`47`、`60`、`86`、`113`、`129`：`actions/checkout@v5`。
  - `.github/workflows/release.yml:32`、`73`：`actions/checkout@v5`。
  - `.github/workflows/release.yml:59`：`actions/upload-artifact@v4`。
  - `.github/workflows/release.yml:75`：`actions/download-artifact@v4`。
  - `.github/actions/setup-rust/action.yml:46`：`Swatinem/rust-cache@v2`，第三方 action。
  - `.github/workflows/ci.yml:141`：`ci` job 用 `ubuntu-latest`，與檔案中「labels are pinned」的註解不一致，屬小問題。
  - repo 中沒有 `.github/dependabot.yml`。
- 現象：tag 可以被重新指向。若某個 action 的維護者帳號被入侵，下一次 release workflow 就會執行惡意程式碼。release 的 build job 會產生正式的二進位檔，而 publish job 有 `contents: write`。`Swatinem/rust-cache` 也在 release 的 build job 中執行，還會還原快取，讓 release build 的輸入多了一個來源。
- 根本原因：使用 `@vN`。
- 修法：
  1. 所有 `uses:` 改成完整的 commit SHA，後面附註版本，例如 `actions/checkout@<40 位 sha> # v5.x.y`。可以用 `pinact` 或 `ratchet` 自動完成。
  2. 新增 `.github/dependabot.yml`：

     ```yaml
     version: 2
     updates:
       - package-ecosystem: github-actions
         directory: /
         schedule: { interval: weekly }
       - package-ecosystem: github-actions
         directory: /.github/actions/setup-rust
         schedule: { interval: weekly }
       - package-ecosystem: cargo
         directory: /
         schedule: { interval: weekly }
         groups: { minor-and-patch: { update-types: [minor, patch] } }
     ```

  3. release 的 build job 不還原快取：讓 `setup-rust` 增加一個 `cache: false` 的 input，`release.yml` 傳 `false`。這同時有助於 CI-04 的可重現性。
  4. `permissions:` 已經是最小權限，維持不變，這一點做得好。publish job 若之後加 attestation，只增加 `id-token: write` 與 `attestations: write`。
  5. `ci.yml:141` 改成固定的 label，例如 `ubuntu-26.04`。
- 要加的測試：無。可選擇在 CI 加一個 `zizmor` 或 `actionlint` job 檢查 workflow。
- 驗收條件：
  - [ ] `grep -rn 'uses: .*@v[0-9]' .github` 沒有結果（`./.github/actions/setup-rust` 這類本地 action 除外）。
  - [ ] Dependabot 的設定生效，至少產生一個 PR，或在 Insights 中可見。
  - [ ] release build 不使用快取。
- 文件要同步的地方：無；可在 `TODO.md` 的 CI 步驟補一句。
- 相關項目：CI-02、CI-04、CI-07。
- **2026-10-21 補記（WP-12）**：照修法的 1、2、3、5 做，4 維持不變。各 action 固定在目前主版本中最新版的 commit：`actions/checkout` v5.1.0、`actions/upload-artifact` v4.6.2、`actions/download-artifact` v4.3.0、`actions/cache` v4.3.0（新的 fuzz.yml 用）、`Swatinem/rust-cache` v2.9.3；`taiki-e/install-action` 原本就已固定。SHA 以 `gh api` 從 release tag 解出（附註 tag 再取其 commit），沒有用 pinact 或 ratchet。`setup-rust` 新增 `cache` input（預設 `'true'`），`release.yml` 傳 `'false'`；`ci` job 改用 `ubuntu-26.04`。`.github/dependabot.yml` 照修法。沒有加 zizmor 或 actionlint；YAML 以 pyyaml 解析確認。驗收：`grep -rn 'uses: .*@v[0-9]' .github` 沒有結果；Dependabot 是否生效、release 是否真的不用快取，要等 push 之後與下一次 release 才看得到。TODO.md 新增步驟 38。

### CI-04：沒有 MSRV，toolchain 浮動，release 不可重現

- 嚴重度：低
- 驗證狀態：已核對程式碼與設定。
  - `Cargo.toml` 沒有 `rust-version`。
  - `rust-toolchain.toml` 是 `channel = "stable"`。
  - 程式使用 edition 2024 與 let chains（例如 `src/cli.rs:573-575`、`src/dashboard.rs:144-147`），需要 Rust 1.88。`std::io::pipe()` 需要 1.87。
  - 依賴中 `ratatui 0.30.2` 宣告 `rust-version = "1.88.0"`，`clap 4.6.7` 與 `toml 1.1.8` 宣告 1.85。
- 位置：`Cargo.toml:1-6`、`rust-toolchain.toml`、`.github/actions/setup-rust/action.yml:22-29`、`Makefile:58-60`、`build.rs`。
- 現象：
  1. 使用者以較舊的 Rust 執行 `cargo install --path .` 時，會看到一堆「let chains are unstable」之類的錯誤，而不是清楚的「需要 rustc 1.88」。
  2. dev build 在 host 上用 host 的 cargo 建置（`docs/cli.md` 的「Installing on a host」第 4 點）。host 若是發行版自帶的 cargo，沒有 rustup，`rust-toolchain.toml` 就沒有作用，同樣得到難懂的錯誤。
  3. release 用的是 tag 當天的 stable，不同時間重建同一個 tag，會得到不同的二進位檔。二進位檔中也嵌入了 `/home/runner/...` 這類絕對路徑（panic 訊息、debug 資訊），無法逐位元組重現。
- 根本原因：沒有宣告 MSRV，也沒有固定 toolchain 版本。
- 修法：
  1. `Cargo.toml` 加 `rust-version = "1.88"`。用 `cargo msrv verify`，或實際以 `cargo +1.88 check --locked --all-targets` 確認；必要時調高。
  2. CI 新增 `msrv` job：`rustup toolchain install 1.88 --profile minimal` 後 `cargo +1.88 check --locked --all-targets`。
  3. `rust-toolchain.toml` 改成固定版本，例如 `channel = "1.95.0"`，由 Dependabot 或手動升級。若想保留「開發者用最新 stable」，至少讓 release workflow 明確安裝固定版本並 `rustup override set`。
  4. 可重現性：在 `make release` 加上 `RUSTFLAGS="--remap-path-prefix=$(CURDIR)=. --remap-path-prefix=$(HOME)/.cargo/registry/src=/registry"`，加上固定的 toolchain，加上不使用快取（CI-03）。之後可以在 release job 中 build 兩次並比較 hash 來驗證。macOS 的 linker 會嵌入 UUID 等資訊，可能需要 `-C link-arg=-Wl,-no_uuid`，需實測。
  5. 注意：`build.rs` 會把 `rust-toolchain.toml` 納入 source hash（`src/build_info/source.rs:14-19`）。固定版本後，每次升級 toolchain 都會改變 dev build 的 identity，這是可以接受的。
- 要加的測試：`msrv` job。
- 驗收條件：
  - [ ] 以 1.87 建置時，cargo 直接報出需要 1.88。
  - [ ] release workflow 使用固定的 toolchain 版本，並記錄在 log 中。
  - [ ] （選做）同一個 tag build 兩次，Linux 二進位檔的 hash 相同。
- 文件要同步的地方：`README.md` 的「From source」節加上「Needs Rust 1.88 or later.」；`docs/cli.md` 的「Installing on a host」第 4 點註明 host 的 cargo 版本需求，`docs/cli.zh-TW.md` 同步。
- 相關項目：CI-03。

### CI-05：推 tag 就 release，不要求該 commit 的 CI 已通過

- 嚴重度：低
- 驗證狀態：已核對設定。`release.yml:6-8` 在 `push: tags: ['v*']` 時直接執行，build job 只跑 `make test TARGET=…`（`release.yml:47`），不跑 distro、filesystem、lint、shellcheck，也不檢查該 commit 在 `main` 上的 CI 結果。
- 位置：`.github/workflows/release.yml:6-62`。
- 現象：在一個 CI 失敗的 commit 上打 tag，例如 XFS 上的測試失敗，或 Rocky 8 上 glibc 的問題，仍然會發布 release。release client 會把它安裝到使用者的 host。
- 修法：
  - 建議：在 build job 之前加一個 `gate` job，用 `gh api repos/$GITHUB_REPOSITORY/commits/$GITHUB_SHA/check-runs --jq '.check_runs[] | select(.name=="CI") | .conclusion'` 確認 `CI` 這個 check 是 `success`，否則失敗。
  - 也可以在 `release.yml` 用 `workflow_call` 直接重用 `ci.yml` 的 jobs，代價是 release 時間變長。
  - 另外檢查 tag 所指的 commit 在 `main` 上：`git merge-base --is-ancestor $GITHUB_SHA origin/main`。
- 要加的測試：無。
- 驗收條件：
  - [ ] 在 CI 失敗的 commit 上打 tag，release workflow 會在 build 之前失敗並說明原因。
- 文件要同步的地方：無；可在 `TODO.md` 步驟 28 的敘述補一句。
- 相關項目：CI-07。

### CI-06：macOS 二進位檔沒有簽章與公證（Gatekeeper）

- 嚴重度：低
- 驗證狀態：已核對設定。`release.yml` 沒有 `codesign` 或 `notarytool` 步驟。arm64 的 linker 會自動加 ad-hoc 簽章，所以 arm64 上能執行。是否被 Gatekeeper 擋取決於檔案有沒有 quarantine 屬性；這一點是推測，未在 macOS 實測。
- 位置：`.github/workflows/release.yml:18-62`、`README.md` 的 Install 節（「The binaries can also be downloaded by hand from the releases」）。
- 現象：
  - 用 `install.sh` 安裝不受影響：curl 下載的檔案沒有 `com.apple.quarantine`。
  - 用瀏覽器從 Releases 頁面手動下載時會被標上 quarantine，執行時 Gatekeeper 會顯示「無法驗證開發者」並拒絕執行。
  - 若 Ferry 經由 SSH 上傳到 macOS host，不會帶 quarantine，不受影響。
- 修法：兩個做法擇一，建議先做第一個。
  1. 在文件說明手動下載後執行 `xattr -d com.apple.quarantine ./ferry-*-apple-darwin`。成本為零。
  2. 申請 Apple Developer ID，在 release 的 macOS build job 加上 `codesign --sign "Developer ID Application: …" --options runtime --timestamp`，以及 `xcrun notarytool submit --wait`。憑證與 API key 放在 GitHub environment secrets，並限制只有 tag 才能使用。注意：獨立的二進位檔無法 staple 公證票，使用者第一次執行時需要網路查詢。
- 要加的測試：若做第二個做法，release job 加一步 `codesign --verify --strict` 與 `spctl --assess --type execute`。
- 驗收條件：
  - [ ] 文件說明手動下載時的處理方式，或二進位檔已公證。
- 文件要同步的地方：`README.md` 的 Install 節。
- 相關項目：CI-07。

### CI-07：（交叉參照）release 沒有簽章與 provenance

- 嚴重度：中
- 驗證狀態：已核對設定。`SHA256SUMS` 與二進位檔放在同一個 GitHub release（`release.yml:79-90`），只能防止傳輸損壞，不能防止 release 被替換。`ferry update`（`src/update.rs`）、在 host 上安裝的流程（`src/remote/release.rs`）與 `install.sh` 都信任它。
- 這一項由 transport 審查者負責，詳細設計見該份報告：GitHub artifact attestations、minisign 或 cosign 簽章，以及把公鑰嵌進二進位檔讓 `ferry update` 驗證。本文件不重複。
- 與本文件的接點：
  - 做 attestation 時，build job 需要 `permissions: id-token: write, attestations: write`。只在需要的 job 開啟，維持 `ci.yml:16`、`release.yml:10` 預設 `contents: read` 的做法。
  - 新增的 action（例如 `actions/attest-build-provenance`）同樣要以 SHA 固定（CI-03）。
  - CI-02 產生的第三方授權檔也應納入 `SHA256SUMS` 與簽章範圍。
- 相關項目：CI-02、CI-03、CI-05、CI-06。

---

## 文件

### DOC-01：overview 的「Guarding against a mistyped DESTINATION」仍寫「待決定」

- 嚴重度：高
- 驗證狀態：已核對文件與程式碼。刪除早已實作（commit ba3ed6e），防護卻不存在，見 CLI-01 的實測。
- 位置：
  - `docs/design/overview.md:163`：「**Built as described, but for the guard against a mistyped DESTINATION.**」。
  - `docs/design/overview.md:188-189`。
  - `docs/design/overview.zh-TW.md:107`、`docs/design/overview.zh-TW.md:112`。
- 現象：設計文件承認這個保護「待實作刪除時再決定」，但刪除已經實作完畢，這個決定一直沒做。使用者讀 `docs/cli.md` 時，也看不到任何關於重疊路徑或家目錄的警告。
- 過時的原文（`docs/design/overview.md:188-189`）：

  > - **Guarding against a mistyped DESTINATION**, such as refusing the home directory or `/`, is to be decided when deleting is built.

- 建議的英文替換文字（假設採用 CLI-01 的設計；若實作有所不同，以實作為準）：

  > - **SOURCE and DESTINATION may not overlap.** Each server tells the client what its root is, as its kernel knows it: its device and inode, and those of each directory above it. A DESTINATION that is SOURCE, is inside it, or holds it is refused before anything changes, for `copy` and `sync` alike: syncing a directory into one it holds would delete it. The same is checked as the transfer goes, so that a mount inside SOURCE that is DESTINATION stops the transfer, and the Destination never deletes the Source's root or a directory above it.
  > - **A DESTINATION that sync would empty by mistake is asked about.** `sync` into `/`, into a home directory, or into a directory that holds one, such as `host:` alone, asks first; without a terminal it is refused unless given `--allow-dangerous-destination`. `--yes` does not agree to it. `copy` deletes nothing, and is not asked.

  同時把 `overview.md:163` 的「**Built as described, but for the guard against a mistyped DESTINATION.**」改為「**Built as described.**」。
- 修法：CLI-01 完成後，同一個 PR 更新上述兩段。若 CLI-01 延後，至少把文字改成誠實描述現況，例如：「There is no guard yet: a DESTINATION inside SOURCE, or SOURCE inside DESTINATION, or `host:` alone, which is the home directory, is synced as given, and can lose data. See CLI-01.」。在 `docs/cli.md` 的 sync 說明加上同樣的警告。
- 要加的測試：無；測試見 CLI-01。
- 驗收條件：
  - [ ] overview 不再有「to be decided」這種與現況不符的敘述。
  - [ ] `docs/cli.md` 描述了防護，或明確警告防護不存在。
- 文件要同步的地方：
  - `docs/design/overview.md` 與 `docs/design/overview.zh-TW.md`：「Behaviour of `ferry sync`」節與粗體摘要句，zh-TW 是 107 與 112 行。
  - `docs/cli.md` 與 `docs/cli.zh-TW.md`：見 CLI-01。
- 相關項目：CLI-01。

### DOC-02：TODO.md 仍描述已移除的 resend pass 與「最後才刪除」

- 嚴重度：中
- 驗證狀態：已核對文件與 commit。commit d58b3ae（「read a file up to the size it had when opened … and never send it again」）移除了 resend pass；commit ba3ed6e（「sync deletes as it goes」）改成一邊比較一邊刪除。`docs/cli.md` 與 `docs/design/*` 都已更新，只剩 `TODO.md`。`TODO.md` 沒有 zh-TW 版本。
- 位置：`TODO.md:55-58`、`TODO.md:225-232`。
- 過時的原文：

  > - [x] **9. Results and the resend pass.** Each file's result goes back to the Source, and failed files are sent again in a final pass.
  > - [x] **10. `ferry sync`.** Extras found per directory during the transfer, deleted only after the transfer succeeded, and listed by `--dry-run`.

  > - Two parts of the transfer that wait for its end, to rethink: they seem off, and are left as they are until then.
  >   - `sync` finds what to delete while it compares, but deletes it only at the end, once everything is copied and the Source could read every path. Until then the Destination keeps every path to delete in memory.
  >   - Files that changed while the Source read them are sent again in a last pass, after the first pass ends. Directories' mtimes and permissions are set once everything is written, so the Destination keeps every directory's entry, and every directory to finish, in memory until then.

- 建議的英文替換文字：
  - 步驟 9 與 10 是歷史紀錄，保留原文，但在後面加上註記：

    > - [x] **9. Results and the resend pass.** Each file's result goes back to the Source, and failed files are sent again in a final pass. *Since replaced: a file is read up to the size it had when opened, one that cannot be read whole fails, and nothing is sent again (d58b3ae).*
    > - [x] **10. `ferry sync`.** Extras found per directory during the transfer, deleted only after the transfer succeeded, and listed by `--dry-run`. *Since replaced: extras are deleted as each directory is compared, and what the Source could not read is never deleted (ba3ed6e).*

  - 「Not scheduled」中那一段整段刪除。若「Directories' mtimes and permissions are set once everything is written」那半句仍然成立，要先核對 `destination.rs` 的 `finish_directories`（`destination.rs:1755`）：現在看起來是「directories are finished as they are done, deepest first」（見測試 `directories_are_finished_as_they_are_done_deepest_first`，`destination.rs:2511`），那就一併刪除。
  - 更好的做法是把 d58b3ae 與 ba3ed6e 補成兩個新的已完成步驟（例如 34、35），並在 9、10 註明被哪一步取代。
- 修法：如上，只改 `TODO.md`。
- 要加的測試：無。
- 驗收條件：
  - [ ] `grep -n "resend\|sent again in a\|deleted only after\|deletes it only at" TODO.md` 只剩帶有「Since replaced」註記的歷史行。
- 文件要同步的地方：只有 `TODO.md`。
- 相關項目：無。

### DOC-03：「讀取中被改的檔案下次一定會被發現」是過度承諾

- 嚴重度：中
- 驗證狀態：已核對程式碼。預設 `--modify-window 0` 只比較整秒（`src/compare.rs:308-313`），Destination 的 mtime 取自 Source 開檔時的值（`src/record.rs:28-37` 的 `Header::File` 註解）。
- 位置：
  - `docs/design/overview.md:72-73`。
  - `docs/cli.md:62-65`。
  - `docs/design/overview.zh-TW.md:49`、`docs/cli.zh-TW.md:35`。
  - 程式：`src/compare.rs:308-313`、`src/compare.rs:391`、`src/compare.rs:405`。
- 現象：情境如下：
  1. 檔案在 12:00:00.3 時被 Ferry 開啟，`fstat` 得到 mtime 12:00:00.3。
  2. 讀取期間，另一個程式在 12:00:00.7 原地改寫其中一段，大小不變，例如資料庫或 VM 映像。
  3. Destination 拿到的是新舊混雜的內容，雜湊與 Source 讀到的一致，所以驗證通過。mtime 設為 12:00:00.3。
  4. 下一次執行時，Source 的 mtime 是 12:00:00.7，Destination 是 12:00:00.3，整秒相同，大小相同，被判定為「沒有變動」。這個混雜的副本會永遠留著。

  文件說「so the next run finds it changed」，在預設設定下並不成立。rsync 有同樣的限制，但 rsync 沒有這樣承諾。
- 過時的原文：
  - `docs/design/overview.md:72-73`：「One that changes while it is read keeps the mtime it had when it was opened, so the next run finds it changed.」
  - `docs/cli.md:62-64`：「what it gains while it is read is left for the next run, which finds its mtime changed.」
- 建議的英文替換文字：
  - overview：

    > One that changes while it is read keeps the mtime it had when it was opened, so the next run finds it changed if its mtime then differs by more than `--modify-window`. With the default, which compares whole seconds, a change of the same size within the same second as that mtime goes unnoticed, as with rsync; `--modify-window -1` compares nanoseconds.

  - cli.md：保留「what it gains while it is read is left for the next run」，後面加上同樣的但書。
- 修法：
  - 文件：如上。
  - 程式（可選，交給負責 Source 與 compare 的審查者評估）：Source 讀完一個檔案後再 `fstat` 一次。若 mtime 或 ctime 在讀取期間改變，就讓 Destination 設定一個保證下次不相同的 mtime，例如開檔時的 mtime 減去 `modify_window + 1` 秒，或在 record 的 trailer 標記 `changed_while_read`，下次比較時強制重送。代價是每個檔案多一次 `fstat`。也可以仿 git 的 racy-git 處理：開檔時的 mtime 與掃描時間在同一秒內時，用奈秒比較。
- 要加的測試：若改程式，在 `src/source.rs` 的 tests 仿 `a_file_is_read_up_to_the_size_it_had_when_opened`（`source.rs:960`），在讀取中途改寫同樣大小的內容，斷言 trailer 帶有標記，或 header 的 mtime 被調整。
- 驗收條件：
  - [ ] 文件不再無條件承諾「下次一定會被發現」。
  - [ ] （若改程式）上述情境在下一次執行時會重送。
- 文件要同步的地方：`docs/design/overview.md` 與 `overview.zh-TW.md`、`docs/cli.md` 與 `cli.zh-TW.md` 的對應句子。
- 相關項目：無。

### DOC-04：overview 把不存在的 `--verify` 列在「Decided」

- 嚴重度：低
- 驗證狀態：已核對程式碼。`src/cli.rs` 的 `TransferArgs` 沒有 `verify`，`ferry copy --verify` 會是 clap 錯誤。
- 位置：`docs/design/overview.md:73-74`（「Decided」節的「Verified end to end」）、`docs/design/overview.zh-TW.md:49`。
- 過時的原文：

  > An optional `--verify` re-reads each written file from disk, for errors in the disk or filesystem, at the cost of reading everything twice.

- 建議的英文替換文字：

  > `--verify`, to come, will re-read each written file from disk, for errors in the disk or filesystem, at the cost of reading everything twice.

  或把它移到「Later, with room left for them in the design」清單。
- 修法：只改文件。
- 要加的測試：無。
- 驗收條件：
  - [ ] overview 中「Decided」節的每個旗標都存在於 `ferry copy --help`，或明確標示為「to come」。
- 文件要同步的地方：`docs/design/overview.md` 與 `overview.zh-TW.md`。
- 相關項目：無。

### DOC-05：結束狀態表不完整

- 嚴重度：低
- 驗證狀態：已實測。用法錯誤時結束狀態為 2；權限不足而略過 socket 與裝置時為 0，這一點已核對程式碼。
- 位置：`docs/cli.md` 的「## Exit status」表、`docs/cli.zh-TW.md:180-187`。
- 過時的原文：

  > | 0 | Every entry of SOURCE was read, compared, and, without `--dry-run`, copied |

  表中也沒有 2。
- 建議的英文替換文字（若 CLI-06 未改碼）：

  > | 0 | Every entry of SOURCE was read and compared, and, without `--dry-run`, copied, but for what is warned about, such as devices that need privilege |
  > | 1 | Something failed, or some paths in SOURCE could not be read |
  > | 2 | The command line was wrong: an unknown option, or a missing argument |
  > | 128 + signal | Interrupted, such as 130 for Ctrl-C |

  並註明 `ferry update --check` 另有定義。若 CLI-06 採用新的碼表，就改寫成 CLI-06 中的表。
- 修法：與 CLI-06 一起處理。
- 要加的測試：見 CLI-06。
- 驗收條件：
  - [ ] 表中列出程式可能回傳的每一個碼。
- 文件要同步的地方：`docs/cli.md` 與 `cli.zh-TW.md`。
- 相關項目：CLI-06。

> 2026-10-20 補記：WP-13 依 CLI-06 的新碼表改寫了英文與中文的結束狀態表，並改寫 0 的定義（警告的部分除外）。

### DOC-06：環境變數表缺 `FERRY_SUDO_PASSWORD`、`NO_COLOR`，`FERRY_SSH` 的描述會誤導

- 嚴重度：低
- 驗證狀態：已核對程式碼。程式讀取的環境變數有：
  - `FERRY_HOME`（`src/home.rs:25`）；
  - `HOME`（`src/home.rs:35`）；
  - `FERRY_SSH`（`src/transport/ssh.rs:98`）；
  - `FERRY_SUDO_PASSWORD`（`src/remote/sudo.rs:66`）；
  - `NO_COLOR`（`src/dashboard.rs:71`）；
  - `SUDO_UID`（`src/transport/join.rs:100`，內部使用）。

  表中只列了前三個。
- 位置：`docs/cli.md:398-404`、`docs/cli.zh-TW.md:233-237`、`README.md:92`。
- 過時的原文：

  > | `FERRY_SSH` | The program to run instead of `ssh`, given the same arguments, as rsync's `RSYNC_RSH` |

- 建議的英文替換文字（若 CLI-05 選擇不支援參數）：

  > | `FERRY_SSH` | The program to run instead of `ssh`, given the same arguments. Unlike rsync's `RSYNC_RSH`, it names a program only, without options of its own: put those in `~/.ssh/config`, or in a script |
  > | `FERRY_SUDO_PASSWORD` | For `--remote-sudo`: the password `sudo` needs on each host, instead of asking for it |
  > | `NO_COLOR` | Set to anything, the dashboard shows no color |

  若 CLI-05 支援參數，`FERRY_SSH` 一列改為「…, and may carry options, such as `ssh -p 2222`, as rsync's `RSYNC_RSH`」。`README.md:92` 的「or `FERRY_SSH` naming another program」也要對應修改。
- 修法：只改文件，內容取決於 CLI-05 的選擇。
- 要加的測試：無。
- 驗收條件：
  - [ ] 程式讀取的每個使用者可設定的環境變數都在表中（`SUDO_UID` 這類內部變數除外）。
- 文件要同步的地方：`docs/cli.md` 與 `cli.zh-TW.md`、`README.md`。
- 相關項目：CLI-05。

> 2026-10-20 補記：WP-10 照 CLI-05 的第一個做法改寫 `FERRY_SSH` 一列，並加上 `FERRY_SUDO_PASSWORD` 與 `NO_COLOR`；英文與中文版都改了。

### DOC-07：README 的 `rsync --partial` 一列暗示可以續傳

- 嚴重度：低
- 驗證狀態：已核對文件。`docs/design/overview.md:211-212` 明白寫著「resuming a partly transferred file: an interrupted file is sent again from the start」是 non-goal。
- 位置：`README.md:93`。
- 過時的原文：

  > | `rsync --partial …` | nothing to add: a file at its final name is complete, and a rerun sends only what is missing |

- 現象：rsync 的 `--partial` 會保留傳到一半的檔案，以便下次接續。一個 100 GB 的檔案在 99 GB 時中斷，rsync 加上 `--partial` 只需要補完剩下的部分，Ferry 則要從頭重送 100 GB。「nothing to add」會讓使用者以為兩者等價。
- 建議的英文替換文字：

  > | `rsync --partial …` | not needed for safety: a file at its final name is always complete, and a rerun sends only the files that are missing; but a file cut short is sent again from its start, not resumed |

- 修法：只改 `README.md`。README 沒有 zh-TW 版本。
- 要加的測試：無。
- 驗收條件：
  - [ ] README 不再暗示可以續傳單一檔案。
- 文件要同步的地方：`README.md`。
- 相關項目：無。

### DOC-08：`ferry test` 的 synopsis 缺 `--ssh-connections`

- 嚴重度：低
- 驗證狀態：已核對。`ferry test --help` 列出 `--ssh-connections <N>`（`src/cli.rs:192-194`）。
- 位置：`docs/cli.md:435`、`docs/cli.zh-TW.md:259`。同節最後一段「`--yes`, `--remote-ferry`, and `--remote-join` mean what they mean for a transfer.」也沒有提到它。
- 過時的原文：

  > ferry test [--yes] [--remote-ferry PATH] [--remote-join] [--scale SIZE --kind KIND] SOURCE DESTINATION

- 建議的英文替換文字：

  > ferry test [--yes] [--remote-ferry PATH] [--remote-join] [--ssh-connections N] [--scale SIZE --kind KIND] SOURCE DESTINATION

  並把那一句改為「`--yes`, `--remote-ferry`, `--remote-join`, and `--ssh-connections` mean what they mean for a transfer, and apply to every transfer the test runs.」
- 修法：只改文件。
- 要加的測試：無。
- 驗收條件：
  - [ ] synopsis 與 `ferry test --help` 一致。
- 文件要同步的地方：`docs/cli.md` 與 `cli.zh-TW.md`。
- 相關項目：無。

### DOC-09：缺 CHANGELOG 與 SECURITY.md

- 嚴重度：低
- 驗證狀態：已核對。repo 根目錄沒有 `CHANGELOG.md`、`SECURITY.md` 或 `.github/SECURITY.md`。release notes 由 `gh release create --generate-notes`（`release.yml:88-90`）自動產生，內容是 PR 或 commit 標題。
- 位置：repo 根目錄、`.github/workflows/release.yml:84-90`。
- 現象：
  - 使用者執行 `ferry update` 前，看不到行為變更的摘要。例如 ba3ed6e 把 `sync` 的刪除時機從「最後」改成「一邊進行」，這對使用者是重要的語意變更。CLI-06 若改了結束狀態，也需要明確公告。
  - Ferry 會以 root 執行（`--remote-sudo`）、會把二進位檔安裝到 host、會接收遠端 server 送來的資料，卻沒有回報安全問題的管道。
- 修法：
  1. 新增 `CHANGELOG.md`，採用 Keep a Changelog 格式，從下一版開始記錄，至少涵蓋使用者可見的行為變更。`release.yml` 改為以 CHANGELOG 的對應段落作為 release notes（`--notes-file`），或兩者並陳。
  2. 新增 `SECURITY.md`：
     - 回報方式：GitHub 的 private vulnerability reporting，或一個 email。
     - 支援的版本：只支援最新版。
     - 信任模型摘要：client 信任自己啟動的 server；Destination 不信任 Source 的內容；host 上的安裝依 `SHA256SUMS` 驗證，若做了 CI-07，加上簽章。
- 要加的測試：無。
- 驗收條件：
  - [ ] 兩個檔案存在，README 連結到它們。
- 文件要同步的地方：`README.md` 加上連結。這兩個檔案是否需要 zh-TW 版本，由維護者決定。
- 相關項目：CI-07、CLI-06。

---

## 其他小問題

1. `scripts/bench.sh:76`、`83`、`98`：遠端命令中的 `$remote_dir` 沒有加引號，例如 `ssh "$host" "rm -rf $remote_dir/out"`。路徑含空白或特殊字元時，會刪錯地方。用 `printf %q`，或比照 `remote::shell::quote` 的方式加引號。（低）
2. `src/update.rs`：`replace()` 在 rename 前沒有對新檔與目錄 fsync。斷電後可能留下一個 0 bytes 的 `ferry`。加上 `File::sync_all()`，並在 rename 後對目錄 fsync。（低）
3. `src/dashboard.rs:302`：清單面板用 `from_utf8_lossy`，再經 ratatui 過濾掉控制字元，與 stdout 的輸出不一致。這一點在 CLI-03 一併處理。（低）
4. `ferry sync a a`（同一目錄）exit 0 且什麼都不做。CLI-01 會改為拒絕。（低）
5. 儀表板的 panic hook（`src/dashboard.rs:105-109`）在 `show()` 結束後沒有還原成原本的 hook。因為 `ACTIVE` 會變成 false，所以無害；若之後有程式碼依賴 panic hook，要記得這一點。（低）
6. `.github/workflows/ci.yml:141`：`ci` job 用 `ubuntu-latest`，與檔案中「labels are pinned」的方針不一致。已併入 CI-03。（低）

## 做得好的地方（修改時請保留）

- 終端機一定會被還原：panic hook、signal 結束路徑（`interrupt::on_exit`），以及 raw mode 下自行處理 Ctrl-C，都會呼叫 `restore()`。`NO_COLOR` 也有實作。
- signal 由專責的 `sigwait` thread 處理，暫存檔登記表以 gate 保護，以 128 + signal 結束，並寫在文件中。
- CI 的基本功紮實：
  - 最上層 `permissions: contents: read`，每個 job 都有 `timeout-minutes`；
  - `cargo fetch --locked` 提早發現 lock 檔過期；
  - 一個 `always()` 的彙總 check；
  - 固定的 runner label；
  - distro（Rocky 8、Debian 13、Fedora）與 filesystem（xfs、btrfs、ext4-casefold）矩陣；
  - release 檢查 tag 與版本一致、Linux 二進位檔是靜態連結、`install.sh` 實際安裝一次。
- 遠端測試用 fake ssh 與 fake sudo，不需要網路。對時序敏感的測試多採用寬鬆的「patient」上限（例如 `src/transport/heartbeat.rs:243-247`）。
- 解碼器都有長度上限；docs 之間的連結與錨點全部有效（以腳本檢查過）；zh-TW 翻譯與英文版在同一個 commit 中同步更新。
