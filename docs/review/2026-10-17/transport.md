# Transport 與遠端安裝審查：交接文件

範圍：`src/transport/`（`mod.rs`、`ssh.rs`、`start.rs`、`join.rs`、`striped.rs`、`heartbeat.rs`）、`src/remote/`（`probe.rs`、`install.rs`、`release.rs`、`sudo.rs`、`shell.rs`、`location.rs`、`mod.rs`）、`src/update.rs`、`src/home.rs`、`src/build_info/`、`build.rs`、`scripts/install.sh`、`.github/workflows/release.yml`。

所有位置都以 HEAD `3c17a2f`（工作樹乾淨）重新核對過。每一項都可單獨閱讀，不需要先看過這次審查。

用詞依 `docs/design/*.zh-TW.md`：heartbeat channel、SSH connection、control connection、path、片段（piece）、結尾標記（end marker）、開頭標記（start mark）、探測（probe）、交握（handshake）。

## 目錄

| ID | 嚴重度 | 標題 |
|---|---|---|
| TRN-01 | 高 | 先啟動的 server 在另一側準備期間收不到 heartbeat，40 秒後自行結束 |
| TRN-02 | 高 | 上傳中斷的執行檔被改名就位，之後每次都執行壞掉的 build |
| TRN-03 | 中 | release 只以同一來源的 `SHA256SUMS` 檢查，沒有簽章（端到端簽章方案） |
| TRN-04 | 中 | 多行的 `sh -c '…'` 在 csh/tcsh 登入 shell 上失敗 |
| TRN-05 | 中 | Ferry 只收到 SIGTERM/SIGHUP 時留下 ssh 行程，`ssh -N` 讓 master 永不過期 |
| TRN-06 | 中 | 使用者 ssh_config 的 `RemoteCommand` 與轉發設定會破壞或干擾 Ferry 的 session |
| TRN-07 | 低 | `/tmp` 下可預測的目錄名稱讓其他本機使用者能阻斷 Ferry；中斷時殘留 join socket |
| TRN-08 | 低 | IPv6 位址的位置 `[addr]:path` 解析錯誤 |
| TRN-09 | 低 | 經 SCM_RIGHTS 收到的 fd 沒有 close-on-exec |
| TRN-10 | 低 | `FERRY_SUDO_PASSWORD` 留在所有子行程的環境中 |
| TRN-11 | 低 | macOS 上 `messages_arrive_whole_and_in_order_until_the_end` 偶發卡住（調查計畫） |
| TRN-12 ～ TRN-24 | 低 | 較小的項目，見文末 |

## 關於 `materials/transport/bin/` 裡的假 ssh

審查時用了一個假的 ssh：[`materials/transport/bin/fakessh`](materials/transport/bin/fakessh)，並以 `FERRY_SSH` 指向它。它在本機執行所有東西：

- 遇到 `-G` 以 1 結束，讓 Ferry 不把它當成使用者自己的 ControlMaster。
- `-N`（holder）執行 `exec sleep 100000`，像真的 `ssh -N` 一樣一直存活。
- `-O check` 以 0 結束。
- `-W <socket>` 以同一目錄的 `fwd.py`（`python3 -I fwd.py <socket>`）轉接 Unix socket。
- 指令經 `cd "$HOME" && exec sh -c "$1"` 執行，模擬登入 shell。
- 指令含 `'server'` 但不含 `--join`（也就是 server 的啟動指令）時，先 `sleep "$DELAY"`。

可以成為 regression test，但不要原封不動搬進去：

- 不要沿用 `-W` 的 python3 轉接。CI 的 container 測試（Rocky 8、Debian 13、Fedora 映像）不保證有 python3。`tests/remote.rs` 現有的 `fake_ssh` 已用 `--remote-join` 避開 `-W`，新測試應照做。
- `tests/remote.rs` 現有的 `fake_ssh` 對 `-N` 與 `-O check` 都執行 `sh -c ""`，會立刻結束。因此 holder 在測試中其實不存在。TRN-05 的測試需要一個會一直存活、並把 pid 寫進檔案的 `-N`。
- `DELAY` 的做法可以直接加進 `fake_ssh`，用環境變數控制。要讓 TRN-01 的測試保持快速，需要一個只在 dev build 生效的 heartbeat 時間調整機制，見 TRN-01 的「要加的測試」。
- 下列的重現步驟都在 repo 根目錄執行。建置結果與工作目錄放在 `.scratch/review-transport/`，用完可以刪掉。

---

### TRN-01：先啟動的 server 在另一側準備期間收不到 heartbeat，40 秒後自行結束

- 嚴重度：高
- 驗證狀態：已實測重現。
  - 以 dev build 搭配上述假 ssh，讓 Destination 的 server 啟動指令延遲 45 秒：`ferry copy` 在約 45.1 秒後失敗，輸出 `ferry: the Source on this machine: the connection: dropped`，結束狀態 1。
  - 同樣條件延遲 3 秒則成功。
- 位置：
  - `src/cli.rs:431-432`：兩側的 transport 先開啟，也就是登入。
  - `src/cli.rs:447-453`：兩側的 sudo 準備。
  - `src/cli.rs:455-470`：依序對 Source、Destination 呼叫 `start`。
  - `src/cli.rs:585-628`：`start`。`remote::program` 在 603，`transport.control` 在 609，`handshake` 在 610-612，`transport.connect` 在 619。
  - `src/remote/mod.rs:37-51`：`program`。
  - `src/remote/mod.rs:64-111`：`ensure_installed`。探測在 68，詢問在 80 與 98，`release::binary` 在 85，`install::release_build` 在 87，`install::dev_build` 在 109。
  - `src/peer.rs:73-96`：server 的 accept 迴圈。
  - `src/peer.rs:103-114`：server 收齊 path 後立刻啟動 heartbeat，用 `SERVER_TIMES`。
  - `src/transport/heartbeat.rs:30-41`：`TIMES` 與 `SERVER_TIMES`。
  - `src/transport/heartbeat.rs:81-109`：`start`。
  - `src/transport/heartbeat.rs:203-230`：`check`。
  - `src/transport/mod.rs:136-223`：`Transport::connect`，不啟動 heartbeat。
  - `src/transport/mod.rs:649-727`：`Connection::split`，其中 698 是 client 唯一呼叫 `heartbeat::start` 的地方。
  - `src/relay.rs:76-78`：`split` 在 `relay()` 開頭才呼叫。
  - `src/testkit/mod.rs:422`：`ferry test` 也呼叫 `remote::program`。
- 現象：
  - 推送（`ferry copy ./data host:dir`）或 remote to remote 時，若 Destination 的 host 需要安裝 Ferry，而安裝這一步超過 40 秒，傳輸會在安裝完成、Destination 啟動之後立刻失敗。訊息是 `ferry: the Source on …: the connection: dropped` 或 `a path ended before its end`。
  - 安裝這一步包括：首次安裝的 `[y/N]` 詢問（等使用者回答）、下載 release、經 SSH 上傳約 8 MiB、dev build 在 host 上 `cargo build`。
  - dev build 第一次裝到新 host 一定超過 40 秒，所以一定失敗。再跑一次就成功（因為已經裝好），看起來像偶發。
  - 版本更新後，第一次推送到每個 host 都要重新上傳。在慢速上行鏈路，或使用者在詢問處停頓時，也會發生。
  - `ferry test` 不會發現，因為它的第一階段先把每個 host 準備好。
- 根本原因：
  1. `transfer()` 先對 Source 執行完整的 `start()`，再對 Destination 執行 `start()`：

     ```rust
     // src/cli.rs:455-470（節錄）
     let source = start(source_transport, source, Role::Source, &hello, &remote_options, source_sudo.as_ref())?;
     let mut destination = start(destination_transport, destination, Role::Destination, &hello, &remote_options, destination_sudo.as_ref())?;
     ```

  2. `start()` 對同一側依序做完探測／詢問／安裝、啟動 server、交握、開啟 connection：

     ```rust
     // src/cli.rs:603-619（節錄）
     let program = remote::program(&transport, options)?;            // 探測、詢問、下載與上傳或建置
     let mut control = transport.control(&script, &first)?;           // 啟動 server
     let welcome = control.handshake(hello(role, root))…;             // 交握
     let mut connection = transport.connect(&welcome, join)?;         // path 與 heartbeat channel 加入 server
     ```

  3. Source 的 `connect()` 一完成，Source 的 server 就收齊了 path、heartbeat channel 與 control channel。它離開 accept 迴圈，立刻開始監看 heartbeat，40 秒內任何一條 heartbeat channel 沒收到東西就結束自己：

     ```rust
     // src/peer.rs:103-114
     drop(joining);
     let heartbeats = heartbeat::start(
         channels,
         heartbeat::SERVER_TIMES,          // interval 5 s，silence 40 s（heartbeat.rs:38-41）
         Box::new(|_| {
             if !DONE.load(Ordering::SeqCst) {
                 interrupt::stop(1);
             }
         }),
     );
     ```

  4. client 這邊，heartbeat 只在 `Connection::split()` 中啟動（`transport/mod.rs:698`），而 `split()` 在 `relay()` 開頭（`relay.rs:76-78`）才被呼叫，也就是兩側都 `start()` 完之後。在那之前，heartbeat channel 是 `Connection.heartbeats` 裡尚未使用的 `Link`，client 沒有寫任何東西。
  5. 因此 Source server 的 40 秒，從 Source 的 `connect` 完成就開始算。client 的第一個 heartbeat 要等 Destination 完成探測、詢問、安裝、server 啟動、交握與 `connect` 之後才送出。超過 40 秒，Source server 呼叫 `interrupt::stop(1)` 結束。之後 relay 讀到 Source 的 path 沒有結尾標記就結束，傳輸失敗。
  6. 與 `docs/design/pipeline.md`「Phase 1: Prepare」（中文版「階段 1 準備」）的差異：
     - 文件的步驟是 3. 探測每個 host，並在缺少時安裝 → 4. 啟動兩個 server 並交握 → 5. 開啟傳輸用的 connection。也就是所有 host 都探測並安裝好之後，才啟動任何一個 server。
     - 程式卻是每一側各自跑完 3→4→5，再換另一側。所以另一側的安裝（含詢問）夾在「第一個 server 已在等 heartbeat」與「client 開始送 heartbeat」之間。
     - 文件也說「所有需要詢問你的事都在這裡詢問」。程式的確在傳輸前詢問，但詢問時第一個 server 已在執行。
     - `architecture.md` 的 Connections 一節說「雙方每 5 秒在每條 heartbeat channel 上送出一個 heartbeat」。實際上 client 在 `connect` 到 `relay` 之間沒有送。
- 重現方式：
  - 用審查時的假 ssh：

    ```sh
    # 在 repo 根目錄
    M=$PWD/docs/review/2026-10-17/materials/transport
    W=$PWD/.scratch/review-transport && mkdir -p $W
    CARGO_TARGET_DIR=$W/target cargo build --locked
    F=$W/target/debug/ferry
    rm -rf $W/dst && mkdir -p $W/src && echo hello > $W/src/a.txt
    DELAY=45 FERRY_SSH=$M/bin/fakessh $F copy --no-ui --yes --remote-ferry $F $W/src fakehost:$W/dst </dev/null
    # 約 45 秒後：ferry: the Source on this machine: the connection: dropped，結束狀態 1
    DELAY=3 FERRY_SSH=$M/bin/fakessh $F copy --no-ui --yes --remote-ferry $F $W/src fakehost:$W/dst </dev/null
    # 成功
    rm -rf $W
    ```

  - 不用 python3 時，在 `ferry copy` 加上 `--remote-join`，`-W` 就不會被用到。
  - 真實情境：以 dev build 對一個沒裝過 Ferry 的 host 執行 `ferry copy ./x newhost:y`，在 host 上建置完之後失敗。
- 修法：有兩個方向，建議兩者都做。先做 A，它是必要的，改動也侷限在 transport；再做 B。
  - **A. client 在 `connect` 之後立刻開始 heartbeat（必要）。**
    - 為什麼必要：即使照 B 重排，從第一個 server 收齊 path，到 relay 開始之間，仍然夾著另一側的 server 啟動、交握與 `connect`。在 `--remote-join` 下，這還包括 host 上 shell 啟動檔的執行時間，而那沒有上限，例如 NFS 上的家目錄卡住。只有 A 能保證 client 活著時 server 就不會誤判。
    - 做法：把 heartbeat 的啟動從 `Connection::split()` 移到 `Transport::connect()` 的最後，在所有開頭標記都等到之後。困難點是目前的 `on_failure` 要呼叫 `Failer::fail()`，而 `Receiving`（因此 `Failer`）要到 `split()` 才存在。加一個共用的失敗槽：

      ```rust
      // src/transport/mod.rs
      /// What a heartbeat failure reaches: the receiving half, once there is one.
      #[derive(Default)]
      struct Fault {
          /// A failure that came before `split`.
          message: Option<String>,
          failer: Option<striped::Failer>,
      }
      type FaultSlot = Arc<Mutex<Fault>>;
      ```

    - `connect()` 結尾：
      - 從 heartbeat 的 `Link` 建出 `heartbeat::Channel`。
      - 收集 path 與 heartbeat 子行程的 pid，以及本機 socket 的 `try_clone`。
      - 呼叫 `heartbeat::start(channels, heartbeat::TIMES, on_failure)`。
      - `on_failure`：鎖住 `FaultSlot`。有 `failer` 就呼叫 `failer.fail(message)`，否則存進 `message`。接著照舊 SIGKILL 子行程、`shutdown` socket。
      - 把 `Heartbeats` 存進 `Connection` 的新欄位 `heartbeats: Option<heartbeat::Heartbeats>`，並把 `Monitor::ages` 在這裡設好，目前在 `split` 的 715-717。
    - `split()`：建出 `Receiving` 後鎖住 `FaultSlot`，設 `failer = Some(receiving.failer())`。若 `message` 已有值，立刻 `failer.fail(message)`。把 `Heartbeats` 交給 `Paths`。`Paths::wait()` 不變。
    - `Connection` 若沒經過 `split` 就被丟棄（例如另一側 `start` 失敗），heartbeat 必須停止。為 `Connection` 實作 `Drop`：取出 `heartbeats` 並結束。
      - `Heartbeats::end(self)` 會 join writer。在 `Drop` 中也可以只設 `ending` 並通知、不 join，以免在錯誤路徑上卡住。
      - 可加一個 `Heartbeats::stop(&self)`，只設 `ending`。
    - 副作用（是好事）：client 的 30 秒逾時從 `connect` 就開始。若 Source 在 Destination 準備期間斷了，失敗會記在 `FaultSlot` 裡，到 `split` 時立刻讓接收端失敗。可另外提供 `Connection::failed(&self) -> Option<String>`，讓 `cli::transfer` 在 `start(destination)` 之後檢查，及早以正確的訊息結束。
    - Linux 與 macOS：`Instant` 在兩者都是單調時鐘、不計睡眠，沒有差異。
  - **B. 依 pipeline.md 重排準備順序（建議）。**
    - 把 `cli::start` 拆成兩步：先對兩側呼叫 `remote::program()`，也就是探測、詢問、安裝；兩側都好了，才啟動兩個 server 並 `connect`：

      ```rust
      // src/cli.rs transfer()，sudo_for 之後
      let source_program = remote::program(&source_transport, &remote_options)?;
      let destination_program = remote::program(&destination_transport, &remote_options)?;
      let source = start(source_transport, source, Role::Source, &hello, &source_program, source_sudo.as_ref())?;
      let mut destination = start(destination_transport, destination, Role::Destination, &hello, &destination_program, destination_sudo.as_ref())?;
      ```

    - `start()` 改為接受 `program: &str`，刪掉其中的 `remote::program` 呼叫。`src/testkit/mod.rs:422` 已經是先取 program，確認它的順序也一樣。
    - 兩側是同一個 host 時（同 host 的 remote to remote），`program` 可以只算一次：以 `transport.place()` 比對，如 `sudo_for` 對 sudo 的做法。目前會探測兩次。
    - 好處：
      - 所有詢問（首次安裝的詢問）都在任何 server 啟動之前，符合文件。
      - 另一側安裝失敗或使用者拒絕時，沒有任何 server 在跑。
      - 第一個 server 等 heartbeat 的空窗只剩另一側的 server 啟動加上 `connect`。
    - 也讓 TRN-24 的修法可行：sudo 的檢查可以用已知的 program。
  - 只做 B 不夠：空窗沒有上限，見上文。只做 A 則留下與文件不符的順序。建議先 A 後 B。
