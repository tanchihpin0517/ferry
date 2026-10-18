# 準備階段改成明確的狀態機

- 狀態：討論中（決定都已做出，見「待決定的問題」；尚未動工）
- 開始：2026-10-18；最後修改：2026-10-18
- 基準：`src/` 與 HEAD `3c17a2f`（0.14.4）相同。以下行號都以它為準。
- 會改動：
  - `docs/design/pipeline.md` 的「Phase 1: Prepare」；
  - `docs/design/architecture.md` 的「Connections」（heartbeat 何時開始）與「Installing on a remote host」；
  - `docs/design/overview.md` 的「Guarding against a mistyped DESTINATION」；
  - 以上三份的 `.zh-TW.md` 版本。
- 相關：
  - [目標架構第 6 節](../review/2026-10-17/target-architecture.md#6-準備階段一個明確的狀態機)；
  - 審查項目 CLI-01、CLI-02、CLI-04、TRN-01、TRN-02、TRN-03、TRN-08、TRN-17、TRN-19、TRN-20、TRN-21、TRN-22、PRO-10；
  - 工作包 WP-01、WP-09、WP-10、WP-12、WP-19b；
  - 待決定事項 D1 至 D3、D5、D11（見[審查的 README](../review/2026-10-17/README.md) 第 7 節）。

這份草稿是 WP-01、WP-09、WP-10 的共同骨架：它們都改 `cli.rs` 的同一段程式，先照這份草稿把結構定下來，各工作包再往裡面填，才不會互相衝突。

---

## 問題

### 現在的流程

`cli.rs` 的 `transfer()`（`src/cli.rs:397-563`）在傳輸前依序做下面這些事：

| # | 做什麼 | 位置 | 會問使用者嗎 |
|---|---|---|---|
| 1 | 解析兩個位置；本機的 Source 先檢查根目錄 | `cli.rs:398-408` | 否 |
| 2 | 開 Source 的 transport，再開 Destination 的：每條 SSH 連線依序登入，並以 `ssh -N` 保持 | `cli.rs:431-432`、`transport/mod.rs:66-80` | 會（密碼、OTP） |
| 3 | 兩側根目錄的種類：host 上以一段 shell 腳本查詢，這時 Ferry 還沒安裝 | `cli.rs:435-436`、`cli.rs:640-678` | 否 |
| 4 | 依種類決定目的地，例如把檔案複製進目錄 | `cli.rs:437-444`、`cli.rs:684-718` | 會（複製進目錄嗎？） |
| 5 | 兩側的 sudo；同一台 host 只準備一次 | `cli.rs:447-454`、`cli.rs:565-579` | 會（sudo 密碼） |
| 6 | **啟動 Source**：探測 host、必要時詢問並安裝、啟動 server、握手、開 path（path 加入後，server 就開始監看 heartbeat，40 秒沒有就結束） | `cli.rs:455-462`、`cli.rs:585-627`、`remote/mod.rs:36-115`、`peer.rs:106` | 會（第一次安裝） |
| 7 | **啟動 Destination**：同上 | `cli.rs:463-470` | 會（第一次安裝） |
| 8 | relay 開始；client 這時才開始送 heartbeat | `relay.rs:76`、`transport/mod.rs:649-724` | 否 |

另外，`ferry test` 有自己的一份「登入、探測、安裝」：`testkit/mod.rs:416-425` 的 `Side::ready`。

### 問題

| # | 問題 | 審查項目 |
|---|---|---|
| P1 | **順序和設計不一致。** 設計是「所有 host 都探測、安裝完，才啟動任何 server」。程式卻是 Source 這一側整個跑完，才開始 Destination 這一側的探測與安裝。第一次推送到新 host 時，安裝的詢問、下載與上傳、在 host 上編譯，都夾在「Source 的 server 已在監看 heartbeat」與「client 開始送 heartbeat」之間，超過 40 秒，Source 的 server 就自行結束 | TRN-01 |
| P2 | **client 送 heartbeat 太晚。** path 在第 6、7 步就加入了，heartbeat 卻要到第 8 步才開始送 | TRN-01 |
| P3 | **失敗時說不清是哪一步、哪一台 host。** 各處的錯誤脈絡各寫各的；ssh 的失敗在沒有終端機時，一律被說成「needs a login」 | CLI-02 |
| P4 | **沒有根目錄的身分，也沒有防呆。** 兩邊根目錄重疊時，`sync` 會刪掉 Source；`host:` 會變成家目錄 | CLI-01、CLI-04 |
| P5 | **安裝不夠穩健。** 不驗證上傳的完整性；探測的輸出沒有標記；`keep_recent` 可能刪掉別人正在用的 build；上傳沒有逾時；暫存檔名會截掉部分版本號；`connect` 中途失敗時不清理 | TRN-02、TRN-19、TRN-20、TRN-21、TRN-22、TRN-17 |
| P6 | **握手先解碼整個 `Hello`，才比對 build。** `Hello` 的格式一改，版本不符的說明就看不到 | PRO-10 |
| P7 | **同一台 host 會開兩次。** `ferry copy host:a host:b` 對同一台 host 開兩組 SSH 連線、探測兩次。用 Ferry 自己的 ControlMaster 時，第二次登入會沿用第一條的 master，所以不會多問一次密碼，但多了一組 `ssh -N`。這是小問題 | （這份草稿新發現） |
| P8 | **`ferry test` 有自己的一份準備流程**，和 `transfer()` 遲早會分歧 | — |

### 已確認、可以依賴的事實

- 握手時，server 只檢查 Source 的根目錄，並建立自己的 socket 目錄（`session.rs:80-92`）。要等 path 全部加入、peer 開始執行，才會動到目錄樹。所以**握手之後、開 path 之前拒絕，兩邊的樹都不會被改動**。
- server 在 path 全部加入之後，才開始監看 heartbeat（`peer.rs:96-116`）。握手之後、開 path 之前，server 只是在等 path，靠它的標準輸入知道 client 還在。

---

## 提案

### 1. 狀態

```text
Parse ─▶ Connect ─▶ Probe ─▶ Ask ─▶ Install ─▶ Sudo ─▶ Start ─▶ Plan ─▶ Join ─▶（傳輸）
```

| 狀態 | 做什麼 | 作用的範圍 | 會問使用者嗎 |
|---|---|---|---|
| **Parse** | 解析位置（含 `host:~`、IPv6，見 CLI-04、TRN-08）；本機的 Source 檢查根目錄；建立 **place 表**（下節） | — | 否 |
| **Connect** | 每個 place 開一次 transport：依序登入每條 SSH 連線 | 每個 place 一次 | 會：密碼、OTP |
| **Probe** | 每台 host 跑一次探測腳本，**同時回報這台 host 上各根目錄的種類**，取代現在的 `root_kind`。輸出夾在標記之間（TRN-19） | 每個 host 一次 | 否 |
| **Ask** | 一次問完所有問題：哪些 host 要第一次安裝；要不要把檔案複製進目錄（現在的 `place_destination`） | 全部一起 | 會；`--yes` 只涵蓋安裝 |
| **Install** | 需要的 host 依序安裝：下載並驗證簽章（TRN-03）、上傳、在 host 上核對大小與雜湊、`--build-id` 確認能執行（TRN-02）、鎖住 build 目錄（TRN-21） | 需要安裝的 host | 否 |
| **Sudo** | `--remote-sudo` 時，每台 host 準備一次 sudo | 每個 host 一次 | 會：sudo 密碼 |
| **Start** | 依序啟動兩個 server 並握手：先比對 build（PRO-10），再交換選項；`Welcome` 帶回 root identity（CLI-01） | 每一側 | 否 |
| **Plan** | 以兩側的 root identity 套用防呆規則：重疊一律拒絕；危險的目的地先詢問（CLI-01、D1 至 D3）；產生傳輸的摘要 | 全部一起 | 會：危險的目的地 |
| **Join** | 兩側的 path 都加入，**client 立刻開始送 heartbeat**（TRN-01 的 A），交給 relay | 每一側 | 否 |

**提問的位置。** 所有提問依序集中在 Connect（登入）、Ask（安裝、複製進目錄）、Sudo（sudo 密碼）、Plan（危險的目的地）四處，都在 Join 之前；Join 之後，任何 server 都還沒開始傳輸。這符合 overview 的「一切需要使用者的事，都在傳輸開始之前問完」。

**對目標架構的修正。** 目標架構第 5.3 節與第 6 節寫著「SSH 連線一建立，heartbeat 就開始送」，這不精確：heartbeat channel 連到的是 server，server 啟動之前沒有對象可送。正確的說法是：

- **path 一開啟，client 就開始送 heartbeat**；
- 在那之前，ssh 連線本身的存活，由 ssh 的 `ServerAliveInterval` 負責；
- Start 與 Join 之間，server 只是在等 path，還不監看 heartbeat。

目標架構的那兩處已經照此修正。

### 2. place 表：每台機器只處理一次

```rust
/// Where a side runs: this machine, or a host as the user named it.
#[derive(Clone, PartialEq, Eq, Hash)]
enum PlaceKey {
    Local,
    Host(String),
}

/// One place, and what preparing has learned of it so far.
struct Place {
    key: PlaceKey,
    transport: Transport,          // Connect 之後才有
    probe: Option<Probe>,          // Probe 之後才有；本機為 None
    program: Option<String>,       // Install 之後才有
    sudo: Option<remote::Sudo>,    // Sudo 之後才有
}

struct SideSpec {
    role: Role,
    location: Location,
    place: usize,                  // places 中的索引
}
```

- 兩側在同一台 host 時，`places` 中只有一個項目：只登入、探測、安裝一次（P7）。
- 兩個不同的別名指向同一台機器時，不合併：各自探測、安裝，transport 也分開（Q3 的決定）。
- `ferry test` 的 `Side::ready` 改成呼叫同一套 Connect、Probe、Ask、Install 函式（P8）。

### 3. 用型別固定順序

每個狀態是一個型別，下一步只能從上一步的結果開始，順序因此由型別保證，無法跳過或顛倒：

```rust
pub fn prepare(cli: &TransferArgs, delete: bool) -> Result<Prepared, Failure> {
    let parsed    = Parsed::new(cli, delete)?;          // Parse
    let connected = parsed.connect()?;                  // Connect
    let probed    = connected.probe()?;                 // Probe
    let agreed    = probed.ask(cli.yes)?;               // Ask
    let installed = agreed.install()?;                  // Install
    let sudoed    = installed.sudo(cli.remote_sudo)?;   // Sudo
    let started   = sudoed.start(&hello)?;              // Start
    let planned   = started.plan(&rules)?;              // Plan
    planned.join()                                      // Join
}
```

- `Prepared` 就是現在交給 `relay::relay` 的兩個 `relay::Side`，relay 不需要修改。
- 任何一步失敗，已開的 transport、已啟動的 server，都隨著它們的 `Drop` 結束。Start 之後失敗時，以 `close_within` 給 server 幾秒鐘結束。
- 每個狀態都可以單獨測試：給它前一個狀態的值（例如以假的 ssh 建出的 transport），斷言它的結果與錯誤。

### 4. 失敗的說明

```rust
/// Where preparing failed: which step, at which place, and why.
pub struct Failure {
    step: Step,              // Connect、Probe、Ask、Install、Sudo、Start、Plan、Join
    place: Option<String>,   // "this machine"、"workbox"
    cause: anyhow::Error,
}
```

統一的訊息格式，例如：

```text
ferry: preparing workbox: installing 0.15.0: the upload ended early (12.0 MiB of 31.4 MiB)
ferry: preparing workbox: logging in SSH connection 1 of 1: ssh: Could not resolve hostname workbox
ferry: preparing: workbox: is the home directory on workbox; sync would delete everything in it that ./site does not have
```

CLI-02 的修法（保留 ssh 原始的錯誤訊息）放在 Connect；PRO-10 放在 Start。

### 5. 模組

```text
src/prepare/
  mod.rs      prepare()：各狀態依序串起來
  place.rs    PlaceKey、Place、place 表
  connect.rs  Connect（由 transport 的 open 做實際的登入）
  probe.rs    Probe：從 remote/probe.rs 移入，加上根目錄種類與輸出標記
  install.rs  Ask 與 Install：從 remote/mod.rs 的 ensure_installed、remote/install.rs 移入
  start.rs    Sudo、Start、Join：從 cli.rs 的 sudo_for、start 移入
  plan.rs     Plan：CLI-01 的防呆規則（審查中暫名 guard.rs）
  failure.rs  Step、Failure
```

`cli.rs` 的 `transfer()` 縮成：解析參數 → `prepare::prepare()` → relay 與顯示 → 摘要與結束狀態。

---

## 使用者會看到的差異（D11）

以 `ferry copy ./a newhost:b` 推送到一台還沒安裝 Ferry 的 host 為例：

| | 現在 | 改了之後 |
|---|---|---|
| 1 | 登入 newhost | 登入 newhost |
| 2 | 「starting the Source on this machine」 | 探測 newhost |
| 3 | 「starting the Destination on newhost」→ 探測 → 問要不要安裝 → 安裝。**這段期間 Source 的 server 已在跑，超過 40 秒就失敗** | 問要不要安裝 → 安裝 |
| 4 | — | 啟動兩個 server、握手 |
| 5 | — | 檢查計畫（重疊、危險的目的地） |
| 6 | relay 開始 | path 加入，heartbeat 立刻開始，relay 開始 |

改了之後，安裝和提問提早到任何 server 啟動之前。代價是：兩台 host 都要安裝時，若第二台安裝失敗，第一台已經裝好了。這沒有害處，下次直接用。

---

## 其他做法

- **只做 TRN-01 的 A：path 一開啟就送 heartbeat，順序不動。** 這修好了 40 秒的問題，改動最小。但 P1 的其他部分仍在：安裝夾在兩個 server 之間、失敗時 Source 的 server 已啟動、各段程式還是散在各處。這一步應該先做（計畫的第 2 步），但不能取代整理成狀態機。
- **延長 server 監看 heartbeat 的逾時，或在 Join 之前不監看。** 治標，而且安裝可能要好幾分鐘（在 host 上編譯 dev build），逾時要長到失去意義。不採用。
- **把 root identity 也放進探測腳本，Plan 提早到 Ask。** 這樣所有提問都在同一處。但 `(dev, ino)` 鏈、canonical path、`boot_id` 要在 shell 中跨 Linux 與 macOS 取得，既脆弱又難測。改由 server 用 Rust 取得，代價只是「危險的目的地」這一問晚一點問，仍在傳輸之前。不採用。

---

## 待決定的問題

2026-10-18 由使用者決定：

- [x] **D11**：重排準備的順序。所有 host 都探測、安裝完，才啟動任何 server。
- [x] **Q1**：「危險的目的地」在 Plan 才問，可以。
- [x] **Q2**：根目錄的種類併入探測腳本。
- [x] **Q3**：都不合併。兩個不同的別名即使指向同一台機器（machine-id 相同），也各自探測、安裝，transport 也分開。第二次探測通常會發現已經裝好，代價不大。同一個別名在兩側出現時，仍然只處理一次（place 表以別名為鍵）。
- [x] **D1**：兩邊的根目錄重疊時（相同、Destination 在 Source 裡、Source 在 Destination 裡），`copy` 與 `sync` 一律拒絕，不提供覆寫的旗標。
- [x] **D2**：`sync` 的目的地是 `/`、某個家目錄或家目錄的上層、或空的遠端路徑時，在終端機上詢問。沒有終端機時拒絕，除非給了新的旗標（暫名 `--allow-dangerous-destination`）。`--yes` 不涵蓋這一類。`--dry-run` 只警告。
- [x] **D3**：遠端路徑的 `~` 與 `~/` 在 client 端展開成相對於家目錄的路徑；`~user` 先明確拒絕；要字面上的 `~`，寫 `host:./~/x`。

目前沒有待決定的問題。

---

## 計畫

每一步都以「`make check` 通過、`ferry test /tmp /tmp` 通過」結束，並各自是一個 commit。

1. **只搬移，不改行為。**
   - 新增 `src/prepare/`，把 `transfer()` 的準備部分、`sudo_for`、`start`、`root_kind`、`ensure_installed` 搬進去；
   - 加入 place 表、`Step`、`Failure`；
   - 順序完全照舊；
   - `ferry test` 的 `Side::ready` 改用同一套函式。
   - 驗收：現有測試全部通過，輸出不變。
2. **TRN-01 的 A：path 一開啟，client 就送 heartbeat。**
   - 把 heartbeat 的啟動從 `Connection::split` 提前到 `Transport::connect` 回傳時。
   - 測試：用 TRN-01 的假 ssh（`DELAY`），加上只在 dev build 生效的較短 heartbeat 時間，確認 Destination 的啟動慢於逾時也不會失敗。
3. **安裝的完整性**：TRN-02、TRN-19、TRN-20、TRN-21、TRN-22、TRN-17，各有自己的測試，見 transport.md。
4. **依 D11 重排順序。**
   - 根目錄種類併入探測（Q2）；
   - 提問集中到 Ask；
   - 全部安裝完，才進入 Start。
   - 測試：用假 ssh 記錄各步驟的順序，斷言「最後一次安裝」先於「第一個 server 啟動」。
5. **Start 帶回 root identity，加入 Plan**：CLI-01、CLI-04、DOC-01，依 D1 至 D3。測試見 CLI-01。
6. **錯誤說明**：CLI-02（Connect 保留 ssh 原始的錯誤）、PRO-10（Start 先比對 build）。
7. **簽章**：TRN-03 掛在 Install 步驟，依 D5。

第 2、3 步不依賴第 1 步，可以先做。第 4 到 7 步建立在第 1 步的結構上。

## 驗收條件（整體）

- [ ] 準備階段任何一步失敗，或在任何一步按 Ctrl-C，兩邊的樹都沒有改動。以假的 ssh 在每一步注入失敗來測試。
- [ ] 所有提問都在 Join 之前。
- [ ] 第一次推送到一台需要安裝 45 秒以上的 host，傳輸成功。
- [ ] 每一個準備階段的錯誤，都說明是哪一步、哪一台 host。
- [ ] `ferry copy host:a host:b` 只登入、探測、安裝 host 一次。
- [ ] `ferry test` 與 `ferry copy` 走同一套準備函式。
