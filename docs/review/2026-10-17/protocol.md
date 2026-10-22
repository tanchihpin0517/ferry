# 線路協定與訊息處理：待修項目交接文件

> 2026-10-22：已完成的項目（15 項）移到 [done/protocol.md](done/protocol.md)，這裡只留還沒完成或決定不做的。

這份文件交給之後動手修的 coding agent。每個項目都可以單獨閱讀，不必看過原本的審查。

## 範圍與基準

- **基準**：HEAD `3c17a2f`（`chore: version 0.14.4`）。文中所有 `path:line` 都已對照這個 commit 重新確認過。
- **範圍**：packet 與 frame 的格式（`src/protocol.rs`、`src/frame.rs`、`src/record.rs`、`src/manifest.rs`）、`src/mux.rs`、client 的轉送（`src/relay.rs`）、交握（`src/session.rs`）、server（`src/peer.rs`）、`src/control.rs`、`src/report.rs`，以及 `src/destination.rs` 如何解碼並處理收到的內容。
- **不在範圍內**：下列三項由 Source 的審查負責，本文件不重複：
  - Source 失敗後仍把整棵樹掃完；
  - 檔案在掃描後被換成 FIFO 時，Source 卡住；
  - Source 的 file sender 出錯時，不送 `Control::Error`。
- **協定變更**：client 與 server 永遠是同一個 build（見 `docs/design/architecture.md` 的「Installing on a remote host」），因此文中提到的協定變更都不需要與舊版相容。唯一例外是交握的第一則訊息，見 PRO-10。
- **嚴重度說明**：
  - 「高」：常見情況下就會讓指令失敗，或留下錯誤的結果。
  - 「中」：情況較少見，或後果較輕。
  - 「低」：防禦性問題，或只有 bug、惡意 host 才會觸發。
- **術語**：依照 `docs/design/*.zh-TW.md`。client、server、peer、Source、Destination、manifest、record、frame、packet、thread、stage、connection、host 與所有程式識別字都保留英文。

## 實驗用的 harness

[`materials/protocol/harness/`](materials/protocol/harness/) 是一個獨立的 crate（`Cargo.toml` 內有空的 `[workspace]`，並以 path 相依於 ferry）。它只呼叫 ferry 的 pub API：

- `destination::run`、`source::run`；
- `protocol::encode` / `encode_data`、`frame::encode`、`record::write_header` / `write_trailer`、`mux::DATA_PACKET`。

它自己組出 packet 串流，餵給 peer。

```text
# 在 repo 根目錄
CARGO_TARGET_DIR=$PWD/.scratch/review-proto/target cargo build \
  --manifest-path docs/review/2026-10-17/materials/protocol/harness/Cargo.toml
H=$PWD/.scratch/review-proto/target/debug/harness
$H $PWD/.scratch/hx                 # 依序跑情境 B、C、A、D
$H $PWD/.scratch/hx latency         # 情境 E
$H $PWD/.scratch/hx truncated       # 情境 G
$H $PWD/.scratch/hx fifo            # 情境 F（屬 Source 審查，本文件不處理）
```

注意：

- 第一個引數所指的目錄**會先被整個刪除**再重建，請只給專用的暫存目錄。
- 各項目會說明對應的情境能否改寫成回歸測試。原則上都可以移進 `src/destination.rs` 的 `mod tests`，沿用其中已有的 `run_on`、`decoded`、`tree` 等 helper。

端到端的實測使用同一個 target 目錄建出的 binary：在 repo 根目錄執行 `CARGO_TARGET_DIR=... cargo build`，得到 `.../target/debug/ferry`。

## 項目一覽

都已完成，移到 [`done/protocol.md`](done/protocol.md)。

---

## 較小的項目

以下各項篇幅較短，每項都可以單獨處理。

這一節的項目都已移到 [`done/protocol.md`](done/protocol.md)。