- 要加的測試：
  - `src/transport/mod.rs` 的 `mod tests`（unit）：
    - 用與 `messages_arrive_whole_and_in_order_until_the_end` 相同的本機 socket server，但記錄 heartbeat channel 上第一個位元組到達的時間。
    - 呼叫 `Transport::Local.connect(...)` 之後不呼叫 `split`，斷言在 `interval` 加一點餘裕之內收到 heartbeat。
    - 需要可注入的時間：加一個 `pub(crate)` 的 `connect_with_times(&self, welcome, join, times: heartbeat::Times)`，`connect` 以 `heartbeat::TIMES` 呼叫它。測試用 `interval: 20 ms`，與 `heartbeat.rs` 測試的 `PATIENT` 相同。
  - 同一處（unit）：`connect` 後、`split` 前讓 heartbeat channel 被 server 關閉；`split` 後 `recv()` 必須立即失敗，訊息含 `dropped`。這驗證 `FaultSlot` 的轉交。
  - `tests/remote.rs`（integration）：
    - 在 `fake_ssh` 加上 `FAKE_SSH_DELAY`：指令含 `'server'` 但不含 `--join` 時先 `sleep`。用 `--remote-join`。
    - 為了不讓測試跑 45 秒，加一個只在 `PROFILE == Profile::Dev` 時生效的隱藏環境變數，例如 `FERRY_TEST_HEARTBEAT=<interval_ms>,<silence_ms>`，同時縮放 `TIMES` 與 `SERVER_TIMES`（server 的 silence 維持比 client 長）。release build 忽略它，文件中的固定時間不變。
    - 測試以 silence 2 s、延遲 5 s 執行，斷言傳輸成功。
    - 若不想加這個開關，就把測試標為 `#[ignore]`，用真實時間，延遲 45 s。不建議，因為 `make test` 不會跑它。
  - 做了 B 之後（integration，`tests/remote.rs`）：`fake_ssh` 把每個指令附加到一個 log 檔；斷言兩側的探測指令（含 `machine_id=`）都出現在第一個 `'server'` 指令之前。
    - 要讓探測回報已安裝，測試需要知道 build ID。建議同時加一個隱藏的 `ferry build-id` 子命令，TRN-02 也用得到。測試在 `FERRY_HOME/hosts/<machine-id>/bin/<build>/ferry` 放一個指向 `CARGO_BIN_EXE_ferry` 的 symlink。
    - 沒有 `/etc/machine-id` 的環境跳過這個測試。
- 驗收條件：
  - [ ] Destination 的 server 啟動延遲超過 `SERVER_TIMES.silence` 時，推送仍成功（integration test，dev 開關縮短時間）。
  - [ ] `connect` 之後、`split` 之前，client 已在每條 heartbeat channel 上每個 interval 送出 heartbeat（unit test）。
  - [ ] heartbeat 在 `split` 之前就失敗時，`split` 之後的 `recv()` 回報該失敗，而不是卡住或回報別的錯誤。
  - [ ] `Connection` 在 `split` 之前被丟棄時，heartbeat thread 會結束，不留下 thread 或寫入。
  - [ ] （B）首次安裝的詢問與所有探測、安裝，都發生在任何 server 啟動之前；同 host 的 remote to remote 只探測一次。
  - [ ] release build 不受測試用的 heartbeat 開關影響。
- 文件要同步的地方：
  - `docs/design/pipeline.md`「Phase 1: Prepare」與 `pipeline.zh-TW.md`「階段 1 準備」：
    - 寫明 heartbeat 從每條 connection 開啟的那一刻就開始，不是等傳輸開始。
    - 若做了 B，寫明所有 host 都探測並安裝之後，才啟動任何 server。步驟本身不變，現在是程式對齊文件。
  - `docs/design/architecture.md`「Connections」與 `architecture.zh-TW.md`「Connection」：heartbeat channel 那一條補上「從 connection 開啟時就開始，準備期間也送」。
  - `docs/cli.md`「SSH connections」與 `cli.zh-TW.md`「SSH connection」：heartbeat 的描述不必改，順序若有改動要對照確認。
- 相關項目：TRN-02（安裝本身的可靠性）、TRN-05（結束時的行程清理）、TRN-18（`Paths::wait` 沒有逾時）、TRN-20（安裝上傳沒有逾時）、TRN-23（heartbeat 與資料共用 TCP）、TRN-24（sudo 檢查需要 program）。

---

### TRN-02：上傳中斷的執行檔被改名就位，之後每次都執行壞掉的 build

- 嚴重度：高
- 驗證狀態：已實測重現。
  - 把 `release_script` 產生的腳本存成 [`materials/transport/release_script.sh`](materials/transport/release_script.sh)（host 的目錄改成參數），只餵它 1000 個位元組：`head -c 1000 /bin/ls | sh release_script.sh HOST_DIR`。
  - 結果：結束狀態 0；`host/bin/0.14.4/ferry` 為 1000 位元組、模式 `-rwxr-xr-x`；probe 的 `[ -x … ]` 成立；執行它以 139（SIGSEGV）結束。
  - dev build 的變體（第 7 點）是推測，未實測。
- 位置：
  - `src/remote/install.rs:42-56`：`release_script`。
  - `src/remote/install.rs:59-110`：`release_build`。上傳迴圈在 85-100，`child.wait()` 在 101，`upload?` 在 102。
  - `src/remote/install.rs:116-141`：dev build 的 `script`。
  - `src/remote/install.rs:143-186`：`dev_build`。
  - `src/remote/install.rs:29-38`：`keep_recent`。
  - `src/remote/probe.rs:54-60`：`installed` 只看 `-x`。
  - `src/remote/probe.rs:61`：`any_build`。
  - `src/remote/mod.rs:64-72`：`ensure_installed` 在 `installed` 為真時直接回傳路徑。
  - `src/remote/release.rs:185-203`：`sha256`，修法會重用。
- 現象：
  - 把 Ferry 裝到 host 的上傳途中，若 client 被 Ctrl-C、當掉、網路中斷，或 SSH master 結束，host 上會留下一個截斷但可執行的 `~/.ferry/hosts/<machine-id>/bin/<version>/ferry`。
  - 之後每次對該 host 執行，探測都回報已安裝，直接執行這個殘檔。server 啟動失敗，例如 `the server did not start (signal: 11 (SIGSEGV))` 或 `exec format error`。
  - 不會自動復原；使用者必須自己找到並刪除該目錄。
  - 兩個 client 同時第一次安裝同一個 build 時，會共用 `ferry.new`，造成暫時性的損毀、`Text file busy`，或其中一個安裝失敗。
- 根本原因：
  1. 安裝腳本：

     ```sh
     # src/remote/install.rs:44-51（release_script 產生的腳本）
     set -e
     dir={dir}
     build={build}
     mkdir -p "$dir/bin/$build"
     cat > "$dir/bin/$build/ferry.new"
     chmod 755 "$dir/bin/$build/ferry.new"
     mv "$dir/bin/$build/ferry.new" "$dir/bin/$build/ferry"
     ```

  2. `cat` 讀到 EOF 就以 0 結束，不知道應該收到多少位元組，所以 `set -e` 不會觸發，`chmod`、`mv` 照常執行。以改名求原子性在這裡沒有用，因為被改名的內容本身就不完整。
  3. client 端任何中斷，到了 host 端都只是 EOF：
     - Ferry 結束時 ssh 的 stdin 被關閉，ssh 在 channel 上送出 EOF。
     - ssh mux client 被 SIGINT 結束時，master 關閉 channel，sshd 關閉子行程的 stdin。
     - 沒有 pty 的 session，sshd 不送 SIGHUP 給子行程，所以 `sh` 會跑完 `chmod` 與 `mv`。
  4. client 端：`release_build` 的上傳 closure（85-100）失敗時，`stdin` 隨 closure 結束被丟棄，等於送出 EOF。接著 `child.wait()`（101），這時遠端已把殘檔改名就位，`upload?`（102）才回報錯誤。
  5. 探測只以 `-x` 判斷：

     ```sh
     # src/remote/probe.rs:54-60
     if [ -x "$dir/bin/$build/ferry" ]; then
         echo installed=yes
         …
     else
         echo installed=no
     fi
     ```

  6. 固定的暫存名 `ferry.new`：兩個同時進行的安裝以 `O_TRUNC` 互相截斷，寫的是同一個 inode。先改名的一方讓後者的 `chmod ferry.new` 失敗；後者還在寫時，前者可能已經執行它（Linux 上 `ETXTBSY`）。
  7. dev build 的變體：
     - `tar -x` 若剛好截斷在成員邊界，可能不報錯。
     - `cargo build` 用殘缺的原始碼建置時，`build.rs` 算出的 `FERRY_SOURCE_HASH` 與 client 的不同，但產物仍被複製到 `bin/<client 的 build>/ferry`（`install.rs:133-135`）。之後每次交握都失敗：`the server is build dev-X, but the client is build dev-Y`，同樣不會自動修復。
     - 另外 `src.new`、`src`、`build` 都是共用的固定名稱，兩個同時進行的 dev 安裝會互相 `rm -rf`。
- 重現方式：

  ```sh
  # 在 repo 根目錄
  H=$PWD/.scratch/review-transport/host && rm -rf $H
  head -c 1000 /bin/ls | sh docs/review/2026-10-17/materials/transport/release_script.sh $H; echo "exit=$?"   # exit=0
  ls -l $H/bin/0.14.4/                                            # -rwxr-xr-x … 1000 … ferry
  $H/bin/0.14.4/ferry --version; echo $?                           # 139
  ```

  真實情境：release build 首次安裝到 host、正在 `uploading Ferry to host: … of …` 時按 Ctrl-C，再執行一次，server 啟動失敗。
- 修法：
  1. `release_script(host_dir: &str, build: &str, size: u64, sha256: &str) -> String`：
     - 寫入唯一的暫存名，失敗時清掉。
     - 在 host 上檢查大小，有工具時再檢查雜湊，並試著執行一次，然後才改名：

     ```sh
     set -e
     dir=…; build=…; size=…; sum=…
     mkdir -p "$dir/bin/$build"
     tmp="$dir/bin/$build/ferry.new.$$"
     trap 'rm -f "$tmp"' EXIT
     cat > "$tmp"
     got=$(wc -c < "$tmp" | tr -d ' ')
     [ "$got" = "$size" ] || { echo "ferry: the upload ended after $got of $size bytes" >&2; exit 1; }
     if command -v sha256sum >/dev/null 2>&1; then have=$(sha256sum "$tmp" | cut -d' ' -f1)
     elif command -v shasum >/dev/null 2>&1; then have=$(shasum -a 256 "$tmp" | cut -d' ' -f1)
     else have=$sum; fi
     [ "$have" = "$sum" ] || { echo "ferry: the upload does not match its checksum" >&2; exit 1; }
     chmod 755 "$tmp"
     "$tmp" build-id </dev/null >/dev/null 2>&1 || { echo "ferry: the uploaded binary does not run here" >&2; exit 1; }
     mv -f "$tmp" "$dir/bin/$build/ferry"
     ```

     - macOS 的 `wc -c` 輸出前面有空格，所以要 `tr -d ' '`。
     - Linux（GNU 或 busybox）有 `sha256sum`，macOS 有 `shasum -a 256`。兩者都沒有時只檢查大小；大小已能抓到截斷，也就是主要的問題。SSH channel 本身有完整性保護，雜湊是額外的保險。
     - 試著執行一次，能抓到家目錄以 `noexec` 掛載、架構不符等問題，在安裝時就給出清楚的訊息。執行時 stdin 必須是 `/dev/null`，以免讀到上傳的資料。
     - `size` 與 `sha256` 由 `release_build` 從本機檔案取得：大小用 `metadata().len()`；雜湊把 `release::sha256` 改為 `pub(crate)` 後重用（`release::verify` 已經算過一次，可以順便回傳）。
     - `$$` 是 host 上 `sh` 的 pid，在 host 上唯一。`mv -f` 在同一目錄內是原子的。
  2. 加一個隱藏子命令 `ferry build-id`，印出 `build_info::identity()`。用途：安裝腳本試執行時，比對輸出等於 `$build`，例如 `[ "$("$tmp" build-id </dev/null)" = "$build" ]`；TRN-01 的測試也用它。
  3. dev build 的 `script()`：
     - 改用 `src.new.$$`。
     - 以 `mkdir "$dir/install.lock"` 當作鎖，讓同一個 host 上的 dev 安裝依序進行。macOS 沒有 `flock(1)`，所以用 `mkdir` 鎖。要處理殘留的鎖：在鎖目錄裡寫入 pid 與時間，超過一段時間（例如 1 小時）且 `kill -0 <pid>` 失敗就視為殘留。
     - 建置後比對 `"$dir/build/debug/ferry" build-id` 必須等於 `$build`，否則不安裝，並說明原始碼上傳不完整。
  4. `release_build` 與 `dev_build`：`upload` 失敗時先 `child.kill()` 再 `wait()`，讓錯誤更快浮現。有了大小檢查之後，這只是改善，不是必要。
  5. 探測不需要改：修正後，只有驗證過的檔案才會出現在最終名稱。舊版本留下的殘檔在舊版本的目錄裡，新版本不會用到。
