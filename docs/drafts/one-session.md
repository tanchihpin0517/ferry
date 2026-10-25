# 每個 host 只開一個 session：準備的步驟都送進同一個 shell

- 狀態：討論中
- 開始：2026-10-25；最後修改：2026-10-25
- 基準：HEAD `53be189`。以下行號都以它為準。
- 會改動：
  - `docs/design/architecture.md` 的「Transport」：control connection 的「What it is for」，準備時依序開好幾條 control connection；
  - `docs/design/pipeline.md` 的「Phase 1: Prepare」第 3、4 步；
  - 以上兩份的 `.zh-TW.md` 版本。
- 相關：
  - [preparation.md](preparation.md)：已採納的準備狀態機；
  - [architecture-redesign.md](architecture-redesign.md) 的「還沒驗證」：啟動的 12 個 RTT；
  - 審查項目 TRN-04（命令列在任何登入 shell 都能跑）、TRN-19（probe 的輸出加標記），見 `docs/review/2026-10-17/done/transport.md`；
  - `0471a76`、`c9fd1e2`：nano4 上 shell 印的東西，以及 master 已在時的 `ssh -N`。

這份草稿是 2026-10-25 和使用者討論的結果，還沒有任何程式碼。

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

### 啟動檔會往 session 的輸出寫

啟動檔可能在 session 的標準輸出印東西，也可能留下之後才印的背景程序：nano4 上，前一個背景的 myenv 還拿著鎖時，新的 shell 會印一行 WARN。現在的處理：probe 與 server 的回答夾在記號之間（`src/transport/start.rs`），一次寫出，client 跳過記號以外的東西並說出來；server 握手之後，control 移到 `ssh -W` 的 channel（`src/transport/mod.rs:686-691`），那裡沒有別人寫得進來。

---

## 提案

### 1. 一個 host 一個 session，準備的步驟依序送進同一個 sh

- 第一個 session 的指令，等登入 shell 跑完啟動檔之後，`exec sh -c '<迴圈>'`。迴圈寫在命令列上，每次都一樣：以 `read` 讀一行請求，執行它，把回應寫回（第 2 節）。
- 從此每一步都不再由 sshd 建 session，也不再跑啟動檔。環境是登入 shell 設好的那一份，和現在每個 session 拿到的一樣。
- 請求是一行：Ferry 的指令本來就編成一行（`sh_command`）。要給資料的請求，資料接在那一行之後，長度由請求說。
- 依序送進去的：
  1. probe：腳本不變，回應是原本的 `key=value`。
  2. `--remote-sudo` 的兩個檢查：`sudo -n … build-id`；要密碼時是 `sudo -k -S -p '' … build-id`，密碼那一行接在請求之後，sudo 一次讀一個位元組，不會多讀。stderr 與結束碼放進回應。
  3. 最後，`exec` 成這個 host 上第一個要啟動的 server，經 sudo 時照舊先寫密碼那一行。session 從此就是它的 control 連線，和現在一樣，握手之後照舊移到 `ssh -W`。
