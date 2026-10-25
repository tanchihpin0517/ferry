# 每個 host 只開一個 session：啟動腳本、安裝，然後交給 host 上的 agent

- 狀態：討論中
- 開始：2026-10-25；最後修改：2026-10-25
- 基準：HEAD `53be189`。以下行號都以它為準。
- 會改動：
  - `docs/design/architecture.md` 的「Transport」：control connection 的「A byte stream to one program」與「What it is for」，以及「Where SSH will not forward to a socket, `--remote-join`…」；還有「Installing on a remote host」：安裝在第一個 session 裡做；
  - `docs/design/pipeline.md` 的「Phase 1: Prepare」第 3、4 步；
  - `docs/cli.md` 的「`ferry server`」與選項表的 `--remote-join`，並為 agent 加一節，同樣是不給人直接用的隱藏子命令；
  - 以上各份的 `.zh-TW.md` 版本。
- 相關：
  - [preparation.md](preparation.md)：已採納的準備狀態機；
  - [architecture-redesign.md](architecture-redesign.md) 的「還沒驗證」：啟動的 12 個 RTT；
  - 審查項目 TRN-04（命令列在任何登入 shell 都能跑）、TRN-19（probe 的輸出加標記），見 `docs/review/2026-10-17/done/transport.md`；
  - `0471a76`、`c9fd1e2`：nano4 上 shell 印的東西，以及 master 已在時的 `ssh -N`；
  - VS Code 的 Remote-SSH，見提案第 8 節。