- 要加的測試（都放在 `src/remote/install.rs` 的 `mod tests`，unit，照現有的 `a_release_is_installed_and_old_builds_removed` 的寫法，在本機以 `sh -c` 執行腳本）：
  - 少給位元組：腳本以非 0 結束，`bin/<build>/ferry` 不存在，`bin/<build>/` 下沒有 `ferry.new.*` 殘留。
  - 大小相同但內容不同：雜湊檢查失敗。若本機沒有 `sha256sum` 與 `shasum`，跳過。
  - 正確內容：安裝成功。要給一個能回應 `build-id` 的假執行檔，例如一個印出 build 的 shell script。
  - 兩個腳本同時執行同一個 build：兩者都成功，或其中一個乾淨地失敗；最終檔案內容正確、可執行。
  - dev build：假 `cargo` 產生的「執行檔」印出另一個 build ID，腳本失敗，`bin/<build>/ferry` 不存在。
- 驗收條件：
  - [ ] 上傳在任何位置中斷，host 上都不會出現最終名稱的 `ferry`，也沒有暫存檔殘留。
  - [ ] 中斷後重新執行，會重新安裝並成功，不需要手動清理。
  - [ ] 同時進行的兩個安裝不會產生損毀的檔案。
  - [ ] dev build 若建出的 build ID 與目錄名稱不符，就不安裝，並給出可理解的錯誤。
  - [ ] 家目錄 `noexec` 的 host，在安裝時就得到「the uploaded binary does not run here」，而不是之後 server 啟動失敗。
- 文件要同步的地方：
  - `docs/design/architecture.md`「Installing on a remote host」與 `architecture.zh-TW.md`「在遠端 host 上安裝」：
    - 「A release build installs a binary」之後補上：上傳後在 host 上檢查大小與 SHA-256、試著執行，才改名就位。
    - 寫明 dev build 會檢查 build ID。
  - `docs/cli.md`「Installing on a host」第 3、4 點與 `cli.zh-TW.md`「在 host 上安裝」：同上。
- 相關項目：TRN-03（下載端的驗證）、TRN-01（安裝時間影響 heartbeat）、TRN-20（上傳沒有逾時）、TRN-21（`keep_recent`）。

---

### TRN-03：release 只以同一來源的 `SHA256SUMS` 檢查，沒有簽章（端到端簽章方案）

- 嚴重度：中
- 驗證狀態：已核對程式碼與工作流程。這是設計上的缺口，不是程式錯誤。
  - checksum 本身的比對是正確的：取名稱完全相符的那一行，處理 `*` 前綴，以小寫十六進位比對。
  - 檢查與上傳之間的 TOCTOU，只有同一個使用者能利用，因為快取在使用者自己的 `~/.ferry/releases/`。
- 位置：
  - `src/remote/release.rs:22-23`：`SUMS`。
  - `src/remote/release.rs:42-48`：`base_url`，來自編譯時的 `CARGO_PKG_REPOSITORY`。
  - `src/remote/release.rs:50-60`：`binary` 與 `binary_of`。
  - `src/remote/release.rs:62-87`：`latest` 與 `version_in`。
  - `src/remote/release.rs:118-131`：`fetch`。
  - `src/remote/release.rs:133-158`：`download`。
  - `src/remote/release.rs:160-183`：`verify` 與 `checksum`。
  - `src/remote/release.rs:258-303`：測試用的 `served` 等。
  - `src/update.rs:25-73`：`run`，在 64 呼叫 `release::binary_of`。
  - `src/update.rs:75-99`：`replace`。
  - `scripts/install.sh:52-68`：下載與檢查。
  - `.github/workflows/release.yml:65-91`：`publish` job，寫 `SHA256SUMS` 與 `gh release create`。
  - `Makefile:58-80`：`release`、`install-script`、`install-test`。
- 現象：使用者看不到任何異狀。問題在信任鏈：
  - client 在 host 上安裝時下載的 `ferry-<version>-<target>`、`ferry update` 下載的新版本、`curl … | sh` 的 `install.sh`，三者都只以同一個 GitHub release 裡的 `SHA256SUMS` 檢查。
  - 能寫入這個 repository release 的人（被盜的帳號、外洩的 token 或 Actions token），可以同時替換執行檔與 `SHA256SUMS`。
  - 之後每個該版本的 client 第一次接觸新的 host 時，都會上傳並執行攻擊者的執行檔；搭配 `--remote-sudo` 時以 root 執行。
  - 更嚴重的是 `ferry update`：它跟隨 `releases/latest`。攻擊者發布一個更新的版本（例如 v9.9.9）附上相符的 `SHA256SUMS`，每個執行 `ferry update` 的人都會裝上它。
- 根本原因：
  1. `fetch` 下載 `SHA256SUMS` 與執行檔，兩者來自同一個 URL 前綴，只檢查兩者一致：

     ```rust
     // src/remote/release.rs:120-131
     fn fetch(base_url: &str, dir: &Path, name: &str) -> Result<PathBuf> {
         let path = dir.join(name);
         let sums = dir.join(SUMS);
         if path.exists() && sums.exists() && verify(&sums, &path, name).is_ok() {
             return Ok(path);
         }
         fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
         download(&format!("{base_url}/{SUMS}"), &sums)?;
         download(&format!("{base_url}/{name}"), &path)?;
         verify(&sums, &path, name)?;
         Ok(path)
     }
     ```

  2. release 工作流程沒有任何簽章或來源證明：

     ```yaml
     # .github/workflows/release.yml:79-91
     - name: Write SHA256SUMS
       working-directory: dist
       run: |
         sha256sum ferry-* > SHA256SUMS
         cat SHA256SUMS
     - name: Publish the release
       env:
         GH_TOKEN: ${{ github.token }}
       run: >
         gh release create "$GITHUB_REF_NAME" dist/*
         --repo "$GITHUB_REPOSITORY"
         --title "Ferry ${GITHUB_REF_NAME#v}"
         --generate-notes
     ```

  3. `install.sh` 同樣只比對 `SHA256SUMS`（`scripts/install.sh:61-68`）。`SHA256SUMS` 能防的只有傳輸損毀，不能證明是誰發布的。
- 方案比較：

  | | minisign（公鑰嵌入程式） | cosign keyless（Sigstore） | GitHub artifact attestations | GitHub immutable releases |
  |---|---|---|---|---|
  | client（Rust）內驗證 | `minisign-verify` crate：純 Rust、無相依、數百行、適合靜態 musl | `sigstore` crate：相依多（x509、TUF、HTTP），需嵌入或線上取得會輪替的 trust root | 與 cosign 相同（Sigstore bundle），或要求 `gh` CLI | 不需要程式 |
  | `install.sh` 內驗證 | 需要 `minisign` 執行檔（不常見），只能「有就驗」 | 需要 `cosign`（不常見） | 需要 `gh` 與網路 | 不需要 |
  | 離線驗證 | 可以 | 需要嵌入的 trust root | 需要 bundle 與 trust root | 不適用 |
  | 金鑰管理 | 要：一把長期私鑰 | 不用 | 不用 | 不用 |
  | 擋得住替換既有 release 的資產 | 擋得住 | 擋得住（簽章綁定 workflow 身分） | 擋得住 | 擋得住 |
  | 擋得住有 repo 寫入權的人發布新版本 | 擋得住，只要私鑰不在一般寫入權可及之處（受保護的 Environment，或離線簽） | 擋不住：能推 tag、跑 workflow 的人就能拿到有效簽章 | 擋不住，理由同左 | 擋不住 |
  | 其他價值 | 簡單、可預測 | 沒有私鑰可外洩 | SLSA 來源證明：哪個 commit、哪個 workflow 建的，供人工稽核 | 發布後資產與 tag 不能再改 |

- 建議：
  - **主要機制：minisign，公鑰嵌入程式。** client 與 `ferry update` 強制驗證；`install.sh` 有 `minisign` 就驗。這是唯一能在 client 內以小成本、離線驗證，又能擋住「有寫入權的人發布新版本」的方案。這一點對 `ferry update` 跟隨 latest 尤其重要。
  - **加上 GitHub immutable releases**（repository 設定），擋住事後替換資產，不需要程式。
  - **（選擇性）加上 `actions/attest-build-provenance`**，供人工稽核，不當作 client 的驗證機制。
  - 不建議 cosign keyless 當主要機制：在 client 內驗證的成本太高，而且擋不住有寫入權的人。
- 修法（以 minisign 為例，端到端）：
  1. **金鑰與金鑰管理**
     - 在一台離線的機器上產生：`minisign -G -p release.pub -s release.key`。CI 用的金鑰見第 2 點關於密碼的說明。
     - 私鑰備份兩份離線保存。
     - 公鑰放在 `src/remote/release.pub`，必須在 `src/` 底下：dev build 上傳到 host 的原始碼只含 `src/` 與 `ROOT_FILES`（`src/build_info/source.rs:14-22`），`include_str!` 指到 `src/` 外會讓 host 上的 dev build 失敗。
     - 程式以 `include_str!("release.pub")` 嵌入。`install.sh` 的公鑰由 `make install-script` 從同一個檔案以 `sed` 寫入，例如新的 `@PUBLIC_KEY@` 佔位符，維持單一來源。
     - CI 簽章：
       - 選項 a（建議的預設）：私鑰放在 GitHub Environment `release` 的 secret（`MINISIGN_KEY`）。該 Environment 設定 required reviewers（維護者本人），並限制只有 `v*` tag 能部署。只有簽章 job 引用這個 Environment，所以每次 release 都要維護者按下核准，私鑰才會交給 job。
       - 選項 b（最強）：CI 只建置；維護者下載 artifacts，在本機核對後簽章並上傳 `SHA256SUMS.minisig`。私鑰從不進 CI，代價是手動步驟。
     - 輪替：程式嵌入公鑰清單 `PUBLIC_KEYS: &[&str]`，含目前的與預先產生的下一把。驗證時逐一嘗試。要換金鑰時，先發布一版「嵌入新舊兩把、以舊鑰簽」的版本，之後的版本改以新鑰簽。
     - 私鑰外洩：立刻發布一版只信任新鑰、以新鑰簽的版本。舊的 client 仍信任外洩的金鑰，這無法事後補救，要在文件說明：請以 `install.sh` 重新安裝，並手動核對。
  2. **`release.yml`**：
     - 在 `build` 與 `publish` 之間加一個 `sign` job：
       - `needs: build`，`environment: release`。
       - 下載 artifacts，寫 `SHA256SUMS`。
       - 以 `minisign -S -s <key> -m SHA256SUMS -t "ferry ${GITHUB_REF_NAME#v} SHA256SUMS"` 簽章，產生 `SHA256SUMS.minisig`。trusted comment 也在簽章範圍內，用來綁定版本。
       - 立刻以 repo 內的公鑰 `minisign -V -p src/remote/release.pub -m SHA256SUMS` 驗一次。
       - 上傳兩個檔案為 artifact。
     - `publish` 改為 `needs: sign`，使用 `sign` 產生的 `SHA256SUMS*`，不要再自己寫一份。
     - minisign 通常從終端機讀私鑰的密碼，CI 中不易提供。CI 用的金鑰可以用 `minisign -G -W`（不加密）產生，保護來自 Environment secret 與審核。實作時確認所用 minisign 版本的行為；也可改用 Rust 的 `rsign2`。
     - minisign 在 Ubuntu 以 `sudo apt-get install --yes --no-install-recommends minisign` 安裝。
     - 簽章前後都不把私鑰寫進 log：`umask 077` 寫到 `$RUNNER_TEMP`，用完刪除。
     - 若啟用 immutable releases：依 GitHub 文件建議，先以 `gh release create --draft` 建立並附上所有資產，再 `gh release edit --draft=false` 發布。
     - 選擇性：`build` job 加 `actions/attest-build-provenance`，需要 `permissions: id-token: write, attestations: write`。
     - actions 以 SHA 釘選，以及 release job 不要用 `Swatinem/rust-cache`，屬於 CLI/CI 審查的範圍，請參照那份報告中 pin actions 與 release 快取的項目，一併處理。
  3. **`src/remote/release.rs`**：
     - 新增：

       ```rust
       const SIGNATURE: &str = "SHA256SUMS.minisig";
       /// Ferry's release keys, newest first; see docs.
       const PUBLIC_KEYS: &[&str] = &[include_str!("release.pub")];

       /// Checks that `sums` is signed by one of `keys` for `version`.
       fn verify_signature(sums: &[u8], signature: &str, version: &str, keys: &[&str]) -> Result<()> {
           let signature = minisign_verify::Signature::decode(signature).context("read the signature")?;
           let signed = keys.iter().any(|key| {
               key_of(key).is_ok_and(|key| key.verify(sums, &signature, false).is_ok())
           });
           if !signed { bail!("{SUMS} is not signed by Ferry's release key"); }
           let expected = format!("ferry {version} {SUMS}");
           if signature.trusted_comment() != expected { bail!("{SUMS} is signed for another release"); }
           Ok(())
       }
       ```

       `key_of` 解析 `release.pub` 的第二行（base64）。實作前請確認 `minisign-verify` 的 API 名稱（`PublicKey::from_base64`、`Signature::decode`、`trusted_comment`）。
     - `fetch(base_url, dir, name)` 改為 `fetch(base_url, dir, name, version, keys)`：
       - 快取命中的條件改為：`SHA256SUMS`、`SHA256SUMS.minisig` 與執行檔都在，簽章有效，checksum 相符。
       - 否則依序下載三者，驗簽章，再驗 checksum。
       - 只讀一次 `SHA256SUMS` 的位元組，以同一份位元組驗簽章並取 checksum，避免兩次讀檔之間被換掉。
       - `verify` 改為接受 `&str`（sums 的內容）。
     - `binary_of(version, target)` 傳入 `version` 與 `PUBLIC_KEYS`。測試傳入測試用的公鑰。
     - 第一個簽章版本之前的 release 沒有簽章，例如 `ferry update --version 0.14.4`。預設拒絕（fail closed），訊息說明那是簽章之前的版本，請用該版本的 `install.sh` 安裝。若需要，可加一個隱藏的 `FERRY_ALLOW_UNSIGNED=1`。
     - client 只會為 host 下載自己的版本；從第一個簽章版本起，那一定有簽章。
     - 0.14.x 以前的 client 不認識簽章，照常只用 `SHA256SUMS`；`SHA256SUMS` 的名稱與格式不變，所以它們不受影響。
     - `Cargo.toml` 加 `minisign-verify`，`Cargo.lock` 一併更新；`dev-dependencies` 加完整的 `minisign` crate，供測試產生金鑰與簽章。
  4. **`src/update.rs`**：`binary_of` 已經走 `fetch`，自動得到簽章驗證，只需調整錯誤訊息。
     - `--version` 指向未簽章的舊版本時，給出第 3 點的訊息。
     - `latest()` 不需要改：跟隨轉向到假的 tag 也做不出有效的簽章，而 trusted comment 綁定了版本。
  5. **`scripts/install.sh`**：

     ```sh
     public_key="@PUBLIC_KEY@"
     curl -fsSL --proto-redir =https -o "$temp/SHA256SUMS.minisig" "$base/SHA256SUMS.minisig"
     if command -v minisign >/dev/null 2>&1; then
         comment=$(minisign -V -q -Q -P "${FERRY_PUBLIC_KEY:-$public_key}" -m "$temp/SHA256SUMS" -x "$temp/SHA256SUMS.minisig") \
             || fail "SHA256SUMS is not signed by Ferry's release key"
         [ "$comment" = "ferry $version SHA256SUMS" ] || fail "SHA256SUMS is signed for another release"
     elif [ -n "${FERRY_REQUIRE_SIGNATURE:-}" ]; then
         fail "minisign is needed to check the signature"
     else
         echo "ferry install: minisign not found; checked SHA256SUMS only" >&2
     fi
     ```

     - `case "$public_key" in *@*)` 要像版本一樣拒絕未替換的模板。
     - `FERRY_PUBLIC_KEY` 只供 `make install-test` 使用，限定與 `FERRY_DOWNLOAD_URL` 同時出現時才採用。
     - 要在文件說明：`curl | sh` 本來就信任下載 `install.sh` 的 TLS 連線；真正的保護在 client 與 `ferry update` 內。
     - `-Q` 只印 trusted comment；確認所用 minisign 版本的選項。
  6. **`Makefile`**：
     - `install-script` 多替換 `@PUBLIC_KEY@`。
     - `install-test` 以 `minisign -G -W` 產生一把一次性的測試金鑰，簽 `served/SHA256SUMS`，以 `FERRY_PUBLIC_KEY` 傳入；並加一個「改過 `SHA256SUMS` 就安裝失敗」的負面測試。
