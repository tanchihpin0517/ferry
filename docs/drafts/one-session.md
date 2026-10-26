# 每個 host 只開一個 session：啟動腳本、安裝，然後交給 host 上的 launcher

- 狀態：已採納（2026-10-25）；第 0 至 5 步都已完成（2026-10-26）：與第 0 步的 build 輪流跑，1 KB 的複製，nano4 上從 2.00 秒變成 1.22 秒，fatcat 上卻從 0.18 秒變成 0.20 秒（中位數）；見[進度](#進度)
- 開始：2026-10-25；最後修改：2026-10-26
- 基準：HEAD `53be189`。以下行號都以它為準。
- 會改動：
  - `docs/design/architecture.md` 的「Transport」：control connection 的「A byte stream to one program」與「What it is for」，以及「Where SSH will not forward to a socket, `--remote-join`…」；還有「Installing on a remote host」：安裝在第一個 session 裡做；
  - `docs/design/pipeline.md` 的「Phase 1: Prepare」第 3、4 步；
  - `docs/cli.md` 的「`ferry server`」與選項表的 `--remote-join`，並為 launcher 加一節，同樣是不給人直接用的隱藏子命令；
  - 以上各份的 `.zh-TW.md` 版本。
- 相關：
  - [preparation.md](preparation.md)：已採納的準備狀態機；
  - [architecture-redesign.md](architecture-redesign.md) 的「還沒驗證」：啟動的 12 個 RTT；
  - 審查項目 TRN-04（命令列在任何登入 shell 都能跑）、TRN-19（probe 的輸出加標記），見 `docs/review/2026-10-17/done/transport.md`；
  - `0471a76`、`c9fd1e2`：nano4 上 shell 印的東西，以及 master 已在時的 `ssh -N`；
  - VS Code 的 Remote-SSH，見提案第 8 節。

這份草稿是 2026-10-25 和使用者討論的結果；程式碼做到哪裡，見[進度](#進度)。討論先後提過三個做法：probe 與 server 合成一個 session、第一個 session 裡留一個 sh 的迴圈、host 上的 launcher。使用者選了 launcher，另外兩個寫在[其他做法](#其他做法)。launcher 原本叫 agent，2026-10-25 使用者改名：它做的是啟動。

---

## 流程總覽

照提案做時，一次傳輸的準備，每一步由誰做、和現在有什麼不同。細節見[提案](#提案)。

### launcher 之前：client 與啟動腳本

| 步驟 | 誰做 | 現在 |
|---|---|---|
| 1. 讀參數、檢查位置 | client | 一樣 |
| 2. 登入每個 host（`ssh -G`、`-O check`，沒有 master 時 `ssh -N`）；密碼、OTP 在這裡打 | client，不開 session | 一樣 |
| 3. probe：machine ID、OS、架構、這個 build 裝了沒有、有沒有 cargo、每個 root 是什麼；build 已裝好時，順便清掉舊的 build | 啟動腳本，在第一個 session（提案第 1 節） | 自己開一個 session |
| 4. 決定 Destination 的根、問要不要安裝 | client | 一樣 |
| 5. 安裝：release 預設由 host 自己下載，失敗才上傳；dev build 上傳原始碼。上傳都放在最後、讀到結束 | 啟動腳本 | 另開 session，一律上傳 |
| 6. exec 成 launcher，開 socket（需要時也開 TCP 與秘密），把位置加框交給 client | 啟動腳本 → launcher（第 2、4、5 節） | 沒有這一步 |

### launcher 做的

每件事一條 `ssh -W` 連到 launcher，launcher fork 出程式，把那條連線交給它。

| 步驟 | 誰做 | 現在 |
|---|---|---|
| 7. launcher 的 control；launcher 活到這條連線結束 | launcher | 沒有 |
| 8. `--remote-sudo` 的兩個檢查：要不要密碼、密碼對不對，最多問三次 | launcher 跑 `sudo … build-id`；在終端機上問密碼的是 client | 各開一個 session |
| 9. 啟動每個 server，可以經 sudo；server 直接在那條連線上握手（第 3 節） | launcher fork 出 server | 開一個 session，握手後再把 control 搬到 `-W` |
| 10. `ferry fixture`，只有 `ferry test` 用 | launcher | 每個開一個 session |

### launcher 之後，或從不經過 launcher 的

| 步驟 | 誰做 | 現在 |
|---|---|---|
| 11. 核對兩個 root（一個包住另一個就拒絕），叫 server 開始 | client，經 server 自己的 control | 一樣 |
| 12. 建 stream，把兩端交到各 server 自己的 socket | client，`ssh -W`；被拒時改 TCP，最後才是 join（第 5 節） | 一樣，只是沒有 TCP 那一步 |
| 13. 關掉 launcher 的 control，launcher 結束；server 各自活到自己的 control 結束 | client | 沒有 |

- **順序。** 所有 host 先做完第 3 步，才進第 4 步。每個 host 裝好就 exec 成 launcher 等著，但第 8、9 步要等每個 host 的 launcher 都起來才做，所以「每個 host 都裝好之後才啟動 server」不變。
- **launcher 不轉送資料。** 它把連線交給 server 之後，server 和 client 直接在那條連線上溝通。
- **本機那一邊。** 沒有啟動腳本，也沒有 launcher，client 照舊直接啟動本機的 server。

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

一個 host 只開一個 session：跑一段啟動腳本，需要時在同一個 session 裡安裝，然後 exec 成 Ferry 的 launcher。之後在那台 host 上要做的事，都由 client 經 `ssh -W` 連到 launcher，由 launcher 替它開程式。這和 VS Code 的 Remote-SSH 是同一個結構（第 8 節）。

### 1. 啟動腳本

- 第一個 session 的指令，仍是一行的 `sh -c '…'`。腳本先做現在 probe 做的事：machine ID、OS、架構、這個 build 裝了沒有、有沒有 cargo、每個 root 是什麼；另外說 host 上有沒有 `curl` 或 `wget`、`sha256sum` 或 `shasum`，好決定能不能自己下載。結果加框寫出（第 4 節）。
- 接著以 `read` 讀 client 的一行指示。`read` 只讀到行尾，dash 與 bash 都是（本機驗證），所以之後的資料不會被 shell 吃掉。client 照現在的順序，所有 host 都 probe 完、該問的都問完才回答；這段期間沒有計時器。
- 指示有四種：
  - **launcher**：build 已經裝好，或 `--remote-ferry` 指定了程式。exec 成 launcher。
  - **下載 release**：release build 預設這樣裝（2026-10-25 使用者決定），和 VS Code 一樣。host 以 `curl`，沒有就以 `wget`，從 GitHub Releases 抓這個版本給它平台的 binary（`<repository>/releases/download/v<version>/ferry-<version>-<target>`，`src/remote/release.rs`），以 client 送去的 SHA-256 核對，執行它說出 build，再改名就位，然後 exec 成 launcher。
    - 雜湊從這個版本的 `SHA256SUMS` 來：client 先抓它，很小，和現在一樣存在本機。所以 host 抓到的，和現在 client 自己下載時核對的是同一份清單。
    - host 沒有 `curl` 與 `wget`，或沒有 `sha256sum` 與 `shasum` 時，不試，直接上傳：沒辦法核對的，不裝。
    - 下載失敗，例如連不出去、要 proxy、太慢：腳本說出原因，回去等下一行指示，client 改送「上傳 release」，在同一個 session 裡退回上傳。連線最多等 10 秒，每秒不到 1 KB 持續 30 秒就放棄。連不出去的 host，每次安裝因此多等最多約 10 秒；安裝每個 build、每台 host 只有一次。
    - compute node 常常沒有對外網路，所以上傳必須留著，Ferry 也就不要求 host 能連外。
  - **上傳 release**：下載不了時。client 下載 binary、核對、存在本機，和現在一樣；host 從標準輸入收它，讀到輸入結束（`cat > "$tmp"`），檢查大小與 SHA-256、執行它說出 build，再改名就位。然後 exec 成 launcher。
  - **安裝 dev build**：從標準輸入收原始碼的 tar，讀到輸入結束（`tar -x -f -`），以 host 的 cargo 建置，放好，和現在一樣。然後 exec 成 launcher。
- client 不做了（例如使用者不同意安裝，或另一個 host 失敗），就關掉輸入，腳本讀到結束就離開。
- 上傳放在標準輸入的最後、讀到結束：現在的安裝 session 就是這樣，收完繼續跑（`src/prepare/install.rs:266`、`:388`）。所以不需要「剛好讀 N 個位元組」：那在 macOS 與 BusyBox 上做不到（見其他做法）。

### 2. launcher

- Ferry 的一個新模式，名稱暫定，像 `ferry server` 一樣是不給人直接用的隱藏子命令。
- 啟動時在 0700 的目錄裡開一個 unix socket，和現在 server 讓 stream 與 control 加入的 socket 同一個目錄（`/tmp/ferry-<uid>/join/`，Linux 上是 `$XDG_RUNTIME_DIR/ferry/join/`），只有使用者自己連得到。socket 的路徑加框寫在 session 的標準輸出。
- client 先開一條 `ssh -W` 到這個 socket，當 launcher 的 control。launcher 活到這條連線結束：標準輸入可能已經被上傳用掉，所以不看它。
- 之後每一件事，client 都開一條 `ssh -W` 到 launcher 的 socket，第一則訊息說要跑什麼。launcher fork 出那個程式，把這條連線當成它的標準輸入輸出。launcher 不轉送任何位元組。
- 子行程怎麼結束，launcher 在 control 上報回，因為 client 看不到它的結束碼；現在是 ssh 的結束碼直接告訴 client。子行程的 stderr 照舊到 client 的終端機。sudo 的檢查要讀的 stderr，launcher 收下來，連同結束碼一起報回。
- launcher 只跑固定的幾種：server（可以經 sudo）、`build-id`（sudo 的檢查）、`ferry fixture`。不跑任意的指令。
- client 不再需要它時，例如兩邊的 server 都握手之後，就關掉 control，launcher 隨即結束。它啟動的 server 各自活到自己的 control 連線結束，不靠 launcher。
- launcher 沒有計時器。

### 3. server

- 由 launcher 啟動時，server 的標準輸入輸出就是那條 `-W` 連線，沒有 shell 會往裡面寫。所以它直接在上面握手，握手之後也不必再把 control 移到另一條 channel（現在的 `attach`）。「server 活到它的輸入結束」照舊成立。
- 經 sudo：launcher 跑 `sudo -k -S -p '' -- ferry server --sudo`，client 先在連線上寫密碼那一行，和現在一樣。sudo 一次讀一個位元組，不會讀到後面的 hello。
- stream 不變：client 把 socket pair 的一端，經 `ssh -W` 交到 server 自己的 socket。

### 4. 只剩第一個 session 的輸出要加框

第一個 session 的標準輸出也在啟動檔留下的背景程序手上，而 probe 的結果、launcher 的 socket 路徑，以及第 5 節 TCP 的 port 與秘密，會寫在那裡。那些程式只拿著寫的那一端，讀不到這些。launcher 的 control 與它開的連線都是乾淨的 `-W`，不必加框。

**逐行加框。** 每一行輸出各自是一個框，一段輸出以一個 `done` 框收尾。下面是 nano4 上第一個 session 的標準輸出，從頭到尾，夾著啟動檔印的東西；probe 省略了說下載工具的兩行。`␀` 是一個 NUL 位元組，現在的記號也用它；每個框以換行結尾；箭頭之後是說明，不是輸出。

```
WARN myenv: …                                         ← .bashrc 印的（示意），在 Ferry 的指令之前
␀ferry:7c3e9a51d2b04f86:probe:43:machine_id=3a9f2c7e1b8d4f6a9c0e2d4b6f8a1c3e
␀ferry:7c3e9a51d2b04f86:probe:8:os=Linux
␀ferry:7c3e9a51d2b04f86:probe:11:arch=x86_64
myenv: …                                              ← 背景程式稍後印的（示意），落在兩行之間
␀ferry:7c3e9a51d2b04f86:probe:70:host_dir=/home/tanch0517/.ferry/hosts/3a9f2c7e1b8d4f6a9c0e2d4b6f8a1c3e
␀ferry:7c3e9a51d2b04f86:probe:13:installed=yes
␀ferry:7c3e9a51d2b04f86:probe:13:any_build=yes
␀ferry:7c3e9a51d2b04f86:probe:8:cargo=no
␀ferry:7c3e9a51d2b04f86:probe:14:root=directory
␀ferry:7c3e9a51d2b04f86:done:1:8
␀ferry:7c3e9a51d2b04f86:launcher:48:socket=/run/user/40123/ferry/join/launcher-5d0c1e9a
␀ferry:7c3e9a51d2b04f86:done:1:1
```

- 一個框：`␀ferry:<標記>:<種類>:<長度>:<內容>`，再一個換行。種類是 `probe`、`launcher` 或 `done`；長度是內容的位元組數。
- probe 的每一行，是現在 probe 印的一個 `key=value`。launcher 的框是 socket 的路徑，開了 TCP 時再多兩行，`tcp=127.0.0.1:<port>` 與 `secret=<64 個十六進位字元>`。
- `done` 收尾一段輸出，內容是這一段送了幾行。沒有它，client 分不出行都到了，還是腳本中途死了。某個框若沒能一次寫出、雜訊插進了它的開頭，client 會把那一行當成雜訊跳過，`done` 的行數抓得到少了一行。
- client 找帶這次標記的開頭，讀出種類與長度，照長度讀內容，接著的位元組必須是換行。框外的東西收起來，像現在一樣說一次（`say_skipped`，`src/transport/mod.rs:600`）。

啟動腳本以一個小函式，每一行一個 `printf`，算好一行就寫一行，不必先把整段收進一個變數；launcher 以同樣的格式寫：

```sh
LC_ALL=C; export LC_ALL      # ${#2} 才是位元組數；UTF-8 下 bash 算的是字元數
n=0
say() { printf '\000ferry:%s:%s:%d:%s\n' "$tag" "$1" "${#2}" "$2"; n=$((n + 1)); }
say probe "os=$(uname -s)"
…
printf '\000ferry:%s:done:%d:%s\n' "$tag" "${#n}" "$n"
```

本機的 dash 0.5.12 與 bash 5.2 都照這樣印出正確的框；內容含 `é` 的那一行，長度算的是位元組。

**標記。**

- 每次執行由 client 從 `/dev/urandom` 重新選，16 個十六進位字元，和 server 的 join token 同一個來源（`src/transport/join.rs:226`），寫進啟動腳本裡。
- 由 client 選，不由 host 產生：client 讀輸出之前就得知道它；由 host 產生，就得先用某種方式告訴 client，而那種方式本身也可能被冒充。
- 一次執行一個就夠：各段輸出靠種類與先後分得開，每段、每行各用一個標記多不了什麼。
- 不是秘密：它在啟動腳本的命令列裡，那台 host 上的其他使用者以 `ps` 看得到。它只防意外：使用者自己的程式碰巧印出像框的東西，或印在框的中間。其他使用者沒有拿著這個 session 的標準輸出，寫不進來；以使用者的身分跑的程式本來就什麼都能做，所以這不是防攻擊。
- TCP 的秘密不同：launcher 自己從 `/dev/urandom` 產生，只出現在 launcher 那一段的框裡，從不在任何命令列上。

**插不進去。** 每個框一次寫出，而且很小：pipe 保證一次寫入在 Linux 4096、macOS 512 位元組以內，不和別人的寫入交錯。上面每個框 33–104 位元組，整段 probe 483 位元組，整段一個框時是 249；只有單獨一行的內容超過約 470 位元組時，才會超過 macOS 的 512。本機驗證 dash 的 `printf` 把 3 KB 一次寫出；bash 以 stdio 寫，沒有驗證到。

**還是被插進去時。** 例如 sshd 用的是 socket pair，保證比 pipe 弱，或某一行特別長：雜訊插進內容，長度之後就不是換行；插進開頭，那一行被當成雜訊，`done` 的行數就對不上。兩種情形 client 都報「something cut into the answer」，不會讀錯。

### 5. 不肯轉接到 unix socket 的 host

現在，sshd 不肯把 channel 轉到 server 的 socket 時，準備就失敗，訊息裡提到 `--remote-join`。使用者下了它，每條 channel 才改成一個 session，跑 `ferry server --join`，把自己的標準輸入輸出經 unix socket 交給 server。join 的 channel 經過使用者的 shell，啟動檔留下的程式可能寫進 stream，所以不在沒問的情況下做（`docs/design/architecture.md:411-421`）。

提案依序試三種，前兩種自動：

1. **`ssh -W <socket>`**：和現在一樣。
2. **`ssh -W 127.0.0.1:<port>`**：launcher 與 server 另外在 loopback 上聽一個 TCP port，由 kernel 挑。這和第 1 種是同一種 channel，只是終點換成 TCP：不經 shell、不開 session，啟動檔留下的程式也寫不進來，所以可以在第 1 種被拒時自動接著試。
   - loopback 的 port，那台 host 上每個使用者都連得到；unix socket 靠 0700 的目錄擋人，TCP 沒有這一層。所以每條連線先送出這次執行隨機產生的秘密，listener 核對過才讀寫其他東西，不對就關掉。秘密和 socket 的路徑一起，裝在第一個 session 的框裡交給 client。別的使用者連得進來，但送不出秘密，讀不到也寫不進任何東西。VS Code 預設就是 loopback 的 TCP 加一把隨機的 key。
   - 一切仍走那條 SSH 連線，不是 host 之間直接的 TCP。
3. **join**：照現在，要使用者下 `--remote-join`。連到 launcher 的連線也改成跑 `ferry server --join` 的 session，把標準輸入輸出交給 launcher，其餘不變。現在 control 與 stream 都已經分成直接與經 join 兩種（`Transport::channel`、`Transport::half`，`src/transport/mod.rs:181`、`:241`），連到 launcher 的連線用同一套。

各種 host 會走哪一種：

| host 的設定 | 現在 | 提案 |
|---|---|---|
| 只有 `AllowTcpForwarding no`，最常見的那種限制 | 直連照常：兩個選項是分開的 | 第 1 種 |
| `AllowStreamLocalForwarding no`，但准 TCP | 要 `--remote-join` | 第 2 種，自動 |
| OpenSSH 6.7 以前，沒有 unix socket 的轉接 | 要 `--remote-join` | 第 2 種，自動 |
| 什麼都不准轉接：`DisableForwarding yes`，或金鑰在 `authorized_keys` 裡被限制不准轉接 | 要 `--remote-join` | 第 2 種也被拒，仍要 join |

最後一種 host 上 rsync 照樣能跑，它只需要一個 session，所以 join 得留著，拿掉它，Ferry 就會在 rsync 能跑的 host 上失敗。每種設定有多常見，沒有資料；nano4 與 fatcat 都准 unix socket 的轉接。

**開哪一種、關哪一種（2026-10-25 使用者決定）。**

- launcher 啟動時兩種都開：client 這時還不知道哪一種會通。連 launcher 的那條 `-W <socket>` 若被拒，client 就沒辦法再叫它開；事先探，又得讀 ssh 印出的訊息。
- 第一條真的是 client 的連線，決定用哪一種，另一種馬上關掉，unix socket 連檔案一起刪掉：
  - 從 unix socket 來的，不論是直接的 `-W`，或 join 的程式在 host 上連進來，都算數：那個目錄只有使用者自己進得去。這時關掉 TCP。
  - 從 TCP 來的，要先交出正確的秘密才算數，算數了就關掉 unix socket。沒交對秘密的連線直接斷掉，不算數：否則同一台 host 上的別人搶先連上 TCP，就能讓 launcher 關掉 unix socket。
- server 只開用得到的那一種：它由 launcher 啟動時，client 已經知道哪一種會通，在啟動的請求裡告訴它。走 join 時，stream 由 host 上的 join 程式去連 unix socket，所以 server 只開 unix socket。
- 兩種都開著的時間，只有 launcher 啟動到 client 第一條連線進來之間，約一個往返；那段時間裡，TCP 沒有秘密也用不了。

### 6. 省下的

| 情況 | 現在 | launcher |
|---|---|---|
| 預設 | 2 | 1 |
| 兩邊在同一個 host | 3 | 1 |
| `--remote-sudo` | 3–4 | 1 |
| 要安裝 | 2 加上傳 | 1 |
| 不准 unix socket、准 TCP（現在要 `--remote-join`） | 4 | 1 |
| 什麼都不准轉接（`--remote-join`） | 4 | 4 |
| `ferry test` 的 fixture | 每個 1 | 0：整個 `ferry test` 每個 host 一個 launcher |

- 在 nano4 上，每少一個 session，就少一整個 session 的時間，sshd 那段與 `.bashrc` 都在內。上面量到的是 10–16 秒，各部分沒有拆開。
- 預設的情況多一條 `-W`，也就是 launcher 的 control：現在 2 條（server 的 control 與 stream），之後 3 條。
- 一條 `-W` 應該只要一個往返，sshd 替 client 連那個 socket，不跑 PAM、也不跑 shell，但沒有量過。整個提案的好處都靠這一點，所以計畫的第 0 步先量它。

### 7. 不變的

- 準備的順序：每個 host 先 probe，問完、裝好、sudo 準備好，才啟動 server。launcher 不是 server，所以「every host has this build before either server starts」不必改。
- 一個 host 一條 SSH 連線；stream 由 client 建立，把兩端交給 server，不經手資料；連線的健康只在一處判斷。
- 第一個 session 的指令仍是一行的 `sh -c '…'`，任何登入 shell 都能跑（TRN-04）。
- 安裝的內容：裝在哪裡、檢查什麼、問不問，都和現在一樣，只是改在第一個 session 裡做。
- 本機的 server 不經 SSH，沒有 session，也沒有 launcher。

### 8. 和 VS Code 的 Remote-SSH 比較

2026-10-25 讀了它的文件與 wiki（[Remote-SSH Troubleshooting](https://github.com/microsoft/vscode-remote-release/wiki/Remote-SSH-Troubleshooting)、[Remote Development Tips and Tricks](https://code.visualstudio.com/docs/remote/troubleshooting)、[Remote Development FAQ](https://code.visualstudio.com/docs/remote/faq)）：

- 一樣的：
  - 經 ssh 跑一段腳本，安裝、啟動或找到已經在跑的 server；
  - 之後全部走 SSH 的轉接；
  - 安裝時預設由 host 自己下載，失敗才由 client 傳過去；VS Code 退回時在本機下載再傳，這裡也是；
  - 正逐步變成預設的 exec server，先啟動一個很小的 control server，再由它啟動完整的 server。
- 不一樣的，以及這份提案為什麼不照做：
  - **常駐。** VS Code 的連線會去找已經在跑的 server 來重用。這裡的 launcher 每次執行一個，用完就結束。VS Code 的文件自己寫了常駐的代價：環境變數改了要重啟 server，另有「Kill VS Code Server on Host」來清掉它。
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
  - launcher 涵蓋它，而且之後的連線都是乾淨的。
- **安裝時一律由 client 上傳，不讓 host 自己下載。** 原本的提案：有了 launcher，上傳放在第一個 session 的最後就夠了；host 自己下載只少傳約 5 MB（v0.16.0 的 binary 是 4.2–5.5 MB），卻要 host 連得到 GitHub、有 `sha256sum` 或 `shasum`、要設逾時並退回上傳，多出新的失敗方式，dev build 也用不上。2026-10-25 使用者決定照 VS Code，預設由 host 自己下載，見提案第 1 節。
- **跨次執行留著 launcher。** 連第一個 session 都省：client 先直接 `ssh -W` 到記住的 socket，連不上才開 session，比 VS Code 每次跑腳本去找還省。但 nano4 一個名稱後面有好幾台 login node，master 換到別台時，舊的 launcher 連不到；login node 上長駐的行程可能被站方清掉；launcher 帶著啟動當時的環境，`.bashrc` 或 module 改了之後就舊了；不同 build 要各一個；閒置多久結束要有規則。先不做。
- **使用者的 `.bashrc` 對 Ferry 先 return。** 2026-10-25 使用者決定：不做。原本的想法：Ferry 在每個遠端指令裡放一個固定的字，例如當成 sh 的 `$0`，任何登入 shell 都照樣能跑；不能用 `VAR=1 cmd` 的寫法，csh 不認。文件給一行，讓想要的人加在 `.bashrc` 最前面：`case $BASH_EXECUTION_STRING in *<那個字>*) return ;; esac`。剩下的那個 session 省掉 `.bashrc` 的時間，nano4 上是 1.6–3.7 秒。和提案互補，opt-in。
  - 要先確認 nano4 的 bash 讀啟動檔時，也已設好 `$BASH_EXECUTION_STRING`。
  - Ferry 不靠 `.bashrc`：它以絕對路徑執行自己的 binary，probe 只用標準的工具。例外是在 host 上建 dev build：Ferry 先找 `~/.cargo/bin`，cargo 只靠 `.bashrc` 才在 PATH 上的，會找不到。
- **Ferry 送 `SHLVL` 過去。** bash 看到就不讀 `.bashrc`（本機驗證），但 host 的 sshd 要以 `AcceptEnv` 收這個變數，預設只收 `LANG`、`LC_*`。不做。
- **client 記住上次的 probe，這次跳過。** nano4 的每台 login node 有自己的 machine ID，build 的路徑跟著不同；root 的種類每次也不同。不做。
- **給第一個 session 一條獨立的輸出：以 `ssh -R` 讓 host 連回 client。** 只有 bash 的 `/dev/tcp` 連得出去，sh 與 dash 不行；要 host 允許 remote TCP forwarding；而且會在 login node 的 localhost 開一個 port，那台機器上的任何使用者都連得到，還得再加驗證。不做；加框就夠了。

---

## 待決定的問題

- [x] **做不做。** 先看第 0 步量到的 `ssh -W` 有多便宜：nano4 上 session 約是 `-W` 的 50 倍（見計畫第 0 步），所以照做。2026-10-25 使用者決定：先量；結果支持的話，草稿標成已採納，設計文件先加 Proposed 段落，然後全部步驟做完再一起報告。
- [x] **名稱。** 原本叫 agent；2026-10-25 使用者定為 launcher，因為它做的是啟動。
- [x] **launcher 跑哪幾種。** server（可以經 sudo）、sudo 的檢查、`ferry fixture`，都是啟動。使用者以名稱定下它只做啟動：以後若要在 host 上查東西，例如續傳時查暫存檔，另外設計，不放進 launcher。
- [x] **TCP 的退路要不要排進第一版。** 只對「不准 unix socket、准 TCP」與 OpenSSH 6.7 以前的 host 有用。2026-10-25 使用者決定：放進第一版。
- [x] **launcher 怎麼知道要不要開 TCP。** 2026-10-25 使用者決定：每次都一起開，多一個只在 loopback 上、沒有秘密就用不了的 port。另一個做法是 client 先對一個不存在的路徑試一條 `-W`，看 sshd 回「不准」還是「連不上」；但這要讀 ssh 印出的訊息，各版的字樣可能不同。
- [x] **host 自己下載 release。** 2026-10-25 使用者決定：release build 預設這樣裝，失敗才上傳（提案第 1 節）。
- [x] **「`.bashrc` 對 Ferry 先 return」要不要一起做。** 2026-10-25 使用者決定：不做。
- [x] **session 便宜的 host 上，一次多的約 0.02 秒。** 驗收時量到：fatcat 上一個 session 只要 20–40 ms，launcher 的收尾多出約 10 ms（client 關掉追蹤它的連線，launcher 結束之後，第一個 session 才關），複製 1 KB 的中位數從 0.18 秒變成 0.20 秒，見[進度](#進度)。其他做法：提早關掉追蹤 launcher 的連線，和 stream 的收尾同時；或收尾時不等第一個 session 結束，但 ssh 可能比 ferry 晚結束，launcher 最後說的話也收不到。2026-10-26 使用者決定：接受，不改。

---

## 計畫

每一步結束時，都要能執行、測試過。

0. **量測。** 決定做之前：nano4（使用者先開好 ControlMaster）與 fatcat，各量開一條 `ssh -W` 與 `ssh host true` 要多久，並把現在一次啟動的時間拆開，每個 session、握手、到第一個 block。`-W` 若不便宜，這份提案要重想。
   - fatcat（2026-10-25，負載 17–20，56 核；我自己開的 master，四種輪流各 10 輪）：session（`ssh fatcat true`）中位數 20.1 ms，最小 14.5；`-W` 連到一個 listener 13.4 ms，最小 11.2；`-W` 連到不存在的 socket 9.6 ms；只在本機的 `ssh -O check` 7.3 ms。`ferry copy` 一個 1 KB 的檔案，第一次要在 fatcat 上建 dev build，43.8 秒；之後 5 次各 0.54–0.69 秒，probe 的 session 28–38 ms，stream 在 0.10–0.12 秒時建好。fatcat 上 session 便宜，launcher 一次約省 30–40 ms。
   - nano4（2026-10-25，login node 25a-lgn05，負載 18–23，216 核；使用者的 master，四種輪流各 10 輪）：session 中位數 1,864 ms，最小 1,748，最大 2,303；`-W` 連到一個 listener 37.5 ms，35.8–39.2；`-W` 連到不存在的 socket 19.8 ms；`ssh -O check` 6.9 ms。session 約是 `-W` 的 50 倍。
   - 同一台上，`bash -c 'source ~/.bashrc'` 1.82–2.20 秒，`bash -c true` 0.03–0.06 秒：負載低時，一個 session 的時間幾乎都是使用者的 `.bashrc`。驗收時負載 430–470 的那台（25a-lgn04），session 10–16 秒、`.bashrc` 1.6–3.7 秒，那時大部分不在 `.bashrc`。
   - `ferry copy` 一個 1 KB 的檔案到 nano4：第一次在 login node 上建 dev build，56.3 秒（cargo 48.8 秒）；之後 5 次各 3.97–4.58 秒。probe 的 session 1.71–2.12 秒；接著 server 的 session，到握手完成約 2 秒；control 與 stream 的 `-W` 在 3.75–4.36 秒時打開；傳輸本身 0.1–0.2 秒。
   - 推算 launcher 之後：一個 session 約 2 秒，加上幾條各約 40 ms 的 `-W`，一次約 2.3 秒，少了約一半。負載高時，兩個各 10–16 秒的 session 剩一個。「`.bashrc` 對 Ferry 先 return」可以讓負載低時剩下的那個 session 也只要約 0.1 秒，但 2026-10-25 決定不做。
   - 結論：`-W` 比 session 便宜得多，提案照做。
   - 量測的小程式在 `.scratch/ms/`，沒有進 git。
1. **啟動腳本與 launcher，build 已裝好的情況。** probe 加框、讀指示、exec 成 launcher；server 經 launcher 啟動、在連線上握手。假的 ssh 要能做 `-W`，例如以 Ferry 自己 join 的方式，把標準輸入輸出交給 socket，不靠 python。測試：
   - 以記錄每次 ssh 的假 ssh 數 session：預設 1、兩邊同一 host 1；
   - 假的 ssh 在每個指令前後印東西，並留下之後才印的背景程序；
   - launcher 隨 control 結束；server 啟動不了時，client 說出它怎麼結束。
2. **安裝放進第一個 session。** release 預設由 host 自己下載，失敗才上傳；dev build 上傳原始碼；上傳放在最後、讀到輸入結束。測試：要安裝時 session 數 1；下載失敗時，在同一個 session 裡改上傳；下載下來的雜湊不符，不裝。
3. **`--remote-sudo` 經 launcher。** 兩個檢查與經 sudo 的 server。測試：session 數 1；密碼打錯，照舊再問，最多三次。
4. **不肯轉接 unix socket 的 host，與 `ferry test`。** TCP 的退路：秘密、被拒時自動改試；最後才是 join，連到 launcher 的連線也改走它；`ferry fixture` 經 launcher。測試：假的 ssh 拒絕 `-W <socket>` 時，自動改走 TCP，session 數 1；秘密不對的連線進不去；假的 ssh 要能做 `-W host:port`。
5. **驗收與文件。** 本機、fatcat、nano4 的 `ferry test`；nano4 上的啟動時間對照第 0 步；「會改動」列的文件，以及 CHANGELOG。

---

## 進度

2026-10-25 記錄，2026-10-26 補上 nano4 的驗收。第 1 至 4 步照計畫的順序做完，第 4 步分成兩個 commit；第 5 步做了本機、fatcat 與 nano4 的驗收、文件與 CHANGELOG，驗收找到的兩個問題都修了。

| 步驟 | commit |
|---|---|
| 1. 啟動腳本與 launcher | `ca2967d` |
| 2. 安裝放進第一個 session | `0cf3459` |
| 3. `--remote-sudo` 經 launcher | `10a1f31` |
| 4. TCP 的退路與 `ferry test` | `86f4e0b`（TCP 的退路）、`46bad1a`（`ferry fixture` 經 launcher） |
| 5. 驗收與文件 | `edbe767`、`7d4c3b0`（驗收找到的問題）、`a39e45d`（文件）、本次的驗收記錄 |

與計畫的差異：

- **第 1 步：假的 ssh 做 `-W`，用的是只有 debug build 才有的隱藏子命令 `ferry relay`**，不是計畫舉例的「以 Ferry 自己 join 的方式」：join 把標準輸入輸出交給 socket，`-W` 則是在兩頭之間搬位元組，launcher 的連線要的是後者。它照 `ssh` 的方式讀寫，不用 `io::copy`：`io::copy` 會 splice，卡住管道，讀的一方等不到已經到的東西。
- **第 1 步：launcher 也看著自己的標準輸出**，即第一個 session 的輸出，由 client 握著：它一關，launcher 就結束。這樣 client 在追蹤 launcher 之前就不在時，例如在那之前失敗，launcher 不會一直留在 host 上。計畫只寫了「launcher 隨 control 結束」。
- **第 1 步：launcher 回報結束時，也帶上跑到結束的程式在標準錯誤上說的話**，最多 64 KiB：第 3 步 sudo 說密碼不對、第 4 步 fixture 失敗的原因，都靠它讓 client 顯示。server 的標準錯誤照舊和 launcher 的在同一處。
- **第 2 步：「下載失敗時，在同一個 session 裡改上傳」只在腳本層測過**：下載的腳本失敗時說出原因、session 照樣接著讀下一個指令；上傳的腳本另外測。整段流程只有 release build 走得到，測試裡沒有 release build；真的 host 上，也要等帶 launcher 的 release 出來才驗得到。
- **第 3 步：「密碼打錯，照舊再問，最多三次」沒有新測試**：再問的迴圈沒有動，而它要終端機才測得到。有測的是 `FERRY_SUDO_PASSWORD` 給錯時什麼都不跑，sudo 說的「Authentication failed」經 launcher 回到 client。
- **第 4 步：`--remote-join` 時，一個 host 仍是 4 個 session**：第一個 session、追蹤 launcher 的、server 的 control connection、stream 各一個，和以前的 probe、server、control connection 的 channel、stream 一樣多：經過 shell 加入的每條連線都要一個 session。計畫沒有提到這點。
- **第 5 步：驗收找到一個錯，`edbe767` 修了。** 從 fatcat 到本機、加上 `--remote-join` 的 `ferry test`，一開始就失敗：「a channel ended before it started」。追蹤 launcher 的 thread 原本以 `let _ = held;` 握著加入的行程交來的東西，但在 Rust 2021 的 closure 裡，這行不會捕捉 `held`，它在 thread 一開始就被丟掉：加入的行程隨即結束，sshd 關掉它那個 session 的輸入，也就是 launcher 被追蹤的那條連線，launcher 在 client 開下一條連線之前就停了。改成 `let _held = held;`；新的測試用一個在命令結束時關掉 session 輸入的假 ssh，沒有這個修正就失敗。同一次也發現 `ferry test` 的「server 啟動不了」情境找的是「did not start」，launcher 起不來時已經不這樣說了；現在說「Ferry's launcher on host did not start: its session ended (exit status: 127)」。
- **第 5 步：驗收又找到一個問題，`7d4c3b0` 修了：fatcat 上新的 build 反而比較慢。** 把第 0 步的 build 與新的輪流跑，1 KB 的複製，第 0 步的 build 0.16–0.19 秒，新的 0.21–0.23 秒。兩者的 channel 打開與關掉的時間一樣，但現在活到最後的第一個 session，在最後一條 channel 關掉之後，中位數還要 23 ms 才結束，以前 server 的 session 和 channel 一起結束：launcher 等追蹤它的連線關掉才結束，而 client 每 20 ms 才看一次 session 結束了沒，server 的 channel 與 stream 的也是。先試了每 1 ms 看一次：session 在最後一條 channel 之後 7 ms 結束，複製 0.18–0.20 秒。現在先隔 1 ms 再看，之後每次加倍，最多 20 ms，期限照舊 5 秒；session 在最後一條 channel 之後 10 ms 結束，複製 0.18–0.21 秒，仍比第 0 步的 build 慢約 0.02 秒，見下面的驗收。
- **第 5 步：計時改成把第 0 步的 build 與新的輪流跑**，不是拿第 0 步記下的數字比：nano4 這次一個 session 約 1 秒，第 0 步時約 1.9 秒；fatcat 上，第 0 步的 build 這次只要 0.16–0.19 秒，第 0 步記下的是 0.54–0.69 秒。不同時候量的數字不能直接比。第 0 步的 build 是把 `7ecae79` 另外放在 `.scratch/` 建的，build 是 `dev-d518bebd611e16dedeec`，和第 0 步用的相同；nano4 上它還裝著，fatcat 上重裝了。
- **第 5 步：更正 `a39e45d` 寫的 fatcat 結論。** 那時寫「0.54–0.69 秒變成 0.22–0.24 秒，以前結束前約 0.4 秒的等待不見了」，是拿不同時候的數字比的。同時輪流跑，第 0 步的 build 也只要 0.16–0.19 秒，那 0.4 秒不是 launcher 省下的；第 0 步時為什麼慢，沒有查。那時的新 build 其實比第 0 步的慢約 0.04 秒，即上面 `7d4c3b0` 修的。
- **第 5 步：connection 的圖也重畫了**（`docs/assets/architecture-connections.svg`），「會改動」沒有列到它。

第 5 步的驗收：

- 本機（`7d4c3b0`，dev build）：完整測試 554 個通過；fmt、clippy、`make fuzz-build` 通過；`ferry test` 本機到本機 36 個通過、2 個略過（要 host 的情境）。`make deny` 在 `a39e45d` 時通過，之後沒有改依賴，沒有重跑。
- fatcat（2026-10-26，負載 13–17，56 核）：`ferry test` 從本機到 fatcat、fatcat 到 fatcat、從 fatcat 到本機加 `--remote-join`，各 39 個全過，`7d4c3b0` 分別 8、9、7 秒（`a39e45d` 時 10、11、8 秒）；之後 fatcat 上沒有留下 launcher 或 server。
- nano4（2026-10-26，login node 25a-lgn05，負載 10–19，216 核；使用者的 master）：`ferry test` 從本機到 nano4、nano4 到 nano4、從 nano4 到本機加 `--remote-join`，各 39 個全過，`7d4c3b0` 分別 55、53、175 秒（`a39e45d` 時 47、51、142 秒，負載約 10，這次 11–14；`ferry test` 沒有輪流跑，這些秒數不能拿來比較兩個 build。A2 收回的驗收時，在負載 430–470 的另一台上，每個情境要 5–25 秒）；之後 nano4 上沒有留下 launcher 或 server。每個 dev build 都由 Ferry 自己在第一個 session 裡上傳原始碼、在 nano4 上建好，例如 `a39e45d` 的那次，整個複製 15.7 秒，其中 cargo 13.7 秒，只重編 ferry 本身。
- 一次啟動的時間：以記錄每次 ssh 的假 ssh，1 KB 的 `ferry copy`，第 0 步的 build 與新的輪流跑，新的 build 先跑一次裝好：

  | | 第 0 步的 build | 新的 build |
  |---|---|---|
  | nano4，`7d4c3b0`，各 10 次 | 1.96–2.04 秒，中位數 2.00 | 1.19–1.28 秒，中位數 1.22 |
  | nano4，`a39e45d`，各 6 次 | 1.86–1.94 秒 | 1.10–1.17 秒 |
  | fatcat，`7d4c3b0`，各 10 次 | 0.17–0.19 秒，有一次 0.26，中位數 0.18 | 0.18–0.21 秒，有一次 0.23，中位數 0.20 |
  | fatcat，`a39e45d`，各 10 次 | 0.16–0.19 秒，中位數 0.18 | 0.21–0.23 秒，中位數 0.22 |

  - 每次都是第 0 步的 build 2 個 session，新的 1 個。
  - nano4 上少掉的正好是第二個 session：第 0 步的 build，probe 的 session 約 1 秒結束，接著是 server 的 session，channel 在 1.85–1.92 秒打開；新的 build，channel 在 1.05–1.14 秒打開。之後的傳輸與結束，兩者都約 0.1 秒。
  - fatcat 上一個 session 只要 20–40 ms，新的 build 在結尾多的那一段就抵掉了它：channel 打開的時間兩者一樣，中位數約 0.09 秒；最後一個 ssh 結束，新的晚約 15 ms，其中約 10 ms 是 client 關掉追蹤 launcher 的連線、launcher 結束之後，第一個 session 才關。第 0 步推算 fatcat 上一次約省 30–40 ms，實際上慢約 20 ms。
  - 第 0 步記下的 nano4 3.97–4.58 秒，這次也重現不出來，原因如上：這次一個 session 只要第 0 步時的一半。
