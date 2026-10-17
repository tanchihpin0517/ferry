# Ferry 目標架構：如果從零設計

- 日期：2026-10-17
- 基準：HEAD `3c17a2f`（version 0.14.4），以及同目錄的審查結果與量測（[README](README.md)、[architecture.md](architecture.md)）。
- 定位：這是長期方向，不是承諾，也不是排程。它回答一個問題：「如果可以完全重寫，架構會長什麼樣子？」
  - 實際上不建議一次重寫。從現在的程式碼一步步走到這裡的路線，寫在第 15 節。
  - [architecture.md](architecture.md) 的 ARC-01 到 ARC-04，都是這條路上的一段。
- 讀者：之後設計或實作 Ferry 的人與 agent。文件從高層概念開始，逐節往下展開到 pipeline 的細節。

## 目錄

1. [設計目標與不變的承諾](#1-設計目標與不變的承諾)
2. [與現在的架構差在哪裡：七個核心決定](#2-與現在的架構差在哪裡七個核心決定)
3. [概念與名詞](#3-概念與名詞)
4. [系統分層](#4-系統分層)
5. [行程、連線與三條邏輯串流](#5-行程連線與三條邏輯串流)
6. [準備階段：一個明確的狀態機](#6-準備階段一個明確的狀態機)
7. [協定：訊息與 block 的格式](#7-協定訊息與-block-的格式)
8. [流量控制與記憶體上限](#8-流量控制與記憶體上限)
9. [Source 的 pipeline](#9-source-的-pipeline)
10. [Destination 的 pipeline](#10-destination-的-pipeline)
11. [持久性與原子性](#11-持久性與原子性)
12. [錯誤、停止與斷線重連](#12-錯誤停止與斷線重連)
13. [安全模型](#13-安全模型)
14. [可觀測性與測試架構](#14-可觀測性與測試架構)
15. [模組配置，以及從現在走過來的路](#15-模組配置以及從現在走過來的路)
16. [開放問題](#16-開放問題)

---

## 1. 設計目標與不變的承諾

### 目標，依優先順序

1. **可靠。** 每個檔案要嘛是舊版，要嘛是完整、驗證過的新版，斷電後也一樣。沒有任何操作碰到目的地根目錄之外的東西。`sync` 絕不刪除 Source 讀不到的東西。
2. **快。** 傳輸開始之後，速度優先。小檔要接近或超過 `cp`；大檔要接近磁碟或網路的上限；在 NFS、Lustre 這類共用檔案系統上，對伺服器的負載要有上限。
3. **記憶體有確定的上限。** 一億個項目的樹，記憶體也不隨項目數增長，只隨「在途的工作」與「最大的一個目錄」增長。
4. **不需要在 host 上預先做任何事。** 只要能 `ssh` 進去，Ferry 自己上傳同一個 build。
5. **看得見。** 任何時候都能說出時間花在哪個 stage、誰在等誰。

### 不變的承諾

這些是從現在的設計（`docs/design/overview.md`）沿用的保證，重寫也不能違反：

- **每個檔案都是原子性的。** 寫在暫存位置，完成並驗證後才給它最終名稱。
- **端到端驗證。** Source 讀取時計算 BLAKE3，Destination 寫入前檢查。
- **預設 fsync。** 資料先持久，名稱才出現。
- **與 `rsync -a` 對等的 metadata**：權限、mtime、owner、group、symlink、裝置、FIFO、socket。
- **一切需要使用者的事，都在傳輸開始之前問完。**
- **目的地上的每一個操作都不跟隨 symlink。**

### 不做的事

- 不相容 rsync 的命令列。
- 不做常駐的 daemon，不開網路 port。
- 第一版不做 delta transfer，但格式要為它留路，見 10.5 節。

---

## 2. 與現在的架構差在哪裡：七個核心決定

| # | 決定 | 現在 | 為什麼改 |
|---|---|---|---|
| 1 | **控制面與資料面分開，資料切成獨立的 block** | 一條有序串流，frame 不管檔案邊界 | 現在 Destination 只能用一個 parser 解析；一個大檔只能由一個 writer 寫；斷線就整個失敗。量測：4 GiB 的檔案受限於一條 writer thread |
| 2 | **由接收端發放額度來控制流量** | 隱含的背壓，加上手調的上限 | 現在的上限不準：backlog 的上限 10 萬，實測到 82 萬（SRC-04）。改成記憶體上限由 Destination 一處決定 |
| 3 | **Destination 依目錄分 shard，每個目錄的名稱空間只有一個寫入者** | 一個 `Mutex<State>`；16 個 writer 在同一目錄內搶目錄鎖 | 量測：小檔情境的系統時間是 `cp` 的 19 倍，主要是同一目錄的鎖競爭 |
| 4 | **stage 圖加上 supervisor** | 每個 stage 自己 spawn、join，靠關 channel 傳遞錯誤 | 報告中最大的一類 bug：失敗後其他 thread 繼續做事、原因被後果蓋掉、panic 變成停滯（PRO-00、SRC-02、SRC-03） |
| 5 | **檔案系統 API 讓錯誤寫不出來** | 靠紀律：`one_name`、`O_NOFOLLOW` | DST-01（chmod 跟隨 symlink）、DST-02（暫存檔殘留）這類問題，用型別一開始就排除 |
| 6 | **錯誤分類與訊息大小都由型別與編碼器決定** | 散落各處，不一致 | DST-04 到 DST-06、PRO-01、PRO-03 這類問題，一開始就不會發生 |
| 7 | **先規劃再傳輸；用故障注入的模擬來測試** | 準備順序與設計不符；沒有根目錄身分；只有整合測試 | 能根除 CLI-01（刪錯資料）與 TRN-01（heartbeat 逾時），也有更強的測試保證 |

沿用不改的：

- Rust 加同步 thread，不用 async；
- 系統的 `ssh`，可以開多條 TCP 連線；
- client 加兩個 server，remote-to-remote 時資料經過 client 中繼；
- client 與 server 必須是同一個 build；
- 比較在 Destination 端做，邊收 manifest 邊合併兩邊的目錄清單；
- manifest 依深度優先、依名稱排序；
- 掃描用一個 lister 加 8 個 stat。

---

## 3. 概念與名詞

| 名詞 | 意思 |
|---|---|
| **client** | 使用者執行的 `ferry`。負責準備、產生計畫、中繼兩個 server 之間的資料、顯示進度。它本身不是 peer |
| **server** | client 經由 transport 啟動的 `ferry server`。每個 server 執行一個 peer |
| **peer** | **Source**（讀檔的一方）或 **Destination**（寫檔的一方） |
| **transport** | 擁有所有 SSH 連線與 path 的模組。對上提供三條邏輯串流（第 5 節） |
| **path** | 一條雙向的位元組通道：一個 SSH channel，或本機的一個 Unix socket。所有 path 地位相同 |
| **root identity** | 一個根目錄的身分：kernel 的 `boot_id`、`(dev, ino)` 鏈、canonical path、檔案系統的種類 |
| **plan** | client 在傳輸前產生的計畫：兩個根目錄、選項、安全檢查的結果。兩個 peer 都依它行事 |
| **entry** | 樹中的一個項目：檔案、目錄、symlink、特殊檔。有 metadata |
| **EntryId** | entry 的編號，依 manifest 的順序遞增。根目錄是 0 |
| **dir key** | 目錄自己的 EntryId，用來決定它歸哪個 shard 管 |
| **manifest page** | 一個目錄的一段 entry 清單，同時以筆數與位元組數為上限 |
| **want** | Destination 對 Source 說「我要這些 entry 的內容」 |
| **block** | 資料面的單位，最大 B（預設 4 MiB）的原始資料，自己說明自己是什麼，可以獨立解壓、驗證、處理。分兩種：**pack**（多個完整的小檔）與 **segment**（大檔的一段） |
| **BlockId** | Source 指定的 block 編號，遞增 |
| **credit** | 額度。Destination 發給 Source 的「還可以送多少」：manifest 額度與資料額度 |
| **shard** | Destination 上的一條 thread，獨自擁有一部分目錄的名稱空間與狀態 |
| **pending file** | 寫入中的檔案，還沒有最終名稱。drop 時一定會被刪除 |
| **stage** | pipeline 中的一個處理步驟，由一條或一組 thread 執行 |
| **supervisor** | 管理 stage 圖的 runtime，負責記下第一個錯誤、讓全體停止、收集 metrics |

---

## 4. 系統分層

```text
┌──────────────────────────────────────────────────────────────────┐
│ L5 client      cli · prepare（狀態機） · plan · relay · dashboard │
├──────────────────────────────────────────────────────────────────┤
│ L4 peers       source/（scan、manifest、read、pack、send）         │
│                destination/（receive、compare、apply、shard、     │
│                             content、durable）                     │
├──────────────────────────────────────────────────────────────────┤
│ L3 wire        messages · block 格式 · 有上限的 codec · 自動分段  │
├──────────────────────────────────────────────────────────────────┤
│ L2 transport   ssh · local · path · control stream（可靠、有序）  │
│                · data（block，無序） · heartbeat · 重連           │
├──────────────────────────────────────────────────────────────────┤
│ L1 runtime     stage 圖 · supervisor · Stop · channel · metrics   │
├──────────────────────────────────────────────────────────────────┤
│ L0 fsx         Name · Dir · Handle · Pending · DirCache           │
│                · Linux / macOS backend · 模擬 backend             │
└──────────────────────────────────────────────────────────────────┘
```

每一層只依賴下面的層：

- **L0 與 L1 不知道 Ferry 的協定。** 它們可以被模擬（第 14 節）。
- **L2 不讀它承載的內容**，只知道三條串流的種類。
- **L3 是純函式**：編碼、解碼、檢查上限，可以直接拿來做 fuzz。
- **L4 只透過 L0 碰檔案系統**，只透過 L2 碰網路。

---

## 5. 行程、連線與三條邏輯串流

### 5.1 行程

```text
             ┌────────────── client（ferry） ──────────────┐
             │ prepare · plan · relay · dashboard          │
             └──────┬───────────────────────────┬──────────┘
                    │ transport A                │ transport B
                    │（本機或 SSH）              │（本機或 SSH）
            ┌───────▼────────┐          ┌───────▼────────┐
            │ ferry server   │          │ ferry server   │
            │ peer: Source   │          │ peer: Destin.  │
            └────────────────┘          └────────────────┘
```

- client 永遠在中間。兩台主機不需要互通，也不需要在任何一台上放憑證，這和現在相同。
- 本機的 server 是 client 自己的 binary，以子行程啟動，path 是 Unix socket。

### 5.2 三條邏輯串流

每個 server 與 client 之間，在同一組 path 上多工三條串流：

| 串流 | 兩端 | 內容 | 性質 |
|---|---|---|---|
| **server control** | client ↔ server | 握手、root identity、plan、reports（進度、警告、dry run 清單）、metrics、快照的查詢、停止 | 可靠、有序，量小 |
| **peer control** | Source ↔ Destination，經 client 中繼 | manifest page、want、credit、results、`ComparedThrough`、error、done | 可靠、有序。可靠性逐段保證（Source ↔ client、client ↔ Destination） |
| **data** | Source → Destination，經 client 中繼 | block | 無序。每個 block 整個走同一條 path |

和現在不同的地方有兩個：

- **reports 改走 server control。** client 中繼 peer control 時完全不必解讀內容；現在的 client 要逐一看 packet 的種類，把 report 挑出來。
- **資料不需要全域的順序。** client 收到一個完整的 block，就挑一條往 Destination 的 path 轉出去，不必重排。

### 5.3 path 上的 frame

```text
frame = [kind: u8][length: u32][payload]
kind  = ServerControl | PeerControl | DataFragment | PathEnd
```

- **control frame** 帶序號 `seq: u64`，以及對另一個方向的累計確認 `ack: u64`。傳送端保留還沒被確認的 control 訊息，放在重送緩衝區中，以位元組為上限，例如 4 MiB；滿了就等。
- **`DataFragment`**：`{ block_id, offset, last, bytes ≤ 64 KiB }`。
  - 同一個 block 的所有 fragment 走同一條 path，依序傳送，所以接收端每條 path 只需要重組「正在收的那幾個 block」。
  - control frame 可以插在兩個 fragment 之間，所以控制訊息的延遲不受大 block 影響。
- **path 的傳送佇列有兩級優先序**：control 永遠先於 data。
- **`PathEnd`** 是 path 正常結束的記號，沿用現在的 end marker 設計。沒有 `PathEnd` 就結束的 path，視為斷線。
- **heartbeat** 仍在每條 SSH 連線旁邊各有一個獨立的 channel，**SSH 連線一建立就開始送**（修正 TRN-01）。

### 5.4 path 的配置

- 開 N 條 SSH 連線（`--ssh-connections`），各自是獨立的 TCP 連線。每條連線上開 K 個 channel 當 path：K 預設為 1，之後依 RTT 決定，以突破 SSH 的 window 上限。
- 本機的 path 是 Unix socket，預設 2 條。
- block 交給「佇列最短」的 path。control 訊息也交給佇列最短的 path；對方依 `seq` 重排，重排緩衝區很小，因為 control 的量很小。

---

## 6. 準備階段：一個明確的狀態機

```text
Parse ─▶ Connect ─▶ Probe ─▶ Confirm ─▶ Install ─▶ Start ─▶ Handshake ─▶ Identify ─▶ Plan ─▶ Transfer ─▶ Drain ─▶ Exit
           │          │         │          │         │          │            │          │
           └──────────┴─────────┴──────────┴─────────┴──────────┴────────────┴──────────┴──▶ Fail（說明是哪一步、哪一台）
```

| 狀態 | 做什麼 | 會問使用者嗎 | 失敗時 |
|---|---|---|---|
| **Parse** | 解析參數與位置（含 `host:`、`host:~`、IPv6）。本機的根目錄先 `stat` | 否 | 用法錯誤，exit 2 |
| **Connect** | 開啟**所有** host 的**所有** SSH 連線，一條接一條，每條先用 `BatchMode=yes`，失敗再在終端機上登入。**連線一建立，heartbeat 就開始送** | 會（密碼、OTP） | 說明哪一台、第幾條連線、ssh 的原始錯誤（修正 CLI-02） |
| **Probe** | 對每台 host 執行探測腳本：OS、架構、machine-id、已安裝的 build。輸出夾在標記之間（修正 TRN-19） | 否 | 說明探測失敗的原因 |
| **Confirm** | 若有 host 需要第一次安裝，一次問完所有 host | 會；`--yes` 跳過 | 使用者拒絕時，什麼都不改就結束 |
| **Install** | 對每台需要的 host：下載（並驗證簽章），上傳到 `bin/<build>/ferry.<random>.part`；host 上的腳本**核對大小與 SHA-256** 之後才改名，再執行 `ferry --build-id` 確認可以執行（修正 TRN-02） | 否 | 刪除殘檔，說明原因 |
| **Start** | 啟動兩個 server。server 回報 socket 與 token，path 加入 | 否 | 說明 server 為何沒有啟動 |
| **Handshake** | 比對 build（先比對 build 字串，再解碼其他欄位，修正 PRO-10），交換角色與選項 | 否 | build 不同時，說明兩邊各是哪一版 |
| **Identify** | 兩個 server 各自回報 root identity（第 13.3 節） | 否 | 根目錄不存在等情況，依 plan 的規則處理 |
| **Plan** | client 套用安全規則：重疊、危險的目的地、`--max-delete`，產生 plan，送給兩個 peer | 危險的目的地會問；沒有終端機時拒絕，除非給了明確的旗標 | 拒絕時，什麼都不改就結束 |
| **Transfer** | 第 9、10 節 | 不再問任何事 | 第 12 節 |
| **Drain** | 等 Destination 的 `Done`、兩個 server 結束；印出摘要 | 否 | — |

原則：

- 進入 Transfer 之前，任何一步失敗，兩邊的樹都不會被改動。
- 各步驟中，對多台 host 的探測與安裝可以平行進行。登入必須一條接一條，以免提示互相穿插。
- host 上的 build 目錄在使用期間以 `flock` 鎖住，其他 client 的「只保留最近三個 build」不會刪掉正在用的 build（修正 TRN-21）。

---

## 7. 協定：訊息與 block 的格式

### 7.1 編碼器的規則

- 用 postcard 編碼。每種訊息有自己的位元組上限，解碼前先檢查長度。
- **凡是帶清單的訊息，都由 builder 依位元組與筆數自動分段。** 例如 `WantBuilder::push(id)` 滿了就自動送出一則，再開新的一則。所以沒有任何訊息會超過上限（根除 PRO-01、PRO-03）。
- client 與 server 一定是同一個 build，所以不需要版本相容。握手時第一個欄位就是 build 字串，先比對它，再解碼其餘的欄位。

### 7.2 peer control 的訊息

| 方向 | 訊息 | 內容 | 說明 |
|---|---|---|---|
| S → D | `Root` | 根目錄的 entry | 第一則訊息 |
| S → D | `Names` | 新出現的 uid、gid 對應的名稱 | 在第一個用到它們的 entry 之前送 |
| S → D | `Page` | `{ dir: EntryId, entries: [Entry], last: bool }` | 一個目錄的一段；`entries` 的 EntryId 必須連續 |
| S → D | `Problem` | `{ id, message }` | Source 讀不到的 entry 或目錄 |
| S → D | `ManifestEnd` | — | manifest 結束 |
| D → S | `ManifestCredit` | `{ bytes }` | 可以再送多少 manifest |
| D → S | `ComparedThrough` | `{ id }` | Source 可以忘掉這個編號以前、沒有被 want 的 entry |
| D → S | `Want` | `{ ids: [EntryId] }` | 要這些檔案的內容 |
| D → S | `DataCredit` | `{ bytes, acked: [BlockId] }` | 歸還額度，**同時確認這些 block 已經收到並驗證過** |
| D → S | `Results` | `{ committed: [EntryId], failed: [(EntryId, reason)] }` | 分批送；用於 Source 的摘要 |
| 雙向 | `Error` | `{ message }` | 每個 peer 最多送一次 |
| D → S | `Done` | — | 一切都已持久，絕不在 `Error` 之後送 |

### 7.3 block 的格式

```text
block  = header || payload（壓縮或原樣） || blake3(raw)
header = { block_id: u64,
           kind: Pack | Segment,
           codec: Raw | Zstd(level),
           raw_len: u32,
           body: PackIndex | SegmentInfo }

PackIndex   = [ { id: EntryId, dir: EntryId, len: u32, unread: bool } ]
SegmentInfo = { id: EntryId, dir: EntryId, offset: u64, len: u32,
                size: u64, mtime: Timestamp, unread: bool }
```

- 雜湊是對**原始資料**計算的。Destination 解壓之後先驗證，再使用。
- **pack** 把多個完整的小檔（≤ 門檻，例如 1 MiB）依 want 的順序串接在一起。不同目錄的檔案可以在同一個 pack 中。
- **segment** 是大檔的一段，長度 ≤ B，帶 offset。同一個檔案的 segment，都帶著開檔時那一次 fstat 的 `size` 與 `mtime`，所以 Destination 能確認它們屬於同一個版本。
- `unread` 表示 Source 沒讀完整：檔案在讀取期間變小，或讀取失敗。原始資料仍補足長度，以保持格式一致；Destination 讓這個 entry 失敗。
- `dir` 讓 Destination 不必查表，就知道要交給哪個 shard。Destination 仍會檢查這個 entry 確實是它要的，而且確實在這個目錄裡，否則就是協定違規。

---

## 8. 流量控制與記憶體上限

### 8.1 三種額度

- **manifest 額度。**
  - Destination 一開始發 8 MiB。之後每消化一個 page，也就是比較完、套用完、送出它的 want，就歸還該 page 的位元組。
  - Source 的 manifest builder 沒有額度時就等。這同時讓 Source 的掃描停下來。
- **want 的上限。**
  - Destination 自己限制「已 want、還沒收到」的檔案數，例如 W = 10 萬。到達上限時，applier 先不處理下一個比較結果，於是 manifest 額度不再歸還，Source 的掃描也就停下來。
  - **這一處上限，同時限制了 Source 的待讀佇列與 Destination 的待寫清單**，取代現在的 backlog 上限（SRC-04）。
- **資料額度。**
  - Destination 一開始發 64 MiB。之後依頻寬與 RTT 的乘積自動調整，下限 16 MiB，上限 512 MiB。
  - Source 每送出一個 block，就扣掉它壓縮後的大小。Destination 釋放一個 block 的記憶體時，歸還額度，並在同一則 `DataCredit` 中確認它的 BlockId。

### 8.2 各側的記憶體上限

| 位置 | 內容 | 上限 |
|---|---|---|
| Source | 掃描的讀取前瞻 | lister 一次只列一個目錄；最大一個目錄的名稱 |
| Source | manifest 在途 | manifest 額度（8 MiB） |
| Source | `Known`（已送出、還沒 `ComparedThrough` 的 entry 路徑） | 由 manifest 額度與 Destination 比較管線的深度決定 |
| Source | 待讀的檔案 | W（由 Destination 的 want 上限保證） |
| Source | 未確認 block 的中繼資料（重連時重送用） | 資料額度 ÷ 最小 block 大小 |
| Source | 讀取與壓縮中的緩衝區 | reader 數 × B，加上 compress 數 × B |
| client | 中繼中的 block | 每條 path 正在收的 block，加上轉送佇列（以位元組為上限，例如 32 MiB） |
| Destination | 比較中的 page | 固定數量，例如 8 段 × 10,000 個 entry |
| Destination | 一個目錄的清單 | 最大一個目錄（現在的設計也是如此） |
| Destination | 解碼後的 block | 資料額度 |
| Destination | 各 shard 中開著的目錄狀態 | 深度優先 stack 上的目錄，加上還有在途檔案的目錄 |
| Destination | 已 want 的 target | W |
| Destination | pending file 的 fd | fd 預算（第 11.4 節） |

**每一項都要有一個測試**，用 `ferry test --scale` 的大型樹量測峰值，確認不隨項目數增長。

---

## 9. Source 的 pipeline

```text
                      ┌─────────────── peer control ───────────────┐
                      │                                            ▼
lister(1) ─▶ stat(8) ─▶ manifest builder(1) ══Page══▶   ◀══ManifestCredit / ComparedThrough / Want / DataCredit══
  依深度優先、        依序；分配 EntryId；                │
  依名稱列目錄        依額度送出 page；                  ▼
                     記入 Known                   want handler(1)
                                                   │ 依 Known 找出路徑
                                                   ▼
                                     read jobs ─▶ readers(16)
                                                   │  小檔：整個讀入
                                                   │  大檔：切成 segment job
                                                   ▼
                                               packer(1)
                                                   │  小檔裝進 pack；segment 原樣通過
                                                   ▼
                                            compress(8)
                                                   │  本機或壓不下去時跳過
                                                   ▼
                                           block sender(1) ══block══▶ transport（任一 path）
                                                   │ 依資料額度送；記下未確認的 block
```

### 9.1 掃描（lister 加 stat pool）

- 沿用現在的設計：一條 lister 依深度優先、依名稱，一次列一個目錄；8 條 stat thread 依序 stat。對 NFS 這類共用伺服器，同時發出的請求最多 9 個。
- 每個目錄從父目錄的 `Dir` handle 以 `openat` 開啟（`O_DIRECTORY|O_NOFOLLOW`）。Linux 用 `getdents64` 搭配大的緩衝區，`statx` 只要必要的欄位，並帶 `AT_SYMLINK_NOFOLLOW|AT_NO_AUTOMOUNT`。
- 目錄超過 10,000 個 entry，就分成多段送出。
- 可選的 `--one-file-system`：遇到 `st_dev` 不同的子目錄，只送 entry 本身，不進去。
- 列到、但 stat 不到的名稱，以 `Unreadable` 送出，Destination 就不會把它當成多餘的項目。
- 列不出內容的目錄，送出時標成「內容不完整」，Destination 不在其中刪除任何東西。

### 9.2 manifest builder

- 依序分配 EntryId，組成 page（≤ 10,000 個 entry 且 ≤ 1 MiB），並記入 `Known`。`Known` 以目錄為單位，存放精簡的路徑，像現在的做法。
- 沒有 manifest 額度時就等，掃描也跟著停下來。
- 收到 `ComparedThrough(id)` 時，從 `Known` 釋放 id 以前、沒有被 want 的 entry。

### 9.3 want handler 與 reader

- want handler 依 `Known` 找出每個 EntryId 的路徑，交給 reader 去讀，並把它從 `Known` 移出。
- reader 開檔的方式：
  1. 從 `DirCache` 取得目錄 handle。路徑不再從根目錄重走，根目錄也只解析一次（修正 SRC-06）。
  2. 以 `O_RDONLY|O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC` 開啟，擁有者是自己時再加 `O_NOATIME`。
  3. `fstat` 確認是一般檔案，否則以 `unread` 回報。這樣檔案被換成 FIFO 也不會卡住（修正 SRC-01）。
  4. 清掉 `O_NONBLOCK`。
  5. 以這一次 `fstat` 的大小與 mtime 為準。
- 小檔：整個讀入，計算雜湊之前先交給 packer。
- 大檔：開檔一次，以 `Arc<File>` 共用，切成每段 B 的 segment job，重新放回 reader 的佇列。每個 segment 由任一 reader 以 `pread` 讀取，**同一個大檔由多條 thread 平行讀取**（修正 SRC-11）。任何一段讀不完整，那一段就標為 `unread`，Destination 讓整個檔案失敗。

### 9.4 packer、壓縮與傳送

- packer 依 want 的順序，把小檔裝進 pack，滿 B 或閒置一段時間（例如 2 ms）就封口；segment 直接通過。每個 block 在這裡計算原始資料的 BLAKE3。
- compress pool：
  - 兩側都在本機時不壓縮。
  - 連續 8 個 block 都壓不下去時，接下來 64 個不試，之後再試一次。
  - 壓縮後沒有變小，就原樣送出。
  - 等級預設為 1。
- block sender：
  - 依資料額度送出 block。
  - 為每個 block 記下中繼資料：它包含的 entry、路徑、offset、長度、開檔時的大小與 mtime。收到確認後就丟掉。
  - 重連時，依這份中繼資料重新讀取並重送（第 12.4 節）。重讀時若大小或 mtime 已經不同，就以 `unread` 送出。

### 9.5 Source 的結束

- 收到 `Done`：送出摘要的 report，然後結束。
- 收到 `Error`，或自己失敗：經 supervisor 停止所有 stage。掃描立刻停（修正 SRC-02），只送出一次 `Error`，然後結束。

---

## 10. Destination 的 pipeline

### 10.1 全圖

```text
transport ══peer control══▶ receiver(1) ──Page──▶ compare lister(1) ─▶ stat(8) ─▶ applier(1)
    ║                          │                  依 manifest 走訪          依 manifest 的順序
    ║                          │                  目的地的目錄              ├ 決定 want 與多餘的項目
    ║                          │                                            ├ 送 DirectoryWork 給 shard
    ║                          │                                            ├ 送 Want、ManifestCredit、
    ║                          │                                            │  ComparedThrough
    ║                          │                                            └ 送 Left{dir} 給 shard
    ║                          │
    ╚══data block══▶ path 重組 ─▶ decode/verify(8)
                                 │ 解壓，驗證 BLAKE3
                                 │ pack 依 dir 分組；segment 原樣
                                 ▼
                          shard[hash(dir) % N]（N 預設 8）
                            ├ 擁有它負責的目錄：狀態、target、pending file
                            ├ 名稱空間的唯一寫入者：刪除、mkdir、symlink、link、rename、目錄收尾
                            └ 把內容工作交給 content workers
                                 │                         ▲
                                 ▼                         │ Written / Synced
                          content workers(16)      ─▶ fsync pool(64，只在等待)
                            O_TMPFILE、寫入、屬性           │
                            大檔：pwrite 到 offset           ▼
                                                    回到 shard：link 到最終名稱、回報結果
```

### 10.2 比較（compare lister、stat pool、applier）

比較的方式沿用現在的設計，只是換了執行它的人。

- **compare lister**：依 manifest 的順序走訪目的地的目錄，每個目錄只開啟一次、只列一次，與 Source 的 page 合併。目的地沒有的目錄不進去，其中的一切都要傳。
- **stat pool**：只 stat 兩邊都有的 entry，做 quick check。
  - mtime 預設以整秒比較，並沿用 `--modify-window`。
  - 新增：mtime 離 Source 掃描開始的時間不到 2 秒時，改以奈秒比較，借鏡 git 處理 racy timestamp 的做法（SRC-13）。
- **applier**：一條 thread，依 manifest 的順序取得比較結果。一個目錄的最後一段比較完時，它：
  1. 算出多餘的項目（只有 `sync` 才算）。Source 讀不到的名稱、內容不完整的目錄、和讀不到的名稱互為別名的 entry，都不算多餘。這些規則沿用現在的設計。
  2. 組成一份 `DirectoryWork`，交給該目錄的 shard。內容包括：
     - 目錄 handle（比較時開啟的那一個）；
     - 要刪除的名稱；
     - 要建立的子目錄、symlink、特殊檔；
     - 要修正屬性的 entry；
     - 要 want 的檔案的 target，也就是名稱與目標 metadata。
  3. **先**把 `DirectoryWork` 放進 shard 的佇列，**再**把這些 want 經 `WantBuilder` 送給 Source。因為 shard 依佇列順序處理，它一定先處理完 `DirectoryWork`，才會收到這些檔案的內容。所以「先刪多餘的項目，再建立任何東西」在新結構中仍然成立。
  4. 檢查 want 的上限 W：到達上限時先停住，不處理下一個比較結果。
  5. 歸還這些 page 的 manifest 額度；每隔 4,096 個 EntryId，送一次 `ComparedThrough`。
  6. manifest 離開一個目錄以及其下所有目錄時，送 `Left{dir}` 給那個目錄的 shard。
- **dry run**：applier 不送 `DirectoryWork` 也不送 want，只把會發生的事以 report 送給 client。Source 不讀任何檔案。

### 10.3 shard：每個目錄的名稱空間只有一個寫入者

**分配**：目錄 d 歸 `shard[hash(dir_key(d)) % N]` 管。manifest 依深度優先前進，相鄰的目錄會落在不同的 shard，所以不同的目錄可以同時進行。

**職責**：只有 shard 會改動它負責的目錄裡的名稱：建立、刪除、改名、link、mkdir，以及設定目錄自己的屬性。檔案的內容由 content workers 處理，它們不碰目錄的名稱空間。量測顯示，Linux 上的目錄鎖競爭就出在這裡：16 個 thread 在同一個目錄裡，sys 時間是 18.7 s；只剩一個寫入者時是 1.2 s（見 architecture.md）。

**目錄的生命週期**：

```text
Announced ──▶ Ready ──▶ Applied ──▶ Draining ──▶ Finishing ──▶ Done ──▶（通知父目錄的 shard）
   │            │          │                                   ▲
   │            │          └─ Failed（打不開、建立失敗）────────┘ 其中的 entry 一律失敗
   │            └─ 等待：父目錄的 shard 建好它，送來 handle
   └─ 父目錄的 page 列出它
```

- **Ready 的條件**：一種是目的地本來就有這個目錄，handle 由 compare lister 交來；另一種是父目錄的 shard 建好它之後，開啟 handle，以 `DirReady{dir, handle}` 送來。子目錄的 `DirectoryWork` 若比 `DirReady` 先到，就先排隊等。
- **Applied**：依序完成三件事：刪除多餘的項目（從比較時列出的那個 handle 刪除）、建立子目錄與其他非檔案的 entry、修正屬性。子目錄建好就送 `DirReady` 給它的 shard。
- **Draining**：要同時滿足三個條件，才能進入 Finishing：
  - 其中 want 的檔案都已經 committed 或 failed；
  - 子目錄都已經 Done；
  - 已經收到 `Left{dir}`。
- **Finishing**：依序設定 owner、mode、mtime，最後 fsync 這個目錄。為了進入目錄而暫時放寬的權限，在這裡還原，失敗時也一定還原（修正 DST-11）。
- **Done**：送 `ChildDone{parent}` 給父目錄的 shard。所以目錄自然由深到淺收尾。

**檔案的生命週期**：

```text
Wanted ──▶ Receiving ──▶ Written ──▶ Synced ──▶ Committed
   │           │             │          │
   └───────────┴─────────────┴──────────┴──▶ Failed（回報原因；pending file 被刪除）
```

**小檔的流程**：

1. decode worker 把一個 pack 依 `dir` 分組，各組交給對應的 shard。
2. shard 確認每個 entry 確實是它 want 的，取出 target，把「內容的切片加上 target 加上目錄 handle」交給 content worker。pack 解碼後的緩衝區以 `Arc` 共用，不複製。
3. content worker：
   - `Pending::create_in(dir)`：Linux 上是 `O_TMPFILE`，建立時不新增目錄項目，所以不必經過 shard。退回暫存名稱時（macOS、NFS），建立暫存名稱本身就是名稱操作，所以改由 shard 建立，再把 fd 交給 content worker；
   - 寫入；
   - 設定屬性：fchown、fchmod、futimens；
   - 交給 fsync pool。
4. fsync pool 做 `fsync`，把結果交還 shard。
5. shard 做 `commit(name)`：目的地原本沒有這個名稱時，用 `linkat`；原本就有時，先 link 到暫存名稱，再 `renameat` 蓋過去。然後回報 `Results`，並更新目錄的計數。

**大檔的流程**：

1. 第一個 segment 到達時，shard 建立 `LargeFile { pending, size, segments_total, done: bitmap, failed }`：`Pending::create_in(dir)` 之後 `ftruncate(size)`。
2. 每個 segment，由 shard 交給 content worker，`pwrite` 到它的 offset。不同的 segment 由不同的 worker 同時寫入。
3. 全部 segment 都完成時，shard 讓 content worker 設定屬性，再交給 fsync pool，最後由 shard commit。
4. 任何一段 `unread`，或 segment 帶的大小、mtime 和第一段不一致，整個檔案就失敗，pending file 被刪除。

**並行度依檔案系統調整**：

- 在本機檔案系統上，shard 對自己的目錄一次只做一個名稱操作。
- 在 NFS、Lustre、SMB 這類受延遲限制的檔案系統上（用 `statfs` 判斷），shard 對同一個目錄可以同時發出 K 個名稱操作，例如 8 個。這由 shard 內部的小型 worker 集合執行，仍然只有 shard 決定順序。

### 10.4 Destination 的結束

當以下四個條件都成立，Destination 才送出 `Done`：

- 收到 `ManifestEnd`；
- 所有 want 都已 committed 或 failed；
- 所有目錄都已 Done；
- 已呼叫 `make_durable`。在 macOS 上，這是對每個用到的 device 做一次 `F_FULLFSYNC`。

### 10.5 為 delta 與續傳留的路

- 每個 segment 帶自己的雜湊。這份「segment 雜湊清單」可以直接當成粗粒度的 delta 簽章：
  - Destination 對已經存在的大檔，先送出各段的雜湊；
  - Source 對雜湊相同的段只回「沿用」，Destination 再從舊檔複製該段（`copy_file_range`）。
  - 這只對就地修改的大檔有效，例如 VM 映像與資料庫檔案；要處理插入的資料，需要 rsync 的滾動 checksum，屬於下一步。
- **續傳**：大檔的 pending file 若改成有名稱的暫存檔，並在旁邊記下已驗證的 segment，以及 Source 的大小與 mtime，下次執行就只需要 want 缺少的 segment。這和 `O_TMPFILE`（沒有名稱）互斥，所以只對大檔使用有名稱的暫存檔。這是一個需要決定的選項（README 的 D16）。

---

## 11. 持久性與原子性

### 11.1 `Pending`

| 平台與情況 | 建立 | 就位 | 中途失敗或被終止 |
|---|---|---|---|
| Linux，檔案系統支援 `O_TMPFILE`（ext4、xfs、btrfs、tmpfs、f2fs） | `openat(dir, ".", O_TMPFILE\|O_WRONLY\|O_CLOEXEC, 0600)` | 名稱不存在：`linkat("/proc/self/fd/N", dir, name, AT_SYMLINK_FOLLOW)`。名稱已存在：先 link 到 `.name.ferry-XXXX`，再 `renameat` | 沒有名稱，最後一個 fd 關閉時就消失，`kill -9` 也一樣 |
| Linux，不支援的檔案系統（NFS、部分 FUSE），或沒有 `/proc` | `.name.ferry-XXXX`，`O_CREAT\|O_EXCL\|O_NOFOLLOW` | `renameat` | `Pending` 在 drop 時刪除；收到訊號時，由登記表刪除 |
| macOS | 同上 | 同上 | 同上 |

- 是否支援 `O_TMPFILE`，以 `st_dev` 為單位偵測一次並記住。
- `Pending` 是 L0 的型別：drop 時一定刪除自己，只有 `commit(name)` 會讓它就位，所以暫存檔殘留這類問題寫不出來（根除 DST-02）。

### 11.2 fsync 的順序

1. 檔案的內容與屬性寫完之後，由 fsync pool 做 `fsync`。fsync 只是在等待，不需要 CPU，所以 pool 開得比 CPU 數多，例如 64 條，讓檔案系統能把許多 fsync 合併成一次 journal commit。
2. macOS：一批檔案就位之前，對每個 device 設一次 `F_BARRIERFSYNC`，確保資料一定先於名稱寫入儲存媒體。
3. shard 做 commit（link 或 rename）。
4. 目錄 Finishing 時，fsync 這個目錄。
5. 整個傳輸結束時，macOS 再對每個 device 做一次 `F_FULLFSYNC`。
6. 根目錄若是這次執行才建立的，也要 fsync 它的父目錄（修正 DST-07）。

`--no-fsync` 跳過第 1、2、4、5 步。

### 11.3 屬性的順序

沿用現在的設計：

- 檔案：先 fchown（因為它會清掉 setuid 與 setgid），再 fchmod，最後 futimens。三者都在 commit 之前。
- 目錄：在 Finishing 時設定。

### 11.4 fd 預算

- 啟動時，把 `RLIMIT_NOFILE` 的軟上限調到硬上限。macOS 上，以 `kern.maxfilesperproc` 為上限。啟動子行程時，還原成原本的值。
- 預算分給以下幾類：
  - `DirCache`；
  - pending file：包括在途的小檔，以及正在寫入的大檔，每個 1 個 fd；
  - reader 開著的檔案；
  - transport。
- 每一類都有自己的上限。到達上限時，相應的 stage 就等，不會因為 `EMFILE` 而失敗（根除 DST-03、SRC-07）。

---

## 12. 錯誤、停止與斷線重連

### 12.1 錯誤分三類，由型別決定

| 類別 | 型別 | 例子 | 處理 |
|---|---|---|---|
| 單一 entry 的錯誤 | `EntryError` | 開不了、讀不了、stat 失敗、寫入失敗、雜湊不符、commit 失敗、Source 讀不到 | 回報 `Results.failed`，計入 failed，繼續。結束狀態為 23 |
| 權限造成的警告 | `Warning` | 非 root 時的 chown、group；FAT、SMB 上的 chmod、utimens；root_squash 下的 `EPERM` | 依種類彙總成一則警告，檔案照樣就位，不算 failed |
| 致命錯誤 | `Fatal` | 協定違規、manifest 檢查失敗、connection 無法恢復、根目錄不能用、`ENOSPC`、`EROFS`、內部錯誤 | 交給 supervisor 停止 |

L0 的每個 API 都回傳 `io::Result`。L4 依一張**錯誤碼對類別的表**轉換它，這張表只寫在一個地方，也有自己的測試。

### 12.2 supervisor 與 `Stop`

- 每個 server 有一個 `Stop`：
  - 它記下第一個 `Fatal`，也就是「原因」。
  - 「因為別人停止而失敗」的後果，型別是 `Stopped`，永遠不會被當成原因。
  - 它保證 `Error` 只送一次，而且 `Done` 絕不會在 `Error` 之後送出。
- 所有 stage 都由 `Stages::spawn(role, …)` 建立：
  - stage 回傳的錯誤，或發生的 panic（以 `catch_unwind` 接住），都會交給 `Stop`；
  - 每一個會阻塞的等待，都用 crossbeam 的 `select!` 同時等「工作」與 `stop.signal()`；
  - 阻塞中的 socket 讀寫，由 transport 以 `shutdown(2)` 喚醒。
- 收到訊號，或 server control 斷線、而且寬限時間已過時，沿用現在的 `interrupt` 路徑：移除登記的暫存檔，然後立刻結束。

### 12.3 結束狀態

| 碼 | 意義 |
|---|---|
| 0 | 成功 |
| 1 | 致命錯誤：準備或傳輸失敗 |
| 2 | 用法錯誤 |
| 3 | `ferry update --check`：有新版 |
| 23 | 部分 entry 無法複製或刪除 |
| 24 | 只是 Source 有 entry 在傳輸期間消失或讀不到 |
| 101 | Ferry 的內部錯誤 |
| 128 + n | 被訊號 n 中斷 |

### 12.4 斷線重連

現在任何一條 SSH 連線斷掉，整個傳輸就失敗。新的設計之所以能重連，是因為資料 block 是獨立、可以重送的：

1. **偵測**：heartbeat 逾時，或 path 沒有 `PathEnd` 就結束，transport 就把那條 SSH 連線上的所有 path 標為失效，並經由 server control 通知兩個 server：`PathsChanged{epoch}`。
2. **重連**：transport 先以 `BatchMode=yes` 重新連線。需要登入時，若有終端機就暫停並請使用者登入，沒有終端機就失敗。新的 path 以原本的 token 加入同一個 server。
3. **control 串流**：兩個方向各自從重送緩衝區中，把還沒被確認的訊息重送一次。對方依 `seq` 丟掉重複的。
4. **資料**：Source 把所有尚未確認的 block 依中繼資料重新讀取、重送。這個數量有上限，等於資料額度。若有 block 其實已經到了，Destination 依 BlockId 或 entry 的狀態丟掉重複的；這是可以接受的代價，換來的是不需要逐條 path 的確認協定。
5. **server 的存活**：server 不再綁在第一條 SSH 連線上。失去 server control 之後，它會等一段寬限時間，例如 120 秒，等 client 重新接上；逾時才結束並清理。

   限制：在設了 systemd-logind `KillUserProcesses=yes` 的 host 上，session 結束時 server 會被殺掉。這時重連失敗，行為和現在一樣；再執行一次，就只會傳缺少的部分。

---

## 13. 安全模型

### 13.1 威脅與對策

| 威脅 | 例子 | 對策 |
|---|---|---|
| 惡意或損壞的 peer | 拉取時，host 被入侵 | manifest 檢查：名稱、順序、EntryId 連續、目錄先宣告。block 檢查：只接受 want 過的 entry、`dir` 必須一致、長度有上限。codec 對每種訊息都有上限。持續 fuzz（第 14 節） |
| 目的地中的其他使用者 | 以 root 還原 `/home` 時，使用者在比較之後把檔案換成 symlink | L0 API 中沒有任何會跟隨 symlink 的呼叫（`Handle` 以 `O_PATH\|O_NOFOLLOW` 取得）。刪除只從列出的那個 handle 進行 |
| 使用者自己打錯 | `ferry sync ./site web1:`、兩邊的根目錄重疊 | plan 的規則（第 13.3 節） |
| 供應鏈 | release 的 asset 被換掉 | `SHA256SUMS` 以 minisign 簽章，公鑰編進 binary。上傳到 host 後再驗證一次。CI 的 action 以 commit SHA 固定 |
| 本機的其他使用者 | 搶先建立 `/tmp/ferry-<uid>` | 執行期的目錄放在 `$XDG_RUNTIME_DIR`，或用 `mkdtemp` 建立。path 傳給子行程，fd 一律設 close-on-exec |
| 密碼外洩 | sudo 的密碼 | 讀取後立即從環境變數移除，不傳給任何子行程 |

### 13.2 型別化的檔案系統 API（L0）

```rust
pub struct Name(Box<[u8]>);          // 非空；不含 '/'、NUL；不是 "." 或 ".."
pub struct Dir { fd: OwnedFd, shown: Arc<[u8]> }
pub struct Handle { fd: OwnedFd }    // O_PATH|O_NOFOLLOW（Linux）
pub struct Pending { /* fd、所在的 Dir、可能的暫存名稱 */ }

impl Dir {
    pub fn open_root(path: &Path) -> io::Result<Dir>;        // 唯一接受路徑的函式
    pub fn child(&self, name: &Name) -> io::Result<Dir>;     // O_DIRECTORY|O_NOFOLLOW
    pub fn list(&self) -> io::Result<Listing>;
    pub fn stat(&self, name: &Name) -> io::Result<Stat>;     // 不跟隨
    pub fn handle(&self, name: &Name) -> io::Result<Handle>;
    pub fn make_dir(&self, name: &Name, mode: u32) -> io::Result<()>;
    pub fn make_symlink(&self, name: &Name, target: &[u8]) -> io::Result<()>;
    pub fn remove(&self, name: &Name, kind: Kind) -> io::Result<()>;
    pub fn remove_tree(&self, name: &Name) -> io::Result<()>; // 不跟隨、不跨掛載點
    pub fn pending(&self) -> io::Result<Pending>;
}
impl Handle {
    pub fn set_owner(&self, …); pub fn set_mode(&self, …); pub fn set_times(&self, …);
}
impl Pending {
    pub fn file(&mut self) -> &mut File;
    pub fn commit(self, name: &Name, replace: bool) -> io::Result<()>;
}
impl Drop for Pending { /* 沒有 commit 就刪除 */ }
```

- `Path` 只出現在 `open_root`。所以「從根目錄以路徑重新解析」這件事，在型別上就做不到。
- `Handle` 的 `set_mode`：Linux 上經由 `/proc/self/fd/N`，或在 6.6 以上的 kernel 用 `fchmodat2(AT_SYMLINK_NOFOLLOW)`；macOS 上是 `fchmodat(AT_SYMLINK_NOFOLLOW)`，並先確認它不是 symlink。

### 13.3 plan 的安全規則

兩個 server 各自回報 root identity：

- kernel 的 `boot_id`，用來判斷兩邊是不是同一台機器上的同一個 kernel；
- 根目錄與每一層祖先的 `(dev, ino)` 鏈；
- canonical path；
- server 所代表的使用者的家目錄；
- 是否為網路檔案系統，以及它的 fsid。

client 依下表判斷。完整的規則與理由，見 CLI-01。

| 情況 | `sync` | `copy` |
|---|---|---|
| 兩邊是同一個目錄 | 拒絕 | 拒絕 |
| Destination 在 Source 裡 | 拒絕 | 拒絕 |
| Source 在 Destination 裡 | 拒絕 | 拒絕 |
| Destination 是 `/`、某個家目錄、家目錄的祖先，或空的遠端路徑 | 先詢問；沒有終端機時拒絕，除非給了明確的旗標 | 允許 |
| 預估刪除量超過 `--max-delete` | 傳輸中達到上限就停止（`Fatal`） | — |

---

## 14. 可觀測性與測試架構

### 14.1 可觀測性

- **metrics 由 runtime 自動產生。** stage 圖中的每條 channel 都記錄深度、等待時間與吞吐量；每個 stage 都記錄忙碌與等待的時間。不需要在程式碼各處手動標記，所以不會有漏標而失真的 stage（修正 ARC-05b）。
- **dashboard 直接讀 stage 圖**：顯示每個 stage 的忙碌程度、每條佇列的深度、最可能的瓶頸，以及每條 SSH 連線的速率與 heartbeat。
- **卡住的偵測看實質的進度**，例如每秒 committed 的 entry 數、確認的位元組數，不看 packet 數（修正 SRC-05）。超過 60 秒沒有進度，就印出每條 thread 在哪裡。
- 沿用 `--profile-file` 的 JSON 格式，加上 stage 圖的結構。

### 14.2 測試的層次

| 層次 | 對象 | 方法 |
|---|---|---|
| 單元測試 | L0 的每個 API；L3 的 codec；各 stage 的純邏輯，例如 applier 的合併、shard 的狀態機 | 一般的 `#[test]`。shard 的狀態機以合成的事件序列驅動 |
| property test | codec 來回編解碼；pack 與 segment 的切分；額度的會計 | proptest |
| fuzz | 解碼器；以任意 packet 串流驅動整個 Destination（搭配模擬的檔案系統） | cargo-fuzz。斷言絕不 panic，絕不碰根目錄以外的東西 |
| **故障注入的模擬** | 整條 pipeline，在單一 process 內執行 | 見下 |
| 整合測試 | 真的 binary、假的 ssh、假的 sudo、假的 getent | `tests/*.rs` |
| 端到端 | `ferry test` 的情境，在各種檔案系統、發行版、macOS 上 | CI 矩陣 |
| 規模與效能 | `ferry test --scale`；固定的效能基準 | CI 中定期執行，記錄趨勢 |

### 14.3 故障注入的模擬

- L0（檔案系統）、L2 的 path，以及時間（`Clock`），都定義成 trait。各有兩個實作：真實的，與模擬的。
- **模擬的檔案系統**在記憶體中，依一個種子產生故障排程，例如：
  - 第 n 次 `write` 回傳 `EIO`；
  - 第 m 次開檔回傳 `EMFILE`；
  - 在比較與套用之間把某個檔案換成 symlink 或 FIFO；
  - 讓目錄不能列出。
- **模擬的 path** 可以延遲、限速、在任意時間點斷線。
- 每次執行之後，檢查下列性質：
  - **完整**：沒有發生故障的 entry，目的地與來源一致。`sync` 時，目的地沒有多餘的項目，除了 Source 讀不到的。
  - **安全**：模擬的檔案系統記錄每一個操作，沒有任何一個落在根目錄之外，沒有任何一個跟隨了 symlink。
  - **原子**：任何時刻，最終名稱上的檔案不是舊版就是新版。
  - **乾淨**：結束後沒有 pending file，也沒有暫存名稱。
  - **誠實**：結束狀態與摘要符合實際發生的故障。
- 這不是完全確定的模擬：真實的 thread 排程無法重現。但故障排程可以由種子重現。CI 每次跑數百個種子，找到失敗時，回報那個種子。

---

## 15. 模組配置，以及從現在走過來的路

### 15.1 模組配置

```text
src/
  main.rs · lib.rs
  cli/            參數、plan 的呈現、結束狀態
  client/         prepare（第 6 節的狀態機）、plan、relay、ui（dashboard、progress）
  runtime/        graph、supervisor、stop、channel 的包裝、metrics
  fsx/            name、dir、handle、pending、cache、linux、macos、sim
  wire/           codec（有上限、自動分段）、messages、block
  transport/      path、frame、control_stream（seq、ack、重送）、data、
                  ssh、local、heartbeat、reconnect
  remote/         probe、install（驗證大小、雜湊與簽章）、release、sudo、shell
  source/         scan、manifest、known、read、pack、compress、send
  destination/    receive、compare、apply、shard、content、durable、finish
  profile/ · report/ · testkit/（含 sim 的故障排程）
```

可以先維持單一 crate。等 fuzz 與模擬的建置時間變成問題時，再把 `fsx`、`wire`、`runtime` 拆成獨立的 crate。

### 15.2 從現在走過來的路

不建議一次重寫。下面每一步都讓現有的 216 個測試與 `ferry test` 的情境保持通過，它們是驗證行為不變的依據。

| 步 | 做什麼 | 對應 | 得到什麼 |
|---|---|---|---|
| 1 | 先修 P0 與高風險的缺陷 | WP-01、WP-02、WP-03、WP-04 | 不再刪錯資料；fd 不再耗盡 |
| 2 | 引入 L0 的 `Name`、`Dir`、`Handle`、`Pending`，先用在 Destination，再用在 Source | DST-01、DST-02 的修法，加上 ARC-03 的 `DirCache` | 錯誤寫不出來；路徑不再重走 |
| 3 | 引入 L1 runtime：`Stop`、`Stages`、crossbeam | ARC-02 | 第一個錯誤全體停止；panic 不再停滯 |
| 4 | Destination 改成 applier 加 shard。先只有 1 個 shard，行為與現在相同；再增加到 N 個 | ARC-03，加上本文件的第 10.3 節 | 單一擁有者；目錄鎖不再競爭 |
| 5 | 寫入路徑改用 `Pending` 的 `O_TMPFILE`；fsync pool 獨立出來 | ARC-01 | 小檔的 sys 時間大幅下降 |
| 6 | 準備階段改成第 6 節的狀態機；plan 與 root identity | WP-01、WP-09 | 準備順序與設計一致 |
| 7 | **資料面改成 block**：pack 與 segment；Destination 改成 decode pool 加 shard 分派，拿掉 parser | ARC-04，加上本文件的第 7.3、9.4、10.3 節 | 大檔平行讀寫；沒有單一 parser |
| 8 | 控制面與資料面分開；reports 改走 server control；client 轉送 block 時不重排 | 本文件的第 5 節 | client 中繼時不用解讀內容 |
| 9 | 額度的流量控制 | 本文件的第 8 節 | 記憶體上限確定，取代各處手調的上限 |
| 10 | 斷線重連 | 本文件的第 12.4 節 | 長時間的傳輸能撐過網路中斷 |
| 11 | 故障注入的模擬與 fuzz，持續擴充 | 本文件的第 14.3 節 | 更強的測試保證 |

第 1 到 6 步，就是 [README](README.md) 中第一到第三波的工作包。第 7 步是唯一真正需要「重寫」的部分：資料面的格式。client 與 server 必須是同一個 build，所以改格式不必相容舊版，代價比一般專案小很多。

---

## 16. 開放問題

這些在實作到相關的那一步時，要先量測或先請使用者決定：

| # | 問題 | 怎麼決定 |
|---|---|---|
| Q1 | block 的大小 B：2 MiB、4 MiB 還是 8 MiB？小檔進 pack 的門檻？ | 量測壓縮率、記憶體與平行度 |
| Q2 | shard 的數量 N，以及網路檔案系統上同一目錄的並行數 K | 在 ext4、xfs、APFS、NFS 上量測 |
| Q3 | 資料額度的初始值與自動調整的方式 | 在高 RTT 的連線上量測 |
| Q4 | server 失去 server control 後的寬限時間；是否需要可以設定 | 依使用情境決定 |
| Q5 | control 串流要不要固定走某一條 path？（現在的設計是「所有 path 地位相同」） | 以實作的簡單程度與重連的行為比較 |
| Q6 | 兩側都在本機時，要不要讓兩個 peer 在 client 的 process 內執行，省掉兩次 process 之間的複製？ | 量測顯示現在的 relay 不是瓶頸，所以預設不做；本機 NVMe 對 NVMe 時再量一次 |
| Q7 | Linux 上的寫入 stage 是否改用 io_uring，把 `openat`、`write`、`fsync`、`linkat` 批次送出 | 在第 5 步之後量測，看還剩多少系統呼叫的成本 |
| Q8 | NFS 上 `(dev, ino)` 與 fsid 能否跨 client 比較，用來判斷根目錄是否重疊 | 在真的 NFS 上實測；不能時，列為已知的限制 |
| Q9 | 大檔續傳的暫存檔格式，以及旁邊記錄的位置 | 與 D16 一起決定 |