- 要加的測試：
  - `src/remote/release.rs` 的 `mod tests`（unit），以 dev-dependency `minisign` 在測試中產生金鑰與簽章，`served()` 同時寫出 `SHA256SUMS.minisig`：
    - 有效簽章：下載、檢查、保存都成功；伺服器消失後，再次 `fetch` 仍以快取成功（重新驗簽章）。
    - 竄改 `SHA256SUMS`（同時竄改執行檔使 checksum 相符）：被拒絕。
    - 以另一把金鑰簽：被拒絕。
    - trusted comment 為另一個版本：被拒絕。
    - 缺少 `.minisig`：被拒絕。
    - 快取中的 `.minisig` 被刪掉或改壞：重新下載。
    - 金鑰清單有兩把、以第二把簽：被接受。
  - `Makefile` 的 `install-test`（以 `make install-test` 執行，CI 的 release job 已經跑它）：正面與負面各一。
  - `release.yml`：簽章後立即以 repo 內的公鑰驗證的步驟本身，就是發布前的檢查。
- 驗收條件：
  - [ ] 每個 release 都有 `SHA256SUMS.minisig`，trusted comment 為 `ferry <version> SHA256SUMS`。
  - [ ] client 在 host 上安裝與 `ferry update`，遇到沒有簽章、簽章無效、版本不符時，都拒絕並說明原因，不會下載或替換任何東西。
  - [ ] 簽章私鑰只有 `sign` job 在維護者核准後才拿得到；`build` 與 `publish` job 都拿不到。
  - [ ] `install.sh` 有 `minisign` 時驗證，沒有時明白說只檢查了 checksum；`FERRY_REQUIRE_SIGNATURE=1` 時沒有 `minisign` 就失敗。
  - [ ] 公鑰只有一個來源檔案（`src/remote/release.pub`），程式與 `install.sh` 都從它產生。
  - [ ] repository 啟用 immutable releases（設定項，記在發布流程的文件中）。
  - [ ] 舊的 client（0.14.x）照常可以安裝新的 release。
- 文件要同步的地方：
  - `README.md`「Install」：說明會檢查簽章，以及如何以 `minisign` 手動驗證，並附上公鑰。
  - `docs/cli.md`「Installing on a host」第 3 點、「`ferry update`」、「Files」表中 `~/.ferry/releases/<version>/` 那一列（多了簽章檔），以及 `cli.zh-TW.md` 對應的「在 host 上安裝」、「`ferry update`」、「檔案」。
  - `docs/design/architecture.md`「Installing on a remote host」中「A release build installs a binary」與「Releases are built by CI」兩點，以及 `architecture.zh-TW.md`「在遠端 host 上安裝」。可新增一個小節寫金鑰管理與輪替。
- 相關項目：TRN-02（上傳到 host 後的驗證）、CLI/CI 審查中 pin actions by SHA 與 release 快取的項目。

---

### TRN-04：多行的 `sh -c '…'` 在 csh/tcsh 登入 shell 上失敗

- 嚴重度：中
- 驗證狀態：
  - 推測：依 csh/tcsh 的語法，單引號字串中不能有未跳脫的換行，否則報 `Unmatched '.`。本機沒有 tcsh、csh、fish，未實際執行。
  - 已核對程式碼：確認了哪些腳本是多行的。
- 位置：
  - `src/remote/shell.rs:1-5`：模組註解說「whatever that shell is」。
  - `src/remote/shell.rs:21-23`：`sh_command`。
  - `src/transport/ssh.rs:55-59`：`Ssh::script`。
  - 多行的腳本：
    - `src/remote/probe.rs:31-66`：探測。
    - `src/remote/install.rs:29-38`：`keep_recent`。
    - `src/remote/install.rs:42-56`：release 安裝。
    - `src/remote/install.rs:116-141`：dev build。
  - 單行的腳本（不受影響）：
    - `src/remote/mod.rs:54-61`：server 啟動。
    - `src/remote/sudo.rs:52-53`、`:119-130`。
    - `src/cli.rs:672-678`：`kind_script`。
- 現象：
  - 使用者在 host 上的登入 shell 是 tcsh 或 csh 時，第一次對它執行就失敗：先印 `Unmatched '.`，接著 `could not probe <host>: it exited with exit status: 1`。部分大學與 HPC 叢集的預設 shell，以及 FreeBSD 的 root，都是這種情況。
  - 以 `--remote-ferry` 可以繞過，因為那時不探測。
- 根本原因：
  1. sshd 以使用者的登入 shell 執行 `ssh` 傳來的指令字串：`$SHELL -c "<指令字串>"`。
  2. Ferry 傳的指令字串是 `sh -c '<script>'`：

     ```rust
     // src/remote/shell.rs:20-23
     /// The command line that runs `script` with `sh` on a host.
     pub fn sh_command(script: &str) -> String {
         format!("sh -c {}", quote(script))
     }
     ```

  3. POSIX sh 的單引號字串可以包含換行；csh 與 tcsh 不行。探測與安裝腳本有很多行，因此在 csh 中就語法錯誤，`sh` 根本沒有被執行。
  4. fish 的單引號中 `\\` 與 `\'` 是跳脫序列。目前的腳本沒有 `\\`，但若 `host_dir` 等被 `quote` 的值含反斜線就會出錯。這是次要的。
- 重現方式：
  - 在有 tcsh 的機器上：`tcsh -c "sh -c 'echo a
    echo b'"`，得到 `Unmatched '.`。
  - 端到端：在測試用的 VM 上 `chsh -s /bin/tcsh <user>`，再 `ferry copy ./x <user>@localhost:y`。
- 修法：
  - 讓 `sh_command` 產生一行不含換行、也不含 `!` 的指令（csh 在某些情況會做 `!` 歷史替換）。做法是把 script 以 `assign` 用的 octal 跳脫編成 `printf` 的格式字串，在 `sh` 裡 `eval`：

    ```rust
    // src/remote/shell.rs
    /// The command line that runs `script` with `sh` on a host, on one line of
    /// plain characters, so that any login shell passes it on as it is:
    /// csh rejects a newline inside quotes.
    pub fn sh_command(script: &str) -> String {
        let escaped: String = script.bytes().map(|byte| format!("\\{byte:03o}")).collect();
        format!("sh -c {}", quote(&format!("eval \"$(printf '{escaped}')\"")))
    }
    ```

  - 產生的指令形如 `sh -c 'eval "$(printf '\''\163\145…'\'')"'`：
    - csh、tcsh、fish 與 POSIX sh 都只看到單引號字串與 `\'`，沒有換行、`!` 或 `$(`（`$(` 在單引號內）。
    - `printf` 的 `\NNN` 是 POSIX 規定的；`%` 被編成 `\045`，印出時是字面上的 `%`。已在 dash、bash、zsh 確認。
    - `$( )` 會去掉腳本結尾的換行，對腳本沒有影響。
    - `eval` 在同一個 shell 中執行，`set -e`、`exec`、`trap` 照常。stdin 照常繼承，安裝腳本的 `cat` 仍讀得到上傳的資料。
  - 長度約變為 4 倍：探測腳本約 1.5 KB，變成約 6 KB，遠低於 `ARG_MAX` 與 sshd 的限制。
  - 不採用的做法：
    - 只跳脫換行：csh 要的是「反斜線加換行」，但在 POSIX sh 的單引號裡，反斜線會留下來。沒有一種跳脫同時適合兩者。
    - 以 `sh -s` 從 stdin 送腳本：安裝腳本的 stdin 要用來傳資料。
    - base64：`base64` 不是 POSIX 工具。
  - 一併更新模組註解。
- 要加的測試（`src/remote/shell.rs` 的 `mod tests`，unit）：
  - `sh_command` 的輸出不含 `\n`、`!`，也不含單引號外的 `$`。
  - 來回測試：多行、含單引號、`$`、`%`、反斜線、非 UTF-8 位元組的腳本，經 `sh -c <sh_command(...)>` 執行的輸出，與直接 `sh -c <script>` 相同。可沿用 `any_bytes_are_assigned_as_they_are` 與 `a_script_survives_the_login_shell` 的寫法。
  - 若 `PATH` 上有 `tcsh`、`csh` 或 `fish`，以它們執行同一條指令並比對輸出；沒有就跳過。建議在 CI 的一個 Linux job 以 apt 安裝 tcsh 與 fish，讓這個測試真的執行。這是 CI 變更，與 CLI/CI 審查協調。
  - `src/remote/probe.rs` 的 `the_script_runs_here` 改為經 `sh_command` 執行。
- 驗收條件：
  - [ ] 每個 Ferry 送到 host 的指令字串都是單行，不含 `!`。
  - [ ] 探測與安裝在登入 shell 為 tcsh 或 fish 的 host 上成功（有 tcsh 的 CI job 或手動驗證）。
  - [ ] 現有的 shell 與探測測試仍通過。
- 文件要同步的地方：
  - `docs/cli.md`「Remote hosts」第一段與 `cli.zh-TW.md`「遠端 host」：寫明任何登入 shell 都可以，包括 csh、tcsh 與 fish。
  - `docs/design/architecture.md`「Transport」中「The server's answer goes between marks」提到登入 shell 之處，以及 `architecture.zh-TW.md`「Transport」：不用改內容，確認用詞一致。
- 相關項目：TRN-06（ssh 設定的影響）、TRN-19（探測輸出沒有標記）。

> 2026-10-20 補記：WP-10 照修法做：`sh_command` 把腳本的每個位元組寫成 `printf` 的八進位跳脫，在 `sh` 裡 `eval`。測試：`the_command_line_is_plain`（單行、沒有 `!`、`$` 只在單引號內）、`a_script_of_many_lines_runs_as_it_is`、`other_login_shells_pass_the_script_on`（有 csh、tcsh 或 fish 才跑）；`the_script_runs_here` 改為經 `sh_command` 執行。CI 的 Ubuntu 26.04 x86_64 job 安裝 csh、tcsh 與 fish，macOS 本來就有 csh 與 tcsh。本機沒有這些 shell，只在 CI 上實際跑過；沒有在登入 shell 是 tcsh 或 fish 的真實 host 上端到端驗證。整合測試的 `logging_ssh` 記下的是編碼後的指令，`copy_logged` 改為先解碼（`script_of`）。

---

### TRN-05：Ferry 只收到 SIGTERM/SIGHUP 時留下 ssh 行程，`ssh -N` 讓 master 永不過期

- 嚴重度：中
- 驗證狀態：已實測重現。
  - 以假 ssh（`-N` 以 `exec sleep 100000` 代替 holder），用 `setsid` 讓 Ferry 自成一個行程群組，延遲 Destination 的 server 啟動，期間只對 Ferry 的 pid 送 SIGTERM。
  - 結果：Ferry 結束後，holder 與尚在等待的 `-T` session 都還在。
  - 真的 `ssh -N` 因為 stdin 是 `/dev/null`，不會察覺父行程消失；這一點依 OpenSSH 的行為推論。
- 位置：
  - `src/interrupt.rs:65-98`：`install`，訊號 thread 在 97 呼叫 `std::process::exit`。
  - `src/interrupt.rs:155-161`：`stop`。
  - `src/interrupt.rs:164-172`：`on_exit` 與 `before_exit`。
  - `src/interrupt.rs:177-189`：`unblock_in_child`。
  - `src/transport/mod.rs:54-58`：`Held`。
  - `src/transport/mod.rs:315-324`：`Drop for Transport`。
  - `src/transport/mod.rs:356-383`：`hold`，holder 的 stdin 與 stdout 都是 null。
  - `src/transport/mod.rs:269-286`：`Link::spawn`。
- 現象：
  - Ferry 被 `kill <pid>`、systemd、程序管理器，或只對主行程送訊號的 CI 取消結束時，每條 SSH connection 的 `ssh -N` 都留在系統裡，被 init 收養，一直執行。
  - Ferry 自己的 ControlMaster 的 `ControlPersist=600`，要等最後一個 client 離開才開始計時。holder 一直在，所以 master 與它已登入的 TCP 連線無限期保留。
  - 若用的是使用者自己的 ControlMaster、且沒有設 `ControlPersist`，holder 就是 master 本身。
  - Ctrl-C 不受影響：終端機把 SIGINT 送給整個前景行程群組，ssh 自己也會收到。
- 根本原因：
  1. 收到 SIGINT、SIGTERM 或 SIGHUP 時，訊號 thread 清掉暫存檔後直接 `exit`，不執行任何 destructor：

     ```rust
     // src/interrupt.rs:93-97
     // Held until the process exits: nothing is created from now on.
     let _gate = GATE.write();
     remove_all();
     before_exit();
     std::process::exit(128 + signal);
     ```

  2. 結束 holder 的只有 `Drop for Transport`：

     ```rust
     // src/transport/mod.rs:315-324
     impl Drop for Transport {
         fn drop(&mut self) {
             if let Self::Ssh(held) = self {
                 for held in held {
                     let _ = held.holder.kill();
                     let _ = held.holder.wait();
                 }
             }
         }
     }
     ```

  3. holder 的 stdin 與 stdout 是 `/dev/null`（`hold`，`mod.rs:365-370`），不會因為 Ferry 結束而收到 EOF。其他 ssh（`-T` 的 control connection、`-W` 的 path）的 stdin 是 Ferry 的管道，Ferry 結束時會收到 EOF 而陸續結束，所以主要洩漏的是 holder。
  4. `interrupt::stop()`（server 端在 client 消失時呼叫）也一樣不執行 destructor；server 本身不產生 ssh，所以只影響 client。