這份草稿是 2026-10-25 和使用者討論的結果，還沒有任何程式碼。討論先後提過三個做法：probe 與 server 合成一個 session、第一個 session 裡留一個 sh 的迴圈、host 上的 agent。使用者選了 agent，另外兩個寫在[其他做法](#其他做法)。

---

## 問題

### 每做一件事就開一個 session

Ferry 在一個 host 上每做一件事，就經那台 host 的 SSH 連線開一個 session（`Transport::command`，`src/transport/mod.rs:112`）。每個 session 都要 sshd 建立，再由使用者的登入 shell 跑完啟動檔，才執行 Ferry 的指令。

一次傳輸，每個遠端 host 開的 session 如下。2026-10-25 讀程式碼，並以一個記錄每次 ssh 的假 ssh 在本機跑一次 `--remote-join` 的 copy 核對：記到的正是 probe、server 與兩個 `server --join`。

| 情況 | 經登入 shell 的 session |
|---|---|
| 預設（直連） | 2：probe（`src/prepare/probe.rs:162`）與 server（`src/prepare/start.rs:78`）。control 與 stream 走 `ssh -W`，直接連到 server 的 socket，不經 shell |
| 兩邊在同一個 host | 3：probe 1、server 2 |
| `--remote-sudo` | 再多 1–2：查要不要密碼、驗證密碼，各跑一次 `sudo … build-id`（`src/remote/sudo.rs:69`、`:109`） |
| `--remote-join` | 4：control 與 stream 再各一個 `ferry server --join`（`src/prepare/start.rs:131-139`） |
| 要安裝 | 再加上傳的 session（`src/prepare/install.rs`） |

`ferry test` 另外每個 `ferry fixture` 一個。rsync 每次傳輸只開一個（`ssh host rsync --server …`）。

### 在 nano4 上，一個 session 要好幾秒

2026-10-25 A2 的收回驗收時，在 nano4 的 login node（25a-lgn04，負載 430–470）上量的：

- 經 master 跑一次 `ssh nano4 true`：10–16 秒。
- `bash -c true`，不讀 `.bashrc`：0.03 秒，負載高峰時有一次 3.4 秒。
- `bash -c 'source ~/.bashrc'`：1.6–3.7 秒。使用者的 `.bashrc` 在每個 shell 都跑一次 `myenv sync --background --check-interval --update`，還有 conda 的 hook。
- 剩下的 7–13 秒沒有拆開。當時推測是 sshd 在那個負載下建立 session，沒有驗證。
- `ferry test` 每個情境 5–25 秒，fatcat 上一兩秒。從 nano4 複製 1 GiB 的混合樹要 13–26 秒（含啟動），ssh 本身兩個方向都有 99–117 MB/s。

### 登入 shell 換不掉

sshd 一律以使用者在那台 host 上的登入 shell 執行收到的指令，例如 `bash -c '<指令>'`，client 沒辦法指定別的 shell。Ferry 送的指令本來就是 `sh -c '…'`（`src/remote/shell.rs:28` 的 `sh_command`，讓 csh、tcsh、fish 當登入 shell 時也能跑），但 bash 在執行它之前，已經讀完啟動檔。本機的 bash 5.2.21 上驗證：

- 像 sshd 那樣帶著 `SSH_CLIENT` 跑 `bash -c "sh -c '…'"`：先讀 `~/.bashrc`，再跑 sh。沒有 `SSH_CLIENT` 時不讀。
- 環境裡已經有 `SHLVL` 時也不讀。
- 讀 `.bashrc` 的時候，`$BASH_EXECUTION_STRING` 已經是收到的那行指令。

所以每個 session 都要付一次啟動檔。只有不開 session 的路，也就是 `ssh -W` 到 host 上一個 socket 的 channel，不經 shell。

### 啟動檔會往 session 的輸出寫

啟動檔可能在 session 的標準輸出印東西，也可能留下之後才印的背景程序：nano4 上，前一個背景的 myenv 還拿著鎖時，新的 shell 會印一行 WARN。現在的處理：probe 與 server 的回答夾在記號之間（`src/transport/start.rs`），一次寫出，client 跳過記號以外的東西並說出來；server 握手之後，control 移到 `ssh -W` 的 channel（`src/transport/mod.rs:686-691`），那裡沒有別人寫得進來。

---

## 提案

一個 host 只開一個 session：跑一段啟動腳本，需要時在同一個 session 裡安裝，然後 exec 成 Ferry 的 agent。之後在那台 host 上要做的事，都由 client 經 `ssh -W` 連到 agent，由 agent 替它開程式。這和 VS Code 的 Remote-SSH 是同一個結構（第 8 節）。

### 1. 啟動腳本

- 第一個 session 的指令，仍是一行的 `sh -c '…'`。腳本先做現在 probe 做的事：machine ID、OS、架構、這個 build 裝了沒有、有沒有 cargo、每個 root 是什麼。結果加框寫出（第 4 節）。
- 接著以 `read` 讀 client 的一行指示。`read` 只讀到行尾，dash 與 bash 都是（本機驗證），所以之後的資料不會被 shell 吃掉。client 照現在的順序，所有 host 都 probe 完、該問的都問完才回答；這段期間沒有計時器。
- 指示有三種：
  - **agent**：build 已經裝好，或 `--remote-ferry` 指定了程式。exec 成 agent。
  - **安裝 release**：從標準輸入收 binary，讀到輸入結束（`cat > "$tmp"`），檢查大小與 SHA-256、執行它說出 build，再改名就位，和現在一樣。然後 exec 成 agent。
  - **安裝 dev build**：從標準輸入收原始碼的 tar，讀到輸入結束（`tar -x -f -`），以 host 的 cargo 建置，放好，和現在一樣。然後 exec 成 agent。
- client 不做了（例如使用者不同意安裝，或另一個 host 失敗），就關掉輸入，腳本讀到結束就離開。
- 上傳放在標準輸入的最後、讀到結束：現在的安裝 session 就是這樣，收完繼續跑（`src/prepare/install.rs:266`、`:388`）。所以不需要「剛好讀 N 個位元組」：那在 macOS 與 BusyBox 上做不到（見其他做法）。

### 2. agent

- Ferry 的一個新模式，名稱暫定，像 `ferry server` 一樣是不給人直接用的隱藏子命令。
- 啟動時在 0700 的目錄裡開一個 unix socket，和現在 server 讓 stream 與 control 加入的 socket 同一個目錄（`/tmp/ferry-<uid>/join/`，Linux 上是 `$XDG_RUNTIME_DIR/ferry/join/`），只有使用者自己連得到。socket 的路徑加框寫在 session 的標準輸出。
- client 先開一條 `ssh -W` 到這個 socket，當 agent 的 control。agent 活到這條連線結束：標準輸入可能已經被上傳用掉，所以不看它。
- 之後每一件事，client 都開一條 `ssh -W` 到 agent 的 socket，第一則訊息說要跑什麼。agent fork 出那個程式，把這條連線當成它的標準輸入輸出。agent 不轉送任何位元組。
- 子行程怎麼結束，agent 在 control 上報回，因為 client 看不到它的結束碼；現在是 ssh 的結束碼直接告訴 client。子行程的 stderr 照舊到 client 的終端機。sudo 的檢查要讀的 stderr，agent 收下來，連同結束碼一起報回。
- agent 只跑固定的幾種：server（可以經 sudo）、`build-id`（sudo 的檢查）、`ferry fixture`。不跑任意的指令。
- client 不再需要它時，例如兩邊的 server 都握手之後，就關掉 control，agent 隨即結束。它啟動的 server 各自活到自己的 control 連線結束，不靠 agent。
- agent 沒有計時器。

### 3. server

- 由 agent 啟動時，server 的標準輸入輸出就是那條 `-W` 連線，沒有 shell 會往裡面寫。所以它直接在上面握手，握手之後也不必再把 control 移到另一條 channel（現在的 `attach`）。「server 活到它的輸入結束」照舊成立。
- 經 sudo：agent 跑 `sudo -k -S -p '' -- ferry server --sudo`，client 先在連線上寫密碼那一行，和現在一樣。sudo 一次讀一個位元組，不會讀到後面的 hello。
- stream 不變：client 把 socket pair 的一端，經 `ssh -W` 交到 server 自己的 socket。

### 4. 只剩第一個 session 的輸出要加框

第一個 session 的標準輸出也在啟動檔留下的背景程序手上，而 probe 的結果、agent 的 socket 路徑，以及第 5 節 TCP 的 port 與秘密，會寫在那裡。那些程式只拿著寫的那一端，讀不到這些。agent 的 control 與它開的連線都是乾淨的 `-W`，不必加框。

- **冒充不了。** 每個回應裝在一個框裡：client 這次執行隨機選的標記、內容的長度、內容，最後是帶同一個標記的結束記號。別的程式不知道這個標記，它們印的東西只會落在框外，client 跳過並說出來，和現在一樣（`say_skipped`，`src/transport/mod.rs:600`）。
- **插不進去。** 每個框一次寫出，而且夠小：pipe 保證一次寫入在 Linux 4096、macOS 512 位元組以內，不和別人的寫入交錯。probe 的結果只有幾百位元組。本機驗證 dash 的 `printf` 把 3 KB 的回應一次寫出；bash 以 stdio 寫，沒有驗證到。
- **還是被插進去時。** 例如 sshd 用的是 socket pair，保證比 pipe 弱：長度與標記對不上，client 清楚地失敗，不會讀錯。

### 5. 不肯轉接到 unix socket 的 host

現在，sshd 不肯把 channel 轉到 server 的 socket 時，準備就失敗，訊息裡提到 `--remote-join`。使用者下了它，每條 channel 才改成一個 session，跑 `ferry server --join`，把自己的標準輸入輸出經 unix socket 交給 server。join 的 channel 經過使用者的 shell，啟動檔留下的程式可能寫進 stream，所以不在沒問的情況下做（`docs/design/architecture.md:411-421`）。

提案依序試三種，前兩種自動：

1. **`ssh -W <socket>`**：和現在一樣。
2. **`ssh -W 127.0.0.1:<port>`**：agent 與 server 另外在 loopback 上聽一個 TCP port，由 kernel 挑。這和第 1 種是同一種 channel，只是終點換成 TCP：不經 shell、不開 session，啟動檔留下的程式也寫不進來，所以可以在第 1 種被拒時自動接著試。
   - loopback 的 port，那台 host 上每個使用者都連得到；unix socket 靠 0700 的目錄擋人，TCP 沒有這一層。所以每條連線先送出這次執行隨機產生的秘密，listener 核對過才讀寫其他東西，不對就關掉。秘密和 socket 的路徑一起，裝在第一個 session 的框裡交給 client。別的使用者連得進來，但送不出秘密，讀不到也寫不進任何東西。VS Code 預設就是 loopback 的 TCP 加一把隨機的 key。
   - 一切仍走那條 SSH 連線，不是 host 之間直接的 TCP。
3. **join**：照現在，要使用者下 `--remote-join`。連到 agent 的連線也改成跑 `ferry server --join` 的 session，把標準輸入輸出交給 agent，其餘不變。現在 control 與 stream 都已經分成直接與經 join 兩種（`Transport::channel`、`Transport::half`，`src/transport/mod.rs:181`、`:241`），連到 agent 的連線用同一套。

各種 host 會走哪一種：

| host 的設定 | 現在 | 提案 |
|---|---|---|
| 只有 `AllowTcpForwarding no`，最常見的那種限制 | 直連照常：兩個選項是分開的 | 第 1 種 |
| `AllowStreamLocalForwarding no`，但准 TCP | 要 `--remote-join` | 第 2 種，自動 |
| OpenSSH 6.7 以前，沒有 unix socket 的轉接 | 要 `--remote-join` | 第 2 種，自動 |
| 什麼都不准轉接：`DisableForwarding yes`，或金鑰在 `authorized_keys` 裡被限制不准轉接 | 要 `--remote-join` | 第 2 種也被拒，仍要 join |

最後一種 host 上 rsync 照樣能跑，它只需要一個 session，所以 join 得留著，拿掉它，Ferry 就會在 rsync 能跑的 host 上失敗。每種設定有多常見，沒有資料；nano4 與 fatcat 都准 unix socket 的轉接。

agent 要在啟動時就知道開不開 TCP：連 agent 的那條 `-W <socket>` 若被拒，client 就沒辦法再叫它開。做法見待決定的問題。

### 6. 省下的

| 情況 | 現在 | agent |
|---|---|---|
| 預設 | 2 | 1 |
| 兩邊在同一個 host | 3 | 1 |
| `--remote-sudo` | 3–4 | 1 |
| 要安裝 | 2 加上傳 | 1 |
| 不准 unix socket、准 TCP（現在要 `--remote-join`） | 4 | 1 |
| 什麼都不准轉接（`--remote-join`） | 4 | 4 |
| `ferry test` 的 fixture | 每個 1 | 0：整個 `ferry test` 每個 host 一個 agent |

- 在 nano4 上，每少一個 session，就少一整個 session 的時間，sshd 那段與 `.bashrc` 都在內。上面量到的是 10–16 秒，各部分沒有拆開。
- 預設的情況多一條 `-W`，也就是 agent 的 control：現在 2 條（server 的 control 與 stream），之後 3 條。
- 一條 `-W` 應該只要一個往返，sshd 替 client 連那個 socket，不跑 PAM、也不跑 shell，但沒有量過。整個提案的好處都靠這一點，所以計畫的第 0 步先量它。

### 7. 不變的

- 準備的順序：每個 host 先 probe，問完、裝好、sudo 準備好，才啟動 server。agent 不是 server，所以「every host has this build before either server starts」不必改。
- 一個 host 一條 SSH 連線；stream 由 client 建立，把兩端交給 server，不經手資料；連線的健康只在一處判斷。
- 第一個 session 的指令仍是一行的 `sh -c '…'`，任何登入 shell 都能跑（TRN-04）。
- 安裝的內容：裝在哪裡、檢查什麼、問不問，都和現在一樣，只是改在第一個 session 裡做。
- 本機的 server 不經 SSH，沒有 session，也沒有 agent。

### 8. 和 VS Code 的 Remote-SSH 比較

2026-10-25 讀了它的文件與 wiki（[Remote-SSH Troubleshooting](https://github.com/microsoft/vscode-remote-release/wiki/Remote-SSH-Troubleshooting)、[Remote Development Tips and Tricks](https://code.visualstudio.com/docs/remote/troubleshooting)、[Remote Development FAQ](https://code.visualstudio.com/docs/remote/faq)）：

- 一樣的：
  - 經 ssh 跑一段腳本，安裝、啟動或找到已經在跑的 server；
  - 之後全部走 SSH 的轉接；
  - 正逐步變成預設的 exec server，先啟動一個很小的 control server，再由它啟動完整的 server。
- 不一樣的，以及這份提案為什麼不照做：
  - **server 從哪來。** VS Code 預設由 host 自己下載，失敗才在本機下載、再傳過去。這裡照現在的做法，由 client 上傳：有了 agent，上傳放在第一個 session 的最後就夠了。host 自己下載寫在其他做法。
  - **常駐。** VS Code 的連線會去找已經在跑的 server 來重用。這裡的 agent 每次執行一個，用完就結束。VS Code 的文件自己寫了常駐的代價：環境變數改了要重啟 server，另有「Kill VS Code Server on Host」來清掉它。
  - **怎麼連。** VS Code 預設聽 host 上 localhost 的一個隨機 TCP port，每條連線帶一把隨機的 key，另可改用只有使用者連得到的 unix socket。這裡一律用後者，Ferry 的 server 本來就這樣。
  - **每條連線落在同一台。** VS Code 的文件提到，叢集若每次把連線分到不同的節點，安裝的連線與轉接的連線可能落在不同台，要使用者自己用 ControlMaster。Ferry 一個 host 只有一條 SSH 連線，所有東西本來就在同一台。
- 沒查證到的：VS Code 的腳本在 host 上到底怎麼跑（wiki 說是 pipe 進 ssh，故障排除頁說寫到 `/tmp` 再執行），以及本機下載之後怎麼傳過去。

---

## 其他做法

- **probe 與啟動 server 合成一個 session。** build 已裝好時，probe 腳本印完結果就 exec server。預設的情況一樣省一個，但 server 會在別的 host 問安裝、裝好之前就啟動，要改「every host has this build before either server starts」；sudo 不好合；要安裝、兩邊同一 host 時都省不到。
- **第一個 session 裡留一個 sh 的迴圈。** 登入 shell 之後 exec 成一個寫在命令列上的 `sh` 迴圈，每次 `read` 一行請求、執行、把加框的回應寫回；probe、sudo 的檢查、最後 exec 成 server，都依序送進去。不用 Ferry，Ferry 還沒裝的 host 也能用。但一次只能做一件事：兩邊同一 host 的第二個 server、`ferry fixture` 都還要另開 session；安裝的上傳若要接在請求後面，得「剛好讀 N 個位元組」：
  - GNU `head -c` 可以（本機驗證，3 MiB 的二進位資料完全相同，迴圈之後照常讀下一行），但 macOS 與 BusyBox 的 `head` 用 stdio 讀，應該會多讀（沒有試）；
  - `dd bs=1 count=N` 到處都剛好，但每個位元組一次 read、一次 write，5 MB 約一千萬次系統呼叫（推算，沒有量）；
  - 迴圈也不能是從標準輸入讀的腳本（`sh -s`）：本機的 dash 0.5.12 從 pipe 讀腳本時一次讀一大塊，把接在請求後面的資料當成指令執行。
  - agent 涵蓋它，而且之後的連線都是乾淨的。
- **host 自己下載 release，失敗再上傳（VS Code 的預設）。** 少傳約 5 MB（v0.16.0 的 binary 是 4.2–5.5 MB），client 的上行慢時有用。但要 host 連得到 GitHub、有 `sha256sum` 或 `shasum` 驗證 client 送去的雜湊、要設逾時並退回上傳，多出新的失敗方式；dev build 也用不上。可以之後當成改進選項。
- **跨次執行留著 agent。** 連第一個 session 都省：client 先直接 `ssh -W` 到記住的 socket，連不上才開 session，比 VS Code 每次跑腳本去找還省。但 nano4 一個名稱後面有好幾台 login node，master 換到別台時，舊的 agent 連不到；login node 上長駐的行程可能被站方清掉；agent 帶著啟動當時的環境，`.bashrc` 或 module 改了之後就舊了；不同 build 要各一個；閒置多久結束要有規則。先不做。
- **使用者的 `.bashrc` 對 Ferry 先 return。** Ferry 在每個遠端指令裡放一個固定的字，例如當成 sh 的 `$0`，任何登入 shell 都照樣能跑；不能用 `VAR=1 cmd` 的寫法，csh 不認。文件給一行，讓想要的人加在 `.bashrc` 最前面：`case $BASH_EXECUTION_STRING in *<那個字>*) return ;; esac`。剩下的那個 session 省掉 `.bashrc` 的時間，nano4 上是 1.6–3.7 秒。和提案互補，opt-in。
  - 要先確認 nano4 的 bash 讀啟動檔時，也已設好 `$BASH_EXECUTION_STRING`。
  - Ferry 不靠 `.bashrc`：它以絕對路徑執行自己的 binary，probe 只用標準的工具。例外是在 host 上建 dev build：Ferry 先找 `~/.cargo/bin`，cargo 只靠 `.bashrc` 才在 PATH 上的，會找不到。
- **Ferry 送 `SHLVL` 過去。** bash 看到就不讀 `.bashrc`（本機驗證），但 host 的 sshd 要以 `AcceptEnv` 收這個變數，預設只收 `LANG`、`LC_*`。不做。
- **client 記住上次的 probe，這次跳過。** nano4 的每台 login node 有自己的 machine ID，build 的路徑跟著不同；root 的種類每次也不同。不做。
- **給第一個 session 一條獨立的輸出：以 `ssh -R` 讓 host 連回 client。** 只有 bash 的 `/dev/tcp` 連得出去，sh 與 dash 不行；要 host 允許 remote TCP forwarding；而且會在 login node 的 localhost 開一個 port，那台機器上的任何使用者都連得到，還得再加驗證。不做；加框就夠了。

---

## 待決定的問題

- [ ] **做不做。** 先看第 0 步量到的 `ssh -W` 有多便宜。使用者決定。
- [ ] **agent 跑哪幾種。** 提案是 server（可以經 sudo）、sudo 的檢查、`ferry fixture`。以後要不要讓別的事也走它，例如續傳時查 host 上的暫存檔，到時再說。
- [ ] **TCP 的退路要不要排進第一版。** 只對「不准 unix socket、准 TCP」與 OpenSSH 6.7 以前的 host 有用；手上有沒有這種 host，決定它排在哪裡。使用者決定。
- [ ] **agent 怎麼知道要不要開 TCP。** 每次都一起開：多一個只在 loopback 上、沒有秘密就用不了的 port。或 client 先對一個不存在的路徑試一條 `-W`：sshd 回「不准」還是「連不上」，分得出准不准轉接，但這要讀 ssh 印出的訊息，各版的字樣可能不同。
- [ ] **host 自己下載 release，要不要列為改進選項。**
- [ ] **「`.bashrc` 對 Ferry 先 return」要不要一起做。**

---

## 計畫

每一步結束時，都要能執行、測試過。

0. **量測。** 決定做之前：nano4（使用者先開好 ControlMaster）與 fatcat，各量開一條 `ssh -W` 與 `ssh host true` 要多久，並把現在一次啟動的時間拆開，每個 session、握手、到第一個 block。`-W` 若不便宜，這份提案要重想。
1. **啟動腳本與 agent，build 已裝好的情況。** probe 加框、讀指示、exec 成 agent；server 經 agent 啟動、在連線上握手。假的 ssh 要能做 `-W`，例如以 Ferry 自己 join 的方式，把標準輸入輸出交給 socket，不靠 python。測試：
   - 以記錄每次 ssh 的假 ssh 數 session：預設 1、兩邊同一 host 1；
   - 假的 ssh 在每個指令前後印東西，並留下之後才印的背景程序；
   - agent 隨 control 結束；server 啟動不了時，client 說出它怎麼結束。
2. **安裝放進第一個 session。** 上傳放在最後、讀到輸入結束；release 與 dev build 都是。測試：要安裝時 session 數 1。
3. **`--remote-sudo` 經 agent。** 兩個檢查與經 sudo 的 server。測試：session 數 1；密碼打錯，照舊再問，最多三次。
4. **不肯轉接 unix socket 的 host，與 `ferry test`。** TCP 的退路：秘密、被拒時自動改試；最後才是 join，連到 agent 的連線也改走它；`ferry fixture` 經 agent。測試：假的 ssh 拒絕 `-W <socket>` 時，自動改走 TCP，session 數 1；秘密不對的連線進不去；假的 ssh 要能做 `-W host:port`。
5. **驗收與文件。** 本機、fatcat、nano4 的 `ferry test`；nano4 上的啟動時間對照第 0 步；「會改動」列的文件，以及 CHANGELOG。