- 要安裝時，迴圈等著，安裝照舊另開 session，裝好之後再送第 3 步。所以要安裝時，probe 與 server 也在同一個 session。
- 迴圈只用 sh 與 `printf`，不用 Ferry，所以 Ferry 還沒裝的 host 也能用。
- 安裝要不要也送進迴圈，見[待決定的問題](#待決定的問題)。
- 迴圈必須寫在命令列上，不能是從標準輸入讀的腳本（`sh -s`）：本機的 dash 0.5.12 從 pipe 讀腳本時一次讀一大塊，把接在請求後面的資料當成指令執行，bash 不會。以 `read` 讀一行，dash 與 bash 都只讀到行尾（本機驗證）。

### 2. 回應加框

session 的標準輸出也在啟動檔留下的背景程序手上。host 上沒有程式在聽 socket，就沒有別人寫不進去的路：只有 `ssh -W` 到 socket 的 channel 是乾淨的，另開 session，也一樣被那個 session 自己的啟動檔拿著。所以回應不求獨立，求別人冒充不了、插不進去：

- **冒充不了。** 每個回應裝在一個框裡：client 這次執行隨機選的標記、內容的長度、內容，最後是帶同一個標記的結束記號。別的程式不知道這個標記，它們印的東西只會落在框外，client 跳過並說出來，和現在一樣（`say_skipped`，`src/transport/mod.rs:600`）。
- **插不進去。** 每個框一次寫出，而且夠小：pipe 保證一次寫入在 Linux 4096、macOS 512 位元組以內，不和別人的寫入交錯。準備的回應只有幾百位元組。本機驗證 dash 的 `printf` 把 3 KB 的回應一次寫出；bash 以 stdio 寫，沒有驗證到。
- **還是被插進去時。** 例如 sshd 用的是 socket pair，保證比 pipe 弱：長度與標記對不上，client 清楚地失敗，或重跑那一步，不會讀錯。probe 與 sudo 的檢查重跑無害。

這和現在 probe 與 server 的回答是同一個原理，多了每次執行不同的標記與長度。

### 3. 省下的

| 情況 | 現在 | 提案 |
|---|---|---|
| 預設 | 2 | 1 |
| `--remote-sudo` | 3–4 | 1 |
| 兩邊在同一個 host | 3 | 2（見待決定的問題） |
| `--remote-join` | 4 | 3 |
| 要安裝 | 2 加上傳 | 1 加上傳 |

- 在 nano4 上，每少一個 session，就少一整個 session 的時間，sshd 那段與 `.bashrc` 都在內。上面量到的是 10–16 秒，各部分沒有拆開。
- 往返（推算，沒有量）：每少一個 session，少它自己開 channel 與啟動指令的一兩個 RTT。

### 4. 不變的

- 準備的順序不變：每個 host 先 probe，問完、裝好、sudo 準備好，才啟動 server。迴圈只是等著，所以「every host has this build before either server starts」不必改。
- 一個 host 一條 SSH 連線；stream 由 client 建立，把兩端交給 server；連線的健康只在一處判斷。迴圈沒有計時器。
- 第一個 session 的指令仍是一行的 `sh -c '…'`，任何登入 shell 都能跑（TRN-04）。
- 本機的 server 不經 SSH，沒有 session，不受影響。

---

## 其他做法

- **probe 與啟動 server 合成一個 session。** build 已裝好時，probe 腳本印完結果就 exec server。預設的情況省的一樣，但 server 會在別的 host 問安裝、裝好之前就啟動，要改「every host has this build before either server starts」；sudo 不好合；要安裝時省不到。迴圈涵蓋它。
- **host 上的 agent。** 第一個 session probe 之後，exec 成 Ferry 的一個新模式：在 socket 上收 `ssh -W` 進來的連線，替每條連線開一個程式，把連線當它的標準輸入輸出。可以同時跑好幾件（兩邊同一 host 3 → 1、`ferry test` 的 fixture），連線本身乾淨。但要 Ferry 已裝好；子行程的結束碼與 stderr 要另外帶回 client，現在是 ssh 的結束碼直接告訴 client。可以疊在迴圈上，見待決定的問題。
- **跨次執行留著 agent。** 連第一個 session 都省。但 nano4 一個名稱後面有好幾台 login node，master 換到別台時，舊的 agent 連不到；login node 上長駐的行程可能被站方清掉；agent 帶著啟動當時的環境，`.bashrc` 或 module 改了之後就舊了；不同 build 要各一個；閒置多久結束要有規則。先不做。
- **使用者的 `.bashrc` 對 Ferry 先 return。** Ferry 在每個遠端指令裡放一個固定的字，例如當成 sh 的 `$0`，任何登入 shell 都照樣能跑；不能用 `VAR=1 cmd` 的寫法，csh 不認。文件給一行，讓想要的人加在 `.bashrc` 最前面：`case $BASH_EXECUTION_STRING in *<那個字>*) return ;; esac`。剩下的每個 session 都省掉 `.bashrc` 的時間，nano4 上是 1.6–3.7 秒。和提案互補，opt-in。
  - 要先確認 nano4 的 bash 讀啟動檔時，也已設好 `$BASH_EXECUTION_STRING`。
  - Ferry 不靠 `.bashrc`：它以絕對路徑執行自己的 binary，probe 只用標準的工具。例外是在 host 上建 dev build：Ferry 先找 `~/.cargo/bin`，cargo 只靠 `.bashrc` 才在 PATH 上的，會找不到。
- **Ferry 送 `SHLVL` 過去。** bash 看到就不讀 `.bashrc`（本機驗證），但 host 的 sshd 要以 `AcceptEnv` 收這個變數，預設只收 `LANG`、`LC_*`。不做。
- **client 記住上次的 probe，這次跳過。** nano4 的每台 login node 有自己的 machine ID，build 的路徑跟著不同；root 的種類每次也不同。不做。
- **給迴圈一條獨立的輸出：以 `ssh -R` 讓 host 連回 client。** 只有 bash 的 `/dev/tcp` 連得出去，sh 與 dash 不行；要 host 允許 remote TCP forwarding；而且會在 login node 的 localhost 開一個 port，那台機器上的任何使用者都連得到，還得再加驗證。不做。

---

## 待決定的問題

- [ ] **做不做，範圍。** 建議：probe、`--remote-sudo` 的檢查與 server 的啟動走迴圈，安裝照舊另開 session。使用者決定。
- [ ] **兩邊在同一個 host 時的第二個 server。** 再開一個 session，3 → 2，建議先這樣；或讓 server 能在背景啟動、改在自己的 socket 上握手，迴圈留著，3 → 1，要改 server 的握手，等於把 agent 疊上來。使用者決定。
- [ ] **安裝要不要也走迴圈。** 上傳的位元組和迴圈的請求在同一條標準輸入上，host 上收檔案的程式必須剛好讀到檔案的結尾就停，多讀一個位元組就吃掉下一個請求。現在的安裝一路讀到輸入結束（`cat > "$tmp"`、`tar -x -f -`，`src/prepare/install.rs`），在迴圈裡不能這樣讀。release 的 binary 是 4.2–5.5 MB（v0.16.0）。做法：
  - **GNU `head -c`。** 剛好讀那麼多：本機驗證，3 MiB 的二進位資料完全相同，之後迴圈照常讀下一行。但只有 Linux 的 GNU coreutils 是這樣；macOS 與 BusyBox 的 `head` 經過 stdio 的緩衝，會多讀（照它們原始碼的寫法推的，沒有試）。probe 說得出 `head` 是不是 GNU 的：是就走迴圈，否則另開 session。兩條路。
  - **`dd bs=1 count=N`。** 到處都剛好，但每個位元組一次 read、一次 write，5 MB 約一千萬次系統呼叫（推算，沒有量），在快的 host 上比開一個 session 還慢。不用。
  - **資料放最後，讀到輸入結束。** 最後一個請求是「收檔案、檢查、放好、啟動 server」。之後迴圈的輸入就沒了，所以 server 要改在自己的 socket 上握手（`ssh -W`），並以 control 連線、不以標準輸入判斷 client 還在：這和上一題的 3 → 1 是同一個改動。`--remote-sudo` 又要安裝時，密碼得在資料之前讀，錯了也不能再問，那種情況照舊另開 session。server 會在它的 host 裝好時就啟動、等著，設計那句要改成「每個 host 都裝好之前，沒有 server 開始工作」。
  - **照舊另開 session**（現在的提案）。要安裝時，每個 host 多一個 session。nano4 上 build 依 machine ID 各裝一份，每台 login node 各裝一次；dev build 在 host 上要建好幾分鐘，多一個 session 不顯著。
  - 建議和上一題一起決定：要 3 → 1，就連安裝一起走「資料放最後」；不要，就照舊另開 session。GNU `head -c` 為了很少發生的事多一條路，不建議。
- [ ] **`ferry test` 的 `ferry fixture` 要不要也走迴圈。** 只影響測試工具，但 nano4 上的驗收會快很多。
- [ ] **「`.bashrc` 對 Ferry 先 return」要不要一起做。**

---

## 計畫

每一步結束時，都要能執行、測試過。

0. **量測。** 決定做了才量：nano4（使用者先開好 ControlMaster）與 fatcat，把現在一次啟動的時間拆開，每個 session、握手、到第一個 block。
1. **迴圈與加框，只有 probe 走它。** 測試：dash 與 bash 下，迴圈讀請求不多讀；框外的東西被跳過並說出來；框被插進去時清楚地失敗；假的 ssh 在每個指令前後印東西，並留下之後才印的背景程序。
2. **`--remote-sudo` 的檢查與 server 的啟動走迴圈。** 以記錄每次 ssh 的假 ssh 測 session 數：預設 1、`--remote-sudo` 1、`--remote-join` 3。
3. **驗收與文件。** 本機、fatcat、nano4 的 `ferry test`；nano4 上的啟動時間對照第 0 步；`docs/design/` 的兩份與它們的 `.zh-TW.md`，以及 CHANGELOG。