- 重現方式：

  ```sh
  # 在 repo 根目錄；先依 TRN-01 的重現方式建置，並建立 $W/src
  M=$PWD/docs/review/2026-10-17/materials/transport; W=$PWD/.scratch/review-transport
  F=$W/target/debug/ferry; rm -rf $W/dst
  (DELAY=20 FERRY_SSH=$M/bin/fakessh setsid $F copy --no-ui --yes --remote-ferry $F $W/src fakehost:$W/dst </dev/null >$W/term.log 2>&1 & echo $! > $W/pid)
  until pgrep -f 'sleep 100000' >/dev/null; do sleep 0.2; done
  kill -TERM "$(cat $W/pid)"; sleep 1
  pgrep -af 'sleep 100000'      # holder 仍在
  pkill -f 'sleep 100000'       # 清理
  ```

- 修法：
  - 建議：一份全域的「結束時要結束的子行程」登記表，在訊號 thread 與 `stop()` 的路徑上結束它們：

    ```rust
    // src/interrupt.rs
    static CHILDREN: Mutex<Vec<u32>> = Mutex::new(Vec::new());

    /// Ends `child` with SIGTERM if the process exits on a signal, until the
    /// guard is dropped, which must be after the child is waited for.
    pub fn end_on_exit(child: &Child) -> EndOnExit { … push child.id() … }
    pub struct EndOnExit(u32);
    impl Drop for EndOnExit { fn drop(&mut self) { /* remove self.0 */ } }

    fn end_children() {
        for &pid in CHILDREN.lock().expect("not poisoned").iter() {
            // SAFETY: the pids are of children not yet waited for.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM); }
        }
    }
    ```

  - 在訊號 thread（97 行之前）與 `stop()` 中呼叫 `end_children()`，放在 `remove_all()` 之後。
  - `Held` 加一個 `EndOnExit` 欄位，在 `Transport::open` 登記 holder。`Drop for Transport` 中 `wait()` 之後才丟棄 guard。
  - 也可以登記 path 與 control connection 的 ssh；它們本來會因 EOF 結束，登記可以結束得更快。
  - pid 重複使用的問題：子行程在被 `wait` 之前不會被回收，pid 不會被重用。guard 在 `wait` 之後才移除，所以登記中的 pid 一定還屬於那個子行程。
  - 用 SIGTERM：ssh 對它的處理是正常結束，mux client 中斷與 master 的連線。
  - 不建議的做法：
    - 讓 ssh 自成行程群組再對群組送訊號：那樣終端機的 SIGINT 與 tty 存取都會改變；背景行程群組讀 `/dev/tty` 會收到 SIGTTIN，密碼提示就會壞掉。
    - Linux 的 `PR_SET_PDEATHSIG`：它綁定的是產生子行程的 thread，不是整個行程。從短命的 thread 產生 ssh 會讓 ssh 提早被殺。macOS 也沒有對應的機制。可以只當作 Linux 上額外的保險，而且要確認所有 spawn 都在主 thread。
  - SIGKILL 無法處理，holder 仍會留下。要在文件說明。
- 要加的測試（`tests/remote.rs`，integration）：
  - 新的假 ssh：`-N` 時把 `$$` 寫進檔案再 `exec sleep 1000`；server 啟動指令延遲（例如 30 s，讓 Ferry 停在準備期間）；用 `--remote-join`。
  - 以 `Command::spawn` 啟動 Ferry，輪詢直到 pid 檔出現，只對 Ferry 送 SIGTERM，等它結束。
  - 斷言 holder 的 pid 在 2 秒內消失（`kill(pid, 0)` 回 `ESRCH`）。
  - SIGHUP 同樣測一次。
  - 測試結束時無論成敗都清掉 holder，以免污染 CI。
- 驗收條件：
  - [ ] Ferry 因 SIGINT、SIGTERM 或 SIGHUP 結束時，它啟動的每個 `ssh -N` 都在 Ferry 結束後立即結束。
  - [ ] 正常結束與錯誤結束的行為不變，仍由 `Drop for Transport` 處理。
  - [ ] Ctrl-C 時密碼提示與終端機行為不變。
- 文件要同步的地方：
  - `docs/cli.md` 開頭描述 Ctrl-C、SIGTERM、SIGHUP 會移除暫存檔的那一段，與 `cli.zh-TW.md` 的對應段落：補上「並結束它啟動的 ssh」。
  - `docs/design/architecture.md`「Connections」中「An SSH connection is opened with no task」與 `architecture.zh-TW.md`「Connection」中「SSH connection 開啟時不執行任何工作」：補上 holder 在 Ferry 結束時（含被訊號結束）一定會結束；SIGKILL 例外。
- 相關項目：TRN-01、TRN-17（`connect` 失敗時的子行程）、TRN-18。

> 2026-10-20 補記：WP-11 照修法做：`interrupt::end_on_exit` 登記子行程，訊號 thread、`stop()` 與 panic 的 `exit_now` 都在結束前送 SIGTERM。只登記 holder（`hold` 一啟動就登記，`Held` 持有 guard，在 `wait` 之後才丟棄）；修法列為可選的 path 與 control connection 的 ssh 沒有登記，它們本來就會因為 stdin 的 EOF 結束。測試：`tests/remote.rs` 的 `ferry_ended_by_a_signal_ends_the_ssh_holding_its_connection`（Linux），SIGTERM 與 SIGHUP 各一次；暫時拿掉 SIGTERM 時，這個測試會失敗。

---

### TRN-06：使用者 ssh_config 的 `RemoteCommand` 與轉發設定會破壞或干擾 Ferry 的 session

- 嚴重度：中
- 驗證狀態：部分實測。用本機 OpenSSH 9.6p1，以一個不存在的 `.invalid` host，並以 `-F` 指定的設定檔（內容見下方的重現方式）測試；這些檢查都發生在連線之前，不會連到任何 host：
  - 設定檔有 `RemoteCommand` 時，`ssh -T -- host 'cmd'` 立即以 255 結束，印出 `Cannot execute command-line and remote command.`。
  - `-N` 與 `-W` 不受這個檢查影響。
  - 加上 `-o RemoteCommand=none -o ClearAllForwardings=yes` 後，這個檢查就通過了。
  - `ssh -G` 確認設定檔的 `LocalForward` 會套用，而 `ClearAllForwardings=yes` 會生效。
  - 多個 master 綁同一個轉發埠的衝突，是依 OpenSSH 的行為推論的，未對真實 host 測試。
- 位置：
  - `src/transport/ssh.rs:37-46`：`Ssh::new` 的選項。
  - `src/transport/ssh.rs:84-92`：`Ssh::ssh`，每個 ssh 指令的共同部分。
  - `src/transport/ssh.rs:128-153`：`control_options`。
- 現象：
  - (a) `~/.ssh/config` 對該 host 設了 `RemoteCommand`（例如登入就自動 `tmux new -A`）：
    - 每條 control connection（探測、安裝、`root_kind`、server 啟動）都失敗：`Cannot execute command-line and remote command.`。
    - `--remote-join` 的每條 path 也失敗。
    - Ferry 對這個 host 別名完全不能用。
  - (b) 設了 `LocalForward`、`RemoteForward` 或 `DynamicForward`：
    - Ferry 自己的每個 master 都會建立這些轉發。
    - `--ssh-connections N`（N ≥ 2）時，第 2 條之後綁不到同一個本機埠，出現 `Address already in use` 的警告。若同時設了 `ExitOnForwardFailure yes`，那條 SSH connection 登入失敗，Ferry 中止。
    - Ferry 結束後，它的 master 還保留 10 分鐘（`ControlPersist=600`），佔住使用者的轉發埠；這段時間內，使用者自己的 `ssh host` 建不了轉發。
- 根本原因：Ferry 只加上自己的幾個選項，使用者設定中的其他一切都照樣套用：

  ```rust
  // src/transport/ssh.rs:37-46
  pub fn new(host: &str, index: usize, yours: bool) -> Result<Self> {
      let mut options = vec!["-o".to_owned(), "ConnectTimeout=10".to_owned()];
      if !(index == 0 && yours) {
          options.extend(control_options(index)?);
      }
      Ok(Self { host: host.to_owned(), options })
  }
  ```

  文件說「`~/.ssh/config` 中的一切都適用」，但 `RemoteCommand` 與轉發和 Ferry 自己的 session 互相衝突。
- 重現方式：

  ```sh
  mkdir -p .scratch/review-transport && C=$PWD/.scratch/review-transport/ssh_config
  printf 'Host nohost.invalid\n  RemoteCommand tmux new -A\n  LocalForward 127.0.0.1:65001 127.0.0.1:22\n' > $C
  ssh -F $C -o BatchMode=yes -T -- nohost.invalid 'echo hi'    # Cannot execute command-line and remote command.
  ssh -F $C -o BatchMode=yes -o RemoteCommand=none -o ClearAllForwardings=yes -T -- nohost.invalid 'echo hi'
  # 通過上述檢查，接著才因為無法解析主機而失敗
  ssh -F $C -o RemoteCommand=none -o ClearAllForwardings=yes -G nohost.invalid | grep -Ei '^(remotecommand|localforward|clearall)'
  ```

  端到端：在 `~/.ssh/config` 對一個測試 host 加上 `RemoteCommand`，然後 `ferry copy ./x thathost:y`。
- 修法：
  - 在 `Ssh::ssh()`（`ssh.rs:84-92`）中，`--` 之前為每個 ssh 指令都加上：
    - `-o RemoteCommand=none`
    - `-o ClearAllForwardings=yes`
  - 命令列的 `-o` 優先於設定檔（第一個取得的值有效），所以加在 `self.options` 裡即可。
  - 可一併考慮（加固，選擇性）：
    - `-o PermitLocalCommand=no`：`LocalCommand` 會在每次新的 master 登入時在本機執行。
    - `-o ForwardX11=no`：Ferry 的 session 用不到。
    - `ForwardAgent` 不要改：使用者也許依賴它。
  - 使用者自己的 ControlMaster（`yours`，第 0 條）：
    - Ferry 的 mux client 加上 `ClearAllForwardings` 只是不再重複要求轉發，不影響使用者的 master。
    - 若 holder 剛好成為使用者設定中的那個 master（`yours` 為真，但 master 還沒在跑），那個 master 就沒有使用者的轉發。之後使用者的 `ssh host` 作為 mux client 連上時，會透過 master 要求自己的轉發（OpenSSH 的 mux client 會送出設定中的轉發），所以仍會建立。
  - `yours()`（`ssh.rs:103-122`）不需要這些選項。
  - `FERRY_SSH` 指定的程式也會收到這些選項，與 ssh 相容的包裝程式應能接受。
- 要加的測試：
  - `src/transport/ssh.rs` 的 `mod tests`（unit）：`Ssh::new("h", 0, false)` 與 `Ssh::new("h", 1, false)` 的 `script`、`forward`、`hold`、`check` 的 `get_args()` 都含 `RemoteCommand=none` 與 `ClearAllForwardings=yes`，且出現在 `--` 之前。
  - 同一處（unit，有 `ssh` 才跑，否則跳過）：以含 `RemoteCommand` 與 `LocalForward` 的暫時設定檔，執行 `ssh -F <cfg> <Ferry 的選項> -G host`，斷言輸出沒有 `remotecommand`、`clearallforwardings yes`。
- 驗收條件：
  - [ ] 使用者設定了 `RemoteCommand` 的 host，Ferry 照常可用。
  - [ ] 設定了 `LocalForward` 且 `ExitOnForwardFailure yes` 的 host，`--ssh-connections 4` 可以登入。
  - [ ] Ferry 結束後，使用者的轉發埠不被 Ferry 的 master 佔住。
- 文件要同步的地方：
  - `docs/cli.md`「Remote hosts」第一段（「everything in `~/.ssh/config` applies」）與 `cli.zh-TW.md`「遠端 host」：補上例外：Ferry 的 session 不執行 `RemoteCommand`，也不建立轉發。
  - `docs/design/architecture.md`「Connections」第一點（Ferry 加上的 ssh 選項）與 `architecture.zh-TW.md`「Connection」。
- 相關項目：TRN-13（`FERRY_SSH` 的分詞）、TRN-14（`ConnectTimeout`）、TRN-04。

> 2026-10-20 補記：WP-10 照修法做，另外加上修法列為選擇性的 `PermitLocalCommand=no` 與 `ForwardX11=no`；`ForwardAgent` 不動。選項放在 `Ssh::new` 的 `options` 裡，所以每個指令都在 `--` 之前帶著它們。測試：`no_session_runs_a_remote_command_or_forwards`，以及有 `ssh` 時以 `-F` 的暫時設定檔跑 `ssh -G` 的 `ssh_itself_takes_the_options_over_the_configuration`。驗收條件中三項真實 host 的情境沒有實測。

---

### TRN-07：`/tmp` 下可預測的目錄名稱讓其他本機使用者能阻斷 Ferry；中斷時殘留 join socket

- 嚴重度：低
- 驗證狀態：已核對程式碼。擁有者、模式與 symlink 的檢查都正確，無法劫持；只能阻斷服務。
- 位置：
  - `src/home.rs:41-58`：`runtime_dir` 與 `sudo_runtime_dir`。
  - `src/transport/ssh.rs:128-153`：`control_options`。
  - `src/transport/ssh.rs:155-175`：`private_directory`。
  - `src/transport/join.rs:89-96`：`sockets`。
  - `src/transport/join.rs:106-128`：`sudo_sockets`。
  - `src/transport/join.rs:133-155`：`listen`。
  - `src/transport/join.rs:208-212`：`Drop for Joining`。
  - `src/transport/join.rs:224-233`：`join`，從 token 推出路徑。
  - `src/peer.rs:42-63`：交握中呼叫 `listen`；stdin 關閉時呼叫 `stop`。
  - `src/peer.rs:103`：`drop(joining)`。
  - `src/interrupt.rs:155-161`：`stop`。
- 現象：
  - 在共用的機器上（例如 HPC 登入節點），任何本機使用者都能先建立 `/tmp/ferry-<受害者 uid>`（或 `/tmp/ferry-sudo-<uid>`、`/tmp/ferry-dev-<uid>`）。之後受害者在這台機器上的 server 每次都失敗：`/tmp/ferry-1234 must be a directory owned by you with mode 0700`，直到 root 刪掉它。
  - 在 client 機器上，同樣能阻斷 Ferry 自己的 ControlMaster。
  - 另外，server 在 accept 迴圈中因 client 消失而 `interrupt::stop(1)` 時（`peer.rs:55-63`），`Joining` 的 `Drop` 不會執行，`/tmp/ferry-<uid>/join/<token>` 就殘留下來。
- 根本原因：
  1. 目錄名稱是固定的：

     ```rust
     // src/home.rs:43-46
     pub fn runtime_dir() -> PathBuf {
         let uid = rustix::process::getuid().as_raw();
         PathBuf::from(format!("/tmp/{}-{uid}", runtime_prefix()))
     }
     ```

  2. `private_directory` 遇到別人擁有的目錄只會拒絕（`ssh.rs:163-172`），不會改用別的目錄。這樣做是正確的，但沒有退路。
  3. `stop()` 呼叫 `process::exit`，所以不會移除 socket。
- 重現方式（需要兩個本機帳號 A 與 B）：

  ```sh
  # 以 B：
  mkdir /tmp/ferry-$(id -u A)
  # 以 A：
  ferry copy ./x ./y   # 本機 transport 也用 join socket：失敗並說明該目錄必須是 A 的
  ```

- 修法：
  - client 的 ControlPath 目錄要在不同次執行之間保持一致，`ControlPersist` 的重複使用才有效。所以採用確定性的搜尋：依序試 `/tmp/ferry-<uid>`、`/tmp/ferry-<uid>-1`、`-2`…（例如到 `-9`），用第一個是自己的且模式為 0700、或能新建的目錄。每次執行都會找到同一個。
  - server 的 join socket 不需要跨次一致：
    - Linux 上若有 `$XDG_RUNTIME_DIR`（`/run/user/<uid>`，使用者自己的、0700、登出時清空），且它屬於自己、模式為 0700，優先使用 `$XDG_RUNTIME_DIR/ferry/join`；否則用上述搜尋。
    - macOS 的 `$TMPDIR`（`/var/folders/…/T/`）是每個使用者自己的，但路徑長；join socket 的路徑（約 50 加 32 位元組）還在 103 以內，可以用。ControlPath 加上 62 位元組的後綴會超過 103，所以 client 不能用它。
  - `ferry server --join <token>` 目前從 token 推出 socket 路徑（`join.rs:228-233`）。改為 `--join <socket path>`：
    - 驗證 basename 是 32 位十六進位的 token，所在目錄屬於自己且為 0700；sudo 時目錄屬於 root、0711，socket 屬於自己。
    - client 從 `Welcome.socket` 取得路徑，已經有了。
  - 殘留的 socket：
    - 在 `interrupt` 加一個「結束時要 unlink 的路徑」登記表，可與 TRN-05 的子行程登記表放在一起。`listen` 登記，`Drop for Joining` 取消登記；`stop()` 與訊號 thread 結束前 unlink。
    - 可選：`listen` 時順便清掉同目錄中連不上（`ECONNREFUSED`）的舊 socket。
  - 搜尋與驗證的邏輯放在一個函式裡，例如 `home::private_runtime_dir(kind) -> Result<PathBuf>`，client 與 server 共用。
- 要加的測試：
  - `src/home.rs` 或 `src/transport/ssh.rs` 的 `mod tests`（unit）：把基底路徑改為可注入，例如 `private_runtime_dir_in(base: &Path, …)`。在暫時目錄中先建一個模式錯誤（0755）的 `ferry-<uid>`，斷言選到 `ferry-<uid>-1`，再呼叫一次仍選到同一個。無法在測試中建立他人擁有的目錄，以模式錯誤代替。
  - `src/transport/join.rs` 的 `mod tests`（unit）：`--join` 給一個 basename 不是 token、或目錄模式錯誤的路徑時被拒絕。
  - `src/transport/join.rs` 的 `mod tests`（unit）：`listen` 之後走登記表的 unlink，socket 檔消失。
- 驗收條件：
  - [ ] 別人先建立了 `/tmp/ferry-<uid>` 時，Ferry 改用另一個目錄，仍能運作，而且每次都用同一個。
  - [ ] 不會使用任何不屬於自己、或他人可寫入的目錄。
  - [ ] server 被 `stop()` 結束時，不留下 join socket。
- 文件要同步的地方：
  - `docs/cli.md`「Files」表中 `/tmp/ferry-<uid>/ssh/` 與 `/tmp/ferry-<uid>/join/` 兩列，以及 `cli.zh-TW.md`「檔案」：寫明備用名稱與 `$XDG_RUNTIME_DIR`。
  - `docs/design/architecture.md`「Connections」第一點（ControlPath 的位置）與 `architecture.zh-TW.md`「Connection」。
- 相關項目：TRN-05（登記表）、TRN-09。

> 2026-10-20 補記：WP-11 照修法做，與修法不同之處：
> - client 與 server 共用 `home::runtime_dir`：依序試 `/tmp/ferry-<uid>`、`-1` 到 `-9`，用第一個只屬於自己、模式 0700 的目錄（`home::first_private`）。server 的 join socket 在 Linux 上優先放在 `$XDG_RUNTIME_DIR/ferry/join`；修法提到的 macOS `$TMPDIR` 沒有採用，macOS 照樣用 `/tmp` 的搜尋。
> - `ferry server --join` 改收 socket 的路徑；`Welcome` 的 `token` 欄位就不再需要，已移除。joiner 驗證：檔名是 32 位十六進位的 token；一般情況，所在目錄屬於自己、模式 0700；sudo 時，所在目錄不能被 group 或 others 寫入、擁有者是 root 或自己（測試用的假 sudo 不是 root），socket 屬於自己。修法寫的是「目錄屬於 root、0711」。
> - sudo 的目錄 `/tmp/ferry-sudo-<uid>` 也加上同樣的備用名稱。
> - socket 以 `interrupt::remove_on_exit` 登記，`stop()` 與訊號都會移除它。修法列為可選的「listen 時清掉連不上的舊 socket」沒有做。
> - 測試：`a_runtime_directory_taken_is_passed_over_for_a_spare`（以模式錯誤代替別人擁有）、`a_socket_in_a_shared_directory_is_not_joined`、`the_socket_is_removed_once_let_go`。沒有以兩個帳號實測。

---

### TRN-08：IPv6 位址的位置 `[addr]:path` 解析錯誤

- 嚴重度：低
- 驗證狀態：已核對程式碼。
- 位置：
  - `src/remote/location.rs:23-42`：`Location::parse`。
  - `src/remote/location.rs:45-81`：測試。
- 現象：
  - `ferry copy ./x '[::1]:dir'` 被讀成 host `[`、path `:1]:dir`，ssh 回報無法解析主機 `[`。
  - `me@[fe80::1]:dir` 也一樣。
  - scp 與 rsync 都接受這種寫法；不加中括號又沒辦法寫 IPv6 位址，因為第一個冒號就被當成分隔。
- 根本原因：

  ```rust
  // src/remote/location.rs:24-34（節錄）
  let bytes = argument.as_bytes();
  let colon = bytes.iter().position(|&byte| byte == b':');
  let slash = bytes.iter().position(|&byte| byte == b'/');
  let Some(colon) =
      colon.filter(|&colon| colon > 0 && slash.is_none_or(|slash| colon < slash))
  else {
      return Ok(Self::Local(PathBuf::from(OsString::from(argument))));
  };
  ```

  找的是第一個冒號，沒有考慮中括號。
- 重現方式：在 `location.rs` 的測試中，`parse("[::1]:dir")` 得到 `Remote { host: "[", path: ":1]:dir" }`。
- 修法：
  - 在找冒號之前，先看可選的 `user@` 之後是否以 `[` 開頭：
    - 是的話，找對應的 `]`，且 `]` 之後緊接著必須是 `:`。
    - host 為 `user@` 加上中括號內的位址，不含中括號；ssh 接受 `ssh ::1` 與 `ssh me@::1`，它以最後一個 `@` 分出使用者。
    - path 為 `]:` 之後的部分。
  - 中括號沒有接著 `]:`（例如 `[::1]` 或 `[a]b:c`）：照目前的規則處理。`[::1]` 沒有冒號會被當成本機路徑；只要冒號在第一個 `/` 之前，就照目前的規則讀成 host。或者回報錯誤並提示寫法；選一種並寫進文件。
  - 以 `-` 開頭的檢查同樣適用於中括號內，例如 `[-oProxyCommand=x]:y` 要被拒絕。
  - 本機路徑仍可用 `./[::1]:x` 寫。
  - `transport.place()` 之後顯示的是 `::1`，訊息中可能要加回中括號，選擇性。
- 要加的測試（`src/remote/location.rs` 的 `mod tests`，unit）：
  - `[::1]:dir` 得到 host `::1`、path `dir`。
  - `me@[fe80::1]:/abs` 得到 host `me@fe80::1`、path `/abs`。
  - `[::1]:` 得到家目錄（path 為空）。
  - `[-x]:y` 是錯誤。
  - `./[::1]:x` 是本機路徑。
  - `[::1]` 是本機路徑。
- 驗收條件：
  - [ ] 上述情況都照預期解析。
  - [ ] 現有的測試都通過。
- 文件要同步的地方：`docs/cli.md`「Locations」表格加一列 `[user@][address]:path`（IPv6），與 `cli.zh-TW.md`「位置」。
- 相關項目：無。

> 2026-10-20 補記：WP-10 照修法做。中括號之後不是 `:` 時（例如 `[::1]` 或 `[a]b:c`），修法給了兩種選擇，這裡選擇當成本機路徑，寫在 `Location::parse` 的說明裡。測試：`an_address_in_brackets_is_a_host`。訊息中顯示的 host 是去掉中括號的位址，沒有加回中括號。

---

### TRN-09：經 SCM_RIGHTS 收到的 fd 沒有 close-on-exec

- 嚴重度：低
- 驗證狀態：已核對程式碼。Linux 的 `recvmsg` 只有加上 `MSG_CMSG_CLOEXEC` 時，才會為收到的 fd 設 `FD_CLOEXEC`。
- 位置：`src/transport/join.rs:169-205`（`Joining::accept`，`RecvFlags::empty()` 在 178）。
- 現象：
  - 用 `--remote-join` 時，server 收到的 path 的 stdin 與 stdout（通往 sshd 的管道）沒有 `FD_CLOEXEC`。server 之後產生的子行程（Linux 上查名稱用的 `getent`）都會繼承它們。
  - 用 `--remote-sudo` 時，這些是 root 的子行程，持有使用者 session 的管道。
  - 子行程很短命，目前影響小；但任何活得比 server 久的子行程，都會讓遠端看不到這條 path 的 EOF。
- 根本原因：

  ```rust
  // src/transport/join.rs:174-180
  let read = rustix::net::recvmsg(
      &stream,
      &mut [IoSliceMut::new(&mut byte)],
      &mut control,
      RecvFlags::empty(),
  )
  ```

- 重現方式：在 Linux 上以 `--remote-join` 執行一次傳輸，傳輸進行時看 `ls -l /proc/<server pid>/fd` 與 `/proc/<server pid>/fdinfo/<fd>` 的 `flags`：收到的 fd 沒有 `O_CLOEXEC`（`02000000`）。
- 修法：
  - Linux：用 `RecvFlags::CMSG_CLOEXEC`。
  - macOS 沒有 `MSG_CMSG_CLOEXEC`：收到後立刻以 `fcntl(F_SETFD, FD_CLOEXEC)` 設定。accept 發生在 peer 開始之前，那時 server 沒有其他 thread 在產生子行程，不會有競爭。
  - 為了簡單，兩個平台都在收到之後再設一次（在 Linux 上無害）：

    ```rust
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let flags = RecvFlags::CMSG_CLOEXEC;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let flags = RecvFlags::empty();
    // … recvmsg(&stream, …, flags) …
    for fd in &received {
        rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC).context("receive a path")?;
    }
    ```

- 要加的測試（`src/transport/join.rs` 的 `mod tests`，unit）：
  - 把 `join` 拆成 `join_with(path: &Path, input: BorrowedFd, output: BorrowedFd)`；`join` 傳入 stdin 與 stdout。
  - 測試中在暫時目錄 `listen`（需要可注入的目錄，見 TRN-07），在另一個 thread 以兩個管道呼叫 `join_with`。
  - 斷言 `accept` 得到的 fd 有 `FD_CLOEXEC`（`rustix::io::fcntl_getfd`）。
- 驗收條件：
  - [ ] 在 Linux 與 macOS 上，收到的 fd 都有 `FD_CLOEXEC`。
  - [ ] `--remote-join` 的傳輸照常。
- 文件要同步的地方：無。
- 相關項目：TRN-07、TRN-11（假設 3 也與 close-on-exec 有關）。

> 2026-10-20 補記：WP-11 照修法做，兩個平台都在收到後再設一次 `FD_CLOEXEC`。測試沒有照修法拆出 `join_with`，而是把接收拆成 `receive`，直接以 socket pair 送兩個 fd 給它（`a_path_handed_over_is_closed_in_children`）。

---

### TRN-10：`FERRY_SUDO_PASSWORD` 留在所有子行程的環境中

- 嚴重度：低
- 驗證狀態：已核對程式碼。
  - 密碼本身不會出現在 argv 或 log。
  - 不會卡住：`check` 只送一行就關閉 stdin。
- 位置：
  - `src/remote/sudo.rs:66-72`：讀環境變數。
  - 會繼承它的子行程：
    - `src/transport/ssh.rs:97-99`：`ssh`。
    - `src/remote/release.rs:99-105`：`curl`。
    - `src/transport/mod.rs:92-100`：本機的 `sh` 與 `ferry server`。
  - `src/cli.rs:304-305`：`interrupt::install()` 是第一件事，它會啟動一個 thread。
- 現象：
  - 設了 `FERRY_SUDO_PASSWORD` 之後，Ferry 啟動的每個子行程都看得到它：`ssh`（以及它的 `ProxyCommand`、`LocalCommand`、askpass）、`curl`、本機的 `ferry server`。
  - 若使用者的 ssh 設定有 `SendEnv *` 或 `SendEnv FERRY_*`，而 host 的 sshd 以 `AcceptEnv` 允許，密碼會被送到 host，出現在遠端 shell 與 server 的環境中。同一個 host 上的 root 可以從 `/proc/<pid>/environ` 讀到。
- 根本原因：

  ```rust
  // src/remote/sudo.rs:66-72
  if let Some(password) = std::env::var_os("FERRY_SUDO_PASSWORD") {
      let sudo = Self {
          password: Some(password.into_encoded_bytes()),
      };
      sudo.check(transport)?;
      return Ok(sudo);
  }
  ```

  讀了之後沒有從環境中移除。
- 重現方式：設 `FERRY_SUDO_PASSWORD=x`，用 `tests/remote.rs` 那種假 ssh，讓它把 `env` 寫進檔案，然後執行 `ferry copy --remote-sudo …`；檔案中有 `FERRY_SUDO_PASSWORD=x`。
- 修法：
  - 在 `cli::run()` 的第一行、`interrupt::install()` 之前（此時還只有一個 thread），把值讀進 `static SUDO_PASSWORD: OnceLock<Option<Vec<u8>>>`，並 `unsafe { std::env::remove_var("FERRY_SUDO_PASSWORD") }`。
    - edition 2024 中 `remove_var` 是 unsafe。安全性的理由寫在註解：此時沒有其他 thread。
  - `Sudo::prepare` 改從那個 `OnceLock` 取值。
  - 也可以改用 `Command::env_remove` 在每個 spawn 點移除，但容易漏掉，不建議。
- 要加的測試（`tests/remote.rs`，integration）：沿用 `push_with_sudo`，讓假 ssh 把它看到的環境寫進檔案，斷言其中沒有 `FERRY_SUDO_PASSWORD`，而 sudo 仍拿到正確的密碼（現有的斷言）。
- 驗收條件：
  - [ ] 任何子行程的環境都沒有 `FERRY_SUDO_PASSWORD`。
  - [ ] `--remote-sudo` 以環境變數提供密碼的行為不變。
- 文件要同步的地方：
  - `docs/cli.md` 中說明 `--remote-sudo` 與 `FERRY_SUDO_PASSWORD` 的段落，以及「Environment」表：目前表中沒有 `FERRY_SUDO_PASSWORD`，補上一列，說明它只被讀取一次、不會傳給任何子行程。
  - `cli.zh-TW.md` 的對應段落與「環境」表。
- 相關項目：TRN-24。

> 2026-10-20 補記：WP-11 照修法做：`cli::run` 在 `interrupt::install` 之前呼叫 `remote::take_password_from_environment`，讀進 `OnceLock` 並從環境中移除。測試：`the_sudo_password_reaches_no_child`（假 ssh 把它看到的環境寫進檔案）。

---

### TRN-11：macOS 上 `messages_arrive_whole_and_in_order_until_the_end` 偶發卡住（調查計畫）

- 嚴重度：低。只影響 CI 的可靠性；目前沒有證據顯示是產品的 bug。
- 驗證狀態：推測。只做了程式碼分析，沒有在 macOS 上重現。這一項是調查計畫，不是修法。
- 位置：
  - `src/transport/mod.rs:774-793`：`echoes`。
  - `src/transport/mod.rs:795-818`：測試本身與回聲 server。
  - `src/transport/mod.rs:161-191`：等開頭標記。
  - `src/transport/mod.rs:649-727`：`split`。
  - `src/transport/mod.rs:743-758`：`Paths::wait`。
  - `src/transport/striped.rs:164-191`：`send` 與 `close`。
  - `src/transport/striped.rs:255-293`：`read_path`。
  - `src/transport/heartbeat.rs:131-140`：`end`。
  - `src/transport/heartbeat.rs:157-179`：`beat`。
- 現象：
  - CI 的 macOS job 曾有一次卡在這個測試，直到 job 逾時（30 分鐘）。
  - 沒有任何有用的輸出：libtest 會攔截測試的輸出，到測試結束才印，而卡住的測試永遠不會結束。
- 程式碼分析：可能讓它無限等待的地方，每一處都對應一個要驗證的假設。
  1. 假設 1：回聲 server 的 accept thread 是 detached thread，對 `accept`、`read_exact`、`start::write`、`try_clone` 都 `unwrap()`：

     ```rust
     // src/transport/mod.rs:802-811
     std::thread::spawn(move || {
         for stream in listener.incoming() {
             let mut stream = stream.unwrap();
             let mut first = [0u8; 6];
             stream.read_exact(&mut first).unwrap();
             assert_eq!(first[0], STRAIGHT);
             start::write(&mut stream).unwrap();
             let mut reader = stream.try_clone().unwrap();
             std::thread::spawn(move || std::io::copy(&mut reader, &mut stream));
         }
     });
     ```

     任何一個失敗（例如 macOS 預設很低的 fd 上限下出現 `EMFILE`），這個 thread panic 之後，client 在 `start::skip` 永遠等不到開頭標記：連線已建立，但沒有人回應。
  2. 假設 2：client 的 path reader（`read_path`）出錯後就不再讀，但 socket 仍然開著，因為還有 writer 與 `shut` 兩個 fd。於是回聲 server 寫回時被塞住、不再讀，client 的 path writer 也被塞住，主 thread 卡在 `sending.send()`。收資料的 thread 會在 `unwrap()` 時 panic，但主 thread 永遠到不了 `echoed.join()`，所以看不到失敗。
  3. 假設 3：`Paths::wait()` 的 `io::copy(&mut socket, sink)`（743-749）沒有逾時，要等遠端的所有 fd 都關閉。macOS 沒有 `SOCK_CLOEXEC` 與 `accept4`，std 是在 `socket()` 或 `accept()` 之後才設 `FD_CLOEXEC`。同一個測試行程中，其他測試同時 fork 出的子行程（`sh`、`cat`、`dd`，例如 `paths_can_join_through_a_shell`）可能繼承回聲 server 那一端的 socket，EOF 就要等那個子行程結束。
  4. 假設 4：`heartbeats.end()` 會 join beat thread；beat thread 只有在寫入被塞住時才不會返回。可能性低。
- 調查步驟：
  1. **在 macOS 上大量重跑**，分成三種條件：單獨跑、與整個 lib 的測試一起跑、限制 fd 上限。macOS 預設沒有 `timeout`，用 perl 的 `alarm` 當看門狗：

     ```sh
     cargo test --lib --no-run
     T=$(ls -t target/debug/deps/ferry-* | grep -v '\.d$' | head -n 1)
     w() { perl -e 'alarm shift; exec @ARGV' 120 "$@"; }
     for i in $(seq 1 2000); do w "$T" transport::tests::messages_arrive_whole_and_in_order_until_the_end --exact --nocapture -q || break; done
     for i in $(seq 1 200); do w "$T" -q --test-threads 8 || break; done
     (ulimit -n 256; for i in $(seq 1 200); do w "$T" -q || break; done)
     ```

     `--nocapture` 讓 spawned thread 的 panic 訊息即時出現。看門狗觸發（以 SIGALRM 結束，status 142）就代表重現了；這時改成不加看門狗再跑，以便收集證據。
  2. **卡住時先收集證據，不要先 kill**：

     ```sh
     P=$(pgrep -n -f "$T")
     sample "$P" 5 -file /tmp/ferry-hang-sample.txt     # 每個 thread 的 stack
     lsof -p "$P" > /tmp/ferry-hang-lsof.txt            # fd、socket 與其對端
     ps -o pid,ppid,command -g "$(ps -o pgid= -p "$P")"  # 同群組中殘留的子行程
     lsof -U > /tmp/ferry-hang-unix.txt                 # 哪些行程持有測試的 Unix socket
     ```

  3. **依證據分辨假設**：
     - 主 thread 停在 `start::skip`（`fill_buf`、`read`），且 stderr 有 `panicked at src/transport/mod.rs:80x`：假設 1。看 panic 的 errno；若是 `EMFILE`，查 `launchctl limit maxfiles` 與 `ulimit -n`。
     - 主 thread 停在 `SyncSender::send`，且已經沒有 `path reader` thread：假設 2。在 `read_path` 的 `break Err` 處暫時加 `eprintln!`，看是什麼錯誤。
     - 主 thread 停在 `Paths::wait` 的 `io::copy`，且 `lsof -U` 顯示另一個行程（`sh`、`cat`）持有同一個 socket：假設 3。從那個子行程的 command line 確認它屬於哪個測試。
     - 停在 `Heartbeats::end` 的 `join`：假設 4。
  4. **在 CI 加上卡住時自動收集**（臨時或長期）：
     - macOS 的 test job 把 `make test` 放到背景執行。
     - 超過例如 20 分鐘仍未結束時，對 `ferry-*` 測試行程執行上面的 `sample`、`lsof`、`ps`，以 `actions/upload-artifact` 上傳，再結束它。
     - 這是 CI 變更，與 CLI/CI 審查協調。
  5. **讓測試本身不會無限等待**（無論原因是什麼都該做）：
     - 回聲 server 的錯誤改經 `mpsc` channel 回報給測試 thread，不要在 detached thread 中 `unwrap()`。
     - `echoes` 加一個看門狗 thread：記錄目前進行到哪一步（例如以 `AtomicU8` 標記 skip、send、join、wait）；超過 60 秒就印出這一步並 `std::process::abort()`，讓 CI 留下 crash report 與 stack。
     - 收資料出錯時，讓主 thread 能停止送：例如收資料的 thread 出錯時呼叫 `drain()`，或關閉 path 的 socket，就像 `relay::pass` 對 `from.drain()` 的處理。
  6. **確認是否只是測試的問題**：
     - 假設 1 與 2 只存在於測試：產品中的 relay 在 `recv` 出錯時會 `drain()`（`relay.rs:168-173`），server 端也有對應的處理。
     - 假設 3 在產品中也可能發生：client 在一個 thread 開 socket 的同時，可能在另一個 thread spawn ssh。若證實，評估：
       - 在 macOS 上把 spawn 與建立 fd 序列化（例如共用一把鎖）。
       - 或在 `unblock_in_child` 的 `pre_exec` 中關閉 3 以上的所有 fd。`close` 是 async-signal-safe 的；Ferry 已經用了 `pre_exec`，所以 std 走的是 fork 加 exec。
- 要加的測試：上面第 5 步的看門狗與錯誤回報本身。根本原因確認後，再依原因補上對應的測試。
- 驗收條件：
  - [ ] 以證據（`sample`、`lsof` 的輸出）確認根本原因，並寫進對應的修正 commit。
  - [ ] 這個測試在任何錯誤下都會在 60 秒內失敗並給出訊息，不會無限等待。
  - [ ] 在 macOS 上連續 2000 次單獨執行、200 次整套執行都通過。
- 文件要同步的地方：無。若決定在 CI 加上卡住時的自動收集，在 `.github/workflows/ci.yml` 的註解中說明。
- 相關項目：TRN-09（close-on-exec）、TRN-17、TRN-18。
- **2026-10-21 補記（WP-15b）**：第 5 步大部分在 2026-10-18 已做（7af2527、f5c3ab0）：回聲 server 的錯誤印出而不 panic，收資料出錯時 `drain()`，各步驟以 `eprintln!` 標記進度，測試在 `test_support::within` 中以 60 秒為限，CI 以 nextest 逐一設時限，所以卡住時會以名稱失敗，不再卡住 job。這次補上的是證據：`within` 逾時時先呼叫新的 `test_support::say_where_threads_are`，在 macOS 上對自己的行程執行 `sample <pid> 2` 與 `lsof -p <pid>`，在 Linux 上印出每條 thread 的名稱、狀態與 `wchan`，再以名稱失敗。下次在 CI 重現時，nextest 會把這些輸出連同失敗一起印出，可以依第 3 步分辨假設。測試：`a_body_that_takes_too_long_fails_by_name`（100 ms 的上限，Linux 上印出了睡著的 thread 停在 `hrtimer_nanosleep`）。沒有做：第 1 到 3 步（這裡沒有 macOS）、第 4 步（CI 背景執行並上傳證據；上面的做法讓它不再需要）、第 6 步（假設 3 在產品中的評估，要先有證據）。掃過 2026-10-19 之後所有失敗的 CI run，這個測試沒有再失敗過；最近一次是 2026-10-18 的 39134198528。驗收條件的「以證據確認根本原因」與「macOS 上 2000 次」都還沒有達成。

---

## 較小的項目

以下各項都是低嚴重度，寫得較短；每項仍可單獨處理。

### TRN-12：`hold()` 在 ssh 持續存活卻沒有 master 時無限輪詢
- 嚴重度：低。驗證狀態：推測。
- 位置：`src/transport/mod.rs:356-383`。
- 說明：
  - `hold()` 每 100 ms 以 `ssh -O check` 檢查 master，直到成功或 holder 結束，沒有總時限。
  - 若 holder 已登入、一直存活，卻沒有 master 回應 `-O check`，Ferry 就永遠停在「connecting to host」。例如 `FERRY_SSH` 指向一個忽略 `ControlPath` 的包裝程式，或使用者的 `ControlMaster ask` 被拒。
- 修法：
  - holder 存活、但 `-O check` 持續失敗超過例如 30 秒（`ConnectTimeout` 之後再寬限），就結束 holder 並報錯。
  - 訊息說明：ssh 已登入，但沒有可共用的 ControlMaster。
  - 互動式登入期間（等使用者輸入密碼）不計時：只在 holder 以 `BatchMode` 執行時計時，或在 stderr 有輸入活動時重設計時。
- 測試：`tests/remote.rs`，假 ssh 的 `-N` 一直存活，`-O check` 一直失敗，斷言 Ferry 在時限內失敗並給出訊息。

> 2026-10-20 補記：WP-10 照修法做：只有不詢問的那一次登入（`BatchMode`）計時，30 秒內 master 沒有回應就結束 holder，說「ssh logged in, but no ControlMaster answered for 30 s to share the connection」；會在終端機詢問的那一次不計時。為了測試，新增只在 debug build 生效的 `FERRY_TEST_MASTER_WAIT`（秒），比照 D12 的注入開關。測試：`tests/remote.rs` 的 `a_master_that_never_answers_is_given_up_on`。

### TRN-13：`FERRY_SSH` 不做分詞，與文件所說「如同 `RSYNC_RSH`」不同
- 嚴重度：低。驗證狀態：已核對程式碼。
- 位置：
  - `src/transport/ssh.rs:97-99`：`program()`。
  - `src/transport/ssh.rs:103-111`：`yours()` 把啟動失敗當成「沒有自己的 ControlMaster」，不報錯。
- 說明：
  - `RSYNC_RSH="ssh -p 2222"` 是常見寫法，rsync 會分詞；Ferry 把整個字串當成程式名稱，得到 `No such file or directory`。
- 修法：二選一。
  - 依 POSIX shell 的規則分詞（例如 `shell-words` crate），第一個字是程式，其餘放在 Ferry 的引數之前。
  - 或修改文件，說明它只能是單一程式的路徑。
- 文件：`docs/cli.md`「Environment」表的 `FERRY_SSH`，以及 `cli.zh-TW.md`「環境」。

> 2026-10-20 補記：WP-10 照 CLI-05 的第一個做法：`FERRY_SSH` 以空白與 tab 分詞，單引號或雙引號內的字不分開，不做其他 shell 解析；第一個字是程式，其餘接在 ssh 的引數之前，`ssh -G` 也一樣。空字串或全是空白時用 `ssh`；引號沒有關上時也用 `ssh`。測試：`a_command_is_split_into_words`、`tests/remote.rs` 的 `ferry_ssh_may_carry_options`。

### TRN-14：`-o ConnectTimeout=10` 覆蓋使用者設定的 `ConnectTimeout`
- 嚴重度：低。驗證狀態：已核對程式碼。
- 位置：`src/transport/ssh.rs:38`。
- 說明：命令列的 `-o` 優先於設定檔。經過慢速 `ProxyJump` 而在設定中寫了 `ConnectTimeout 60` 的使用者，會被改成 10 秒。
- 修法：只在 `ssh -G` 回報 `connecttimeout none` 時才加。`yours()` 已經執行 `ssh -G`，可以一併取得。
- 文件：`docs/design/architecture.md`「Connections」與 zh-TW 版本，若有提到 Ferry 加上的選項。

> 2026-10-20 補記：WP-10 照修法做：`ssh::yours` 改為 `ssh::configured`，一次 `ssh -G` 同時讀出 ControlMaster 與 `ConnectTimeout`；不是 `none` 時，Ferry 不加 `ConnectTimeout=10`。讀不到（例如 `FERRY_SSH` 的程式不支援 `-G`）時照舊加上。測試：`your_connect_timeout_is_kept`。

### TRN-15：`ferry update` 與 `install.sh` 在改名之前沒有 fsync
- 嚴重度：低。驗證狀態：已核對程式碼。
- 位置：`src/update.rs:77-99`（`fs::copy` 後直接 `fs::rename`）、`scripts/install.sh:71-74`。
- 說明：
  - 斷電時，部分檔案系統可能留下 0 位元組的 `ferry`。ext4 的 `auto_da_alloc` 會處理「以改名替換」的情況，但 XFS 等不保證。
  - Ferry 在傳輸中對 fsync 很講究，這裡卻沒有做。
- 修法：
  - `replace` 中 copy 之後 `File::open(&new)?.sync_all()`，rename 之後對目錄 fsync。
  - `install.sh` 可略過：shell 中只有整體的 `sync` 可用。
- 測試：無法直接測斷電；確認 `the_binary_is_replaced_whole` 仍通過。

### TRN-16：接收端的 early 緩衝以片段數而不是位元組數為上限
- 嚴重度：低。驗證狀態：推測，要看實際的 packet 大小分布。
- 位置：
  - `src/transport/striped.rs:35-36`：`EARLY = 256`。
  - `src/transport/striped.rs:274-284`。
  - `src/protocol.rs:18`：`MAX_PAYLOAD` 16 MiB。
- 說明：
  - 一條 path 很慢時，接收端最多保留 256 個提早到達的片段。若片段很大（理論上限 16 MiB），每個接收端最多約 4 GiB。
  - client 有兩個接收端，各為一側。
- 修法：
  - 改為位元組預算（例如 64 MiB）。
  - 保留「下一個該到的片段一定收」的規則，才不會死結。
  - 量測實際的 manifest 與 data packet 大小，決定預算。
- 測試：`striped.rs` 的 `mod tests`，一條慢 path 加上大片段，斷言 early 緩衝的位元組數不超過預算，且順序正確。
- **2026-10-21 補記（WP-17）**：照修法做，但片段數的上限 256 保留，再加上位元組預算 `EARLY_BYTES` = 64 MiB（測試中 1 MiB），兩者任一滿了，不是下一個該到的片段就等待；early 緩衝是空的時，一個片段不論多大都收，所以不會因單一大片段而停住。預算的依據：data packet 是 frame，最大約 2.25 MiB，manifest 的段最大 4 MiB（`PART_BYTES`），64 MiB 約是 28 個 frame；沒有另外量測實際的大小分布。gauge `receive` 仍以片段數計。測試：`what_comes_early_is_held_to_a_budget_of_bytes`（piece 0 被扣住，另一條 path 送 39 個 100 KB 的片段：持有的位元組不超過 1 MiB，放行後全部依序到達；拿掉位元組預算時持有 3.9 MB 而失敗）。量測：1 GiB 隨機檔案、`--ssh-connections 4`、本機與 fatcat 之間，各 3 次交錯：送往 fatcat 修正前 10.5、9.52、9.47 s，修正後 9.68、9.42、9.42 s；從 fatcat 取回 9.44、9.36、9.86 s 與 9.48、9.44、9.35 s。

### TRN-17：`Transport::connect` 中途失敗時，不結束已啟動的子行程
- 嚴重度：低。驗證狀態：已核對程式碼。
- 位置：
  - `src/transport/mod.rs:136-191`。
  - `src/transport/mod.rs:259-313`：`Link` 沒有 `Drop`，只有 `end()`。
- 說明：
  - 某一條 path 失敗（例如 sshd 不允許轉發）而 `bail!` 時，已啟動的 `ssh -W` 或 join 行程的 `Child` 被丟棄，既沒有 kill，也沒有 wait。
  - Ferry 通常隨後就結束，所以只是暫時殘留。
- 修法：
  - 錯誤路徑上對已建立的 `links` 呼叫 `Link::end()`。
  - 或為 `Link` 實作 `Drop`：kill 並 wait；`split` 時用 `ManuallyDrop` 或 `Option::take` 移出欄位，避免重複。
- 測試：`src/transport/mod.rs` 的 `mod tests`，以 `paths_can_join_through_a_shell` 的方式讓第二條 join 立即失敗，斷言第一條的子行程已被 wait。可用 pid 檔與 `kill(pid, 0)` 驗證。

### TRN-18：`Paths::wait()` 在結束 heartbeat 之後無逾時地等待
- 嚴重度：低。驗證狀態：已核對程式碼。
- 位置：
  - `src/transport/mod.rs:743-758`。
  - `src/relay.rs:118-131`：`close_within` 只用在 control connection，而且在 `paths.wait()` 之後。
- 說明：
  - `wait()` 先結束 heartbeat，再等每個 path 子行程結束。若某條 SSH connection 剛好在這時無聲地斷了，`child.wait()` 要等到 TCP 察覺，可能是好幾個小時。
  - 這正是 heartbeat 本來要防的情況，只是時間窗很短。
- 修法：
  - 以時限等待（例如 10 秒，每 50 ms `try_wait`，與 `close_within` 相同），逾時就 kill。
  - 或在所有子行程都結束之後才結束 heartbeat：`end()` 移到子行程等完之後，期間 heartbeat 仍可觸發失敗。注意這時 server 也在結束，要確認 server 的 heartbeat 結束後，client 的 `watch` 不會把正常的 EOF 當成「dropped」：`ended[index]` 已處理 END，應該沒問題。
- 測試：`src/transport/mod.rs` 的 `mod tests`，以一個不會結束的 join 行程模擬，斷言 `wait()` 在時限內返回錯誤。

### TRN-19：探測的輸出沒有加標記，啟動檔印出的 `key=` 行會被當成結果
- 嚴重度：低。驗證狀態：已核對程式碼。
- 位置：
  - `src/remote/probe.rs:68-75`：取第一個符合 `key=` 的行。
  - `src/remote/probe.rs:107-123`。
  - `src/cli.rs:640-678`：`root_kind` 與 `kind_script`；輸出在 659-668 以 `trim()` 整段比對。
- 說明：
  - server 的交握把回覆夾在開頭標記與結束標記之間，以擋掉 shell 啟動檔的輸出。探測與 `kind_script` 沒有。
  - 啟動檔印出 `arch=…`、`os=…` 之類的行時，會被誤讀。
  - `kind_script` 的輸出若前面多了任何一行，就不是 `directory`、`other`、`missing` 三者之一，`root_kind` 以 `unexpected answer` 失敗（`src/cli.rs:663-667`），傳輸無法開始。
- 修法：
  - 兩個腳本都先 `printf '\000ferry: start\000'`，最後 `printf '\000ferry: end\000'`。
  - client 以 `start::skip` 跳過標記之前的內容（並以 `say_skipped` 說一次），只解析兩個標記之間的內容。
- 測試：`probe.rs` 的 `mod tests` 中，在腳本前面加上 `echo arch=bogus`，仍解析出正確的值。

### TRN-20：安裝的上傳期間既沒有逾時，也沒有 heartbeat
- 嚴重度：低。驗證狀態：已核對程式碼。
- 位置：`src/remote/install.rs:85-101`（release）、`src/remote/install.rs:167-177`（dev）。
- 說明：
  - 準備階段還沒有 heartbeat。網路在上傳途中無聲地斷掉時，`write_all` 會一直等到 TCP 放棄，可能是好幾個小時。使用者只能按 Ctrl-C。
  - 探測與 server 交握也一樣。
- 修法：
  - 為 Ferry 自己的 master 加上 `-o ServerAliveInterval=15 -o ServerAliveCountMax=4`（`control_options`，`ssh.rs:145-152`）。master 在約 60 秒沒有回應後結束，它上面所有的 session 都收到錯誤，準備階段就不會無限等待。
  - 使用者自己的 master（`yours`）不受影響，要在文件說明。
- 文件：`docs/design/architecture.md`「Connections」與 zh-TW 版本中 Ferry 自己的 master 的選項。

### TRN-21：`keep_recent` 可能刪掉另一個 client 剛探測到、正要啟動的 build
- 嚴重度：低（罕見）。驗證狀態：已核對程式碼。
- 位置：`src/remote/install.rs:29-38`、`src/remote/probe.rs:54-57`。
- 說明：
  - 同一個 host 被 4 個以上不同版本的 client 使用時，可能發生：client A 探測時 `touch` 了它的 build；在 A 啟動 server 之前，有另外三個 build 被使用過，第四個 client 的 `keep_recent` 就會刪掉 A 的 build。
  - A 的 server 啟動失敗（找不到檔案）；下一次執行會重新安裝。
- 修法：
  - `keep_recent` 只刪除超過 3 個、且 mtime 早於例如 1 小時前的 build。
  - 或在 server 啟動失敗、且原因是檔案不存在時，重新探測並安裝一次。
- 測試：`install.rs` 的 `mod tests`，把剛 `touch` 過的 build 排在第 4 個，斷言它不被刪除。

### TRN-22：下載用的暫存檔名截掉了版本號的一部分，Ctrl-C 時也會殘留
- 嚴重度：低（外觀與清理）。驗證狀態：已核對程式碼。
- 位置：`src/remote/release.rs:135-158`（136 行的 `with_extension`）。
- 說明：
  - `ferry-0.14.4-x86_64-unknown-linux-musl` 的「副檔名」被認為是 `4-x86_64-unknown-linux-musl`，所以暫存檔叫做 `ferry-0.14.part-<pid>`。
  - Ctrl-C 時 curl 被終止，暫存檔留在 `~/.ferry/releases/<version>/`。
- 修法：
  - 以 `dir.join(format!("{name}.part-{pid}"))` 命名。
  - 以 `interrupt` 的暫存檔登記機制（`interrupt::create` 或新的路徑登記表，見 TRN-07）在中斷時刪除；或在下一次 `fetch` 時清掉同目錄中的 `*.part-*`。
- 測試：`release.rs` 的 `mod tests`，下載失敗後目錄中沒有 `*.part-*`。

### TRN-23：heartbeat 與資料共用 TCP 的送出緩衝
- 嚴重度：低。驗證狀態：推測；設計文件已承認 heartbeat 與資料共用 TCP。
- 位置：`src/transport/heartbeat.rs:30-41`；`docs/design/architecture.md`「Connections」的 heartbeat 段落。
- 說明：
  - heartbeat channel 不會排在 SSH window 後面，但會排在 TCP 送出緩衝（Linux 自動調整，最大約 4 MiB）與網路中的佇列後面。
  - 在很慢的上行鏈路上（例如每秒 64 KiB），client 送往 server 的 heartbeat 可能排隊超過 40 秒，server 因此誤判 client 已不在。
- 修法：先量測，在慢速鏈路上以 `--ssh-connections 1` 推送，觀察 dashboard 上 heartbeat 的間隔。若證實會發生：
  - 讓 server 的逾時在「這段時間內 path 上仍有資料進來」時延後。
  - 或把逾時改成相對於最近的吞吐量。
- 文件：若改變規則，更新 `architecture.md` 與 zh-TW 版本的 heartbeat 段落。
- **2026-10-21 補記（WP-17）**：先量測。用一個本機的 ProxyCommand（Python，上行限速，前面有 4 MiB 緩衝，模擬核心的送出緩衝）把 fatcat 的上行限在每秒 64 KiB 與 24 KiB，`--ssh-connections 1` 推送一個隨機檔案：64 KiB/s 時 16 MiB 成功（63.6 KiB/s，heartbeat 排在約一個 channel window 的資料後面，仍在 40 秒內）；24 KiB/s 時約 51 秒後失敗，client 只說「the Destination on fatcat-slow: a path ended before its end」。證實會發生。修法採第一種：「這段時間內 path 上仍有資料進來時延後」，做法是新增 `heartbeat::Hearing` 與包住 path reader 的 `heartbeat::Heard`，path 讀到資料時（每秒最多十次）就像收到了 heartbeat。client 的 path 與 heartbeat channel 依 SSH connection 的順序對應，所以算在那條 connection；server 上 path 與 channel 以任意順序連進來，分不出對應，所以任何 path 的資料都算在每一條 channel，反正 server 的逾時只是為了 client 已不在的情況（有一條 SSH connection 無聲中斷、其他仍在傳時，由 client 那邊以那條 connection 自己的沉默發現）。heartbeat 本身的 channel 不算資料。修正後 24 KiB/s 推送 6 MiB 成功（23.8 KiB/s，258 s，內容一致）。測試：`what_comes_on_a_path_counts_as_hearing`（兩條 channel 都沒有 heartbeat；path 的資料讓對應的那條不逾時，另一條仍逾時）。沒有自動化的慢速鏈路測試。
  - 同一個實驗另外發現：WP-14 的停滯偵測只數 server 之間經過的完整 packet，24 KiB/s 時一個 2 MiB 的 frame 要 85 秒，因而誤報「nothing has passed between the servers for 60 s」。一併修正：`relay::control` 也把每條 SSH connection 在 path 上經過的位元組（儀表板用的 `Monitor`）算作移動；heartbeat channel 不計，所以真正的停滯不會被掩蓋。修正後同一個實驗不再誤報。本機兩側沒有 `Monitor`，不受影響。這是審查清單以外的修正。
  - 沒有修：server 因 heartbeat 逾時結束時，client 只看到「a path ended before its end」，看不出原因。

### TRN-24：sudoers 只對 Ferry 的執行檔設 `NOPASSWD` 時，密碼會被送進 server 的 stdin
- 嚴重度：低。驗證狀態：推測。
- 位置：
  - `src/remote/sudo.rs:49-85`：`prepare` 以 `sudo -n true` 判斷要不要密碼。
  - `src/remote/sudo.rs:88-117`：`check` 以 `true` 驗證密碼。
  - `src/remote/sudo.rs:119-142`。
  - `src/transport/mod.rs:109-115`：`control` 先寫入 `first`。
- 說明：
  - `true` 需要密碼、Ferry 的執行檔卻是 `NOPASSWD` 時，Ferry 會問密碼並以 `sudo -k -S` 啟動 server。但 sudo 不讀密碼，於是密碼那一行成為 server 的 stdin 開頭，交握失敗，錯誤訊息令人困惑。
  - 要確認錯誤訊息不會印出這些位元組。
- 修法：
  - 以實際要執行的程式來判斷與驗證：`sudo -n -- <program> build-id`（`build-id` 見 TRN-02），以及 `sudo -k -S -p '' -- <program> build-id`。
  - 這需要先知道 program。做了 TRN-01 的 B（先為兩側解析 program）之後，`sudo_for` 移到 program 解析之後即可。
- 測試：`tests/remote.rs`，擴充 `fake_sudo`：對 `true` 要求密碼，對 Ferry 的執行檔不要求。斷言傳輸成功，且 server 的 stdin 沒有收到密碼。

> 2026-10-20 補記：WP-11 照修法做：安裝之後才準備 sudo，所以 `Sudo::prepare` 收到要執行的程式，以 `sudo -n -- <program> build-id` 判斷需不需要密碼，以 `sudo -k -S -p '' -- <program> build-id` 驗證密碼。測試：`remote_sudo_needs_no_password_where_ferry_alone_needs_none`（假 sudo 對名稱含 `ferry` 的程式不要密碼，對其他要）；`remote_sudo_runs_the_server_through_sudo_with_its_password` 改為斷言檢查跑的是 `build-id`。
