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

| ID | 嚴重度 | 標題 |
|---|---|---|
| PRO-00 | 中 | Destination 沒有共享的「第一個錯誤與停止」機制（設計提案） |

---

### PRO-00：Destination 沒有共享的「第一個錯誤與停止」機制（設計提案）

- **嚴重度**：中。它是 PRO-04、PRO-05 的共同根因；那兩項的後果在各自的項目中說明。
- **驗證狀態**：已核對程式碼。症狀見 PRO-04、PRO-05 的實測。「`Control::Error` 送兩次」是讀程式碼確認的。
- **位置**（HEAD 3c17a2f）：
  - `src/destination.rs`
    - `:300-305`：`run` 開頭的 `reading_failed`。
    - `:334-358`：parser thread 依結果送 `Done` 或 `Error`。
    - `:377-387`：feeder thread。
    - `:388-413`：`read_packets` 結束後設 `reading_failed`，依 `read? → fed? → written? → pumped?` 的固定順序回傳。
    - `:505-634`：`read_packets` 的迴圈，沒有任何停止檢查。
    - `:702-770`：`take_compared`。
    - `:777-832`：`apply_extras`。
    - `:1895-1942`：`delete_extras`。
    - `:1348-1518`：`write_files`。
    - `:1738-1778`：`finish_as_done`、`finish_directories`。
  - `src/peer.rs:173-184`：`run` 回傳錯誤後，`serve` 再送一次 `Control::Error`。
  - `src/peer.rs:147-172`：outward thread 在 connection 已失敗時，把 peer 的 `Control::Error` 換成 connection 的原因。
  - `src/pool.rs:90-101`：`Submitter::submit` 的 "the workers stopped"。
  - `src/mux.rs:133-142`：`closed()` 與 `Mux::message`。
  - `src/report.rs:347-352`：`Reporter::close()`，「持有 `Mux` 的物件必須先 close，pump 才會結束」這個既有模式。
  - `src/interrupt.rs:62-96`、`:155-161`：訊號 thread 與 `stop`。
- **現象**：
  1. 一個 thread 致命失敗時，其他 thread 不會停，主 thread 仍繼續改動目的地（PRO-04）。
  2. 最後回報給使用者的，可能是後果而不是原因（PRO-05）。
  3. parser 失敗時，`Control::Error` 會送兩次：一次是 parser thread 經 mux 送出（`destination.rs:355`），一次是 `serve` 在 `run` 回傳後送出（`peer.rs:177`）。relay 只留第一個，第二個是雜訊。
- **根本原因**：Destination 有好幾種 thread，包括主 thread（`read_packets`，內含 compare thread 與 stat 池）、pump、解壓縮池、feeder、parser（`write_files` 加 16 個 writer 與 rename thread）、finisher。錯誤之間唯一的傳遞方式是「channel 被關掉，下一個人讀或寫時失敗」：
  1. 誰先失敗，要等別人下一次碰到被關掉的 channel 才會知道。
  2. 那個別人碰到的錯誤（"the workers stopped"、"read a record: failed to fill whole buffer"）看起來也像原因。
  3. 唯一區分因果的手段是 `reading_failed`。它只有一個方向（主 thread 失敗時，parser 不送 Error），而且是在 `read_packets` 回傳之後才設，會與 parser 的判斷互相競爭：

  ```rust
  // src/destination.rs:399-413
  // Known before the stream ends, which closing the frames does.
  if read.is_err() {
      reading_failed.store(true, Ordering::SeqCst);
  }
  drop(frames);
  drop(mux);
  let fed = feeding.join().expect("the frame feeder panicked");
  let written = files.map(|files| files.join().expect("the file writer panicked"));
  reporter.close();
  let pumped = pump.join().expect("the pump panicked");
  read?;
  fed?;
  written.transpose()?;
  pumped?;
  ```

  ```rust
  // src/destination.rs:348-358（parser thread）
  match &result {
      Ok(()) => mux.message(&Control::Done)?,
      Err(_) if reading_failed.load(Ordering::SeqCst) => {}
      Err(error) => mux.message(&Control::Error {
          message: format!("{error:#}"),
      })?,
  }
  ```

  ```rust
  // src/peer.rs:174-184（serve）
  let result = run(hello, &mut BufReader::new(peer_input), &mut writer);
  if let Err(error) = &result {
      let _ = protocol::send(
          &mut writer,
          &Control::Error {
              message: format!("{error:#}"),
          },
      );
      let _ = writer.flush();
  }
  ```

- **重現方式**：見 PRO-04（harness 情境 D）與 PRO-05（情境 A）。送兩次 Error 是讀程式碼確認的：parser 失敗時，`destination.rs:355` 送一次，`run` 回傳的 `written?` 又讓 `peer.rs:177` 送一次。
- **修法**（建議做法）：

  **1. 新增 `src/stop.rs`。** 讓 Source 的審查也能沿用。內容：

  ```rust
  pub struct Stop {
      stopped: AtomicBool,
      inner: Mutex<Inner>,
  }
  struct Inner {
      first: Option<anyhow::Error>, // 第一個「原因」
      told: bool,                   // 對方已經知道（送出 Error，或錯誤本來就來自對方）
      mux: Option<Mux>,             // 送那一則 Control::Error 用；close() 時交還
  }
  /// 只是別人失敗的後果，絕不是原因。
  #[derive(Debug)] pub struct Stopped;
  /// 已告知對方的第一個錯誤；Display 印出內層的 "{:#}"，source() 回傳 None。
  #[derive(Debug)] pub struct Reported(pub anyhow::Error);
  ```

  方法：

  - `fail(&self, error)`：
    - 若 `error.is::<Stopped>()`，什麼都不做；
    - 若已經有 `first`，什麼都不做；
    - 否則存入 `first`，在鎖內把 `stopped` 設為 true，並在鎖內呼叫 `mux.message(&Control::Error { message })`，然後設 `told`。
    - `Mux::message` 走無上限佇列，絕不阻塞，所以在鎖內送沒有問題。只有贏得 `first` 的那一次會送，因此保證只送一次。
  - `peer_failed(&self, error)`：用於收到 Source 的 `Control::Error`。記為原因，但不回送給 Source，`told = true`。
  - `is_stopped()`、`check() -> Result<()>`：停止後回傳 `Err(Stopped)`，供 thread 在改動任何東西之前檢查。
  - `done(&self) -> Result<()>`：在同一把鎖內，只有尚未停止時才送 `Control::Done`。這樣 `Done` 絕不會跟在 `Error` 之後。反過來，`Done` 之後若還有失敗，`Error` 仍照送，client 才知道。
  - `close(&self)`：取走 `mux`，讓 pump 能結束。之後若還有失敗，只記錄，不送。
  - `result(self) -> Result<()>`：
    - 沒有 `first` → `Ok`；
    - `told` → `Err(Reported(first).into())`；
    - 否則 → `Err(first)`，由 `serve` 送出。

  **2. 把「後果」型別化。** 不再靠時間先後判斷因果：

  - `pool::Submitter::submit` 的 "the workers stopped" 改回傳 `anyhow::Error::new(stop::Stopped)`；
  - `mux::closed()` 也改為 `Stopped`（pump 已不在時，原因由 pump 自己記錄，見下）；
  - parser 在 `stop.is_stopped()` 為真時讀到的 EOF，`ChannelReader` 或 `read_exact` 的 `UnexpectedEof`，都轉成 `Stopped`。

  **3. 誰呼叫 `fail`。** 規則：每個 thread 在「關掉或丟棄別的 thread 正在等的東西」之前，先把自己的錯誤記下來。

  - 主 thread：`run` 在 `drop(frames)` 之前執行 `if let Err(e) = read { stop.fail(e) }`。現在設 `reading_failed` 的位置剛好就是這裡。收到 Source 的 `Control::Error` 時，改呼叫 `peer_failed`。
  - feeder：`let bytes = match bytes { Ok(b) => b, Err(e) => { stop.fail(e); break } };`。要在 closure 結束、`data` 與 `decoded` 被丟棄之前記錄。
  - parser：把 `write_files` 的參數由 `ChannelReader` 改成 `&mut ChannelReader`，由呼叫端持有。呼叫端先 `stop.fail(error)`，再丟棄 stream。`finish_directories` 與 `make_durable` 的錯誤也在同一處 `fail`。
  - pump：包一層。`let r = mux::pump(writer, outlet); if let Err(e) = r { stop.fail(e); }`，因為 pump 自己的寫入錯誤才是 connection 這一側的原因。
  - finisher：錯誤經 `join` 回到 parser，由 parser `fail`。

  **4. 誰檢查（`stop.check()?` 或 `is_stopped()`）。**

  - `read_packets` 迴圈的開頭，以及 `while let Some(done) = compared.try_next()` 中每次 `take_compared` 之前。
  - `take_compared` 中，`apply_extras`（刪除）之前與每個 `apply_compared` 之前。
  - `delete_extras` 中，每個 `fsys::remove_tree` 之前。一個目錄可能有上百萬個多餘項目。
  - `Manifest::Root` 的 `apply_root` 之前。
  - compare thread：每次 `tree.part(...)` 之前。`Tree::part`（`src/compare.rs:138`）經由 `Tree::enter`（`:205-256`）在 `let_in` 時會改權限。
  - parser：每次 `record::read_header` 之前。writer 的 `write_file`：`interrupt::create` 之前若已停止，就不建暫存檔，只把該 record 讀完，讓 per-file channel 不卡住。

  **5. `run` 的收尾改成：**

  ```rust
  let read = read_packets(reader, &mux, /*...*/, stop);
  if let Err(error) = read { stop.fail(error); }   // 先記錄，再讓 stream 結束
  drop(frames);
  drop(mux);
  feeding.join().expect("the frame feeder panicked");      // 自己 fail，不回傳錯誤
  if let Some(files) = files { files.join().expect("the file writer panicked"); }
  stop.close();                                     // 與 reporter.close() 同理
  reporter.close();
  pump.join().expect("the pump panicked");          // 包裝後自己 fail
  stop.result()                                     // 回傳第一個原因
  ```

  **6. `serve` 只在錯誤不是 `Reported` 時送 `Control::Error`：**

  ```rust
  if let Err(error) = &result && !error.is::<stop::Reported>() { /* 現有的送出程式 */ }
  ```

  這樣每次失敗只送一則 Error。`peer.rs:153-168` 那段「connection 已失敗時換成 connection 原因」的替換照常有效，因為 Error 仍經過 outward thread。

  **與 mux 的關係：**

  - `Control::Error` 走 messages 佇列。它排在先前已入列的 results 之後，在 data 與 paced 之前。
  - `Stop` 持有一份 `Mux`，必須在 `pump.join()` 前 `close()`，否則 pump 永不結束。
  - 正卡在 `message_paced` 的 thread（dry run 的 listing）照常等 pump 消化，不需特別處理。

  **與 pool 的關係：**

  - 解壓縮池與 compare 的 stat 池的 worker 都不需要檢查 stop，它們在 submitter 被丟棄時自然結束。
  - stat 是唯讀的，跑完也無妨。
  - 只有 `submit` 的錯誤要型別化為 `Stopped`。

  **與 `interrupt` 的關係：**

  - 訊號（SIGINT、SIGTERM、SIGHUP）與 control connection 關閉，走的是 `interrupt` 的路：移除已登記的暫存檔，然後立刻 `process::exit`。`Stop` 處理的是行程內部的失敗，不取代這條路，`interrupt` 也不需要設定 `Stop`。
  - server 不使用 `interrupt::graceful`，所以 `Stop::check` 不必看 `interrupt::stop_asked()`。若日後 server 也採用 graceful 模式，可以在 `check` 裡一併判斷。
  - 暫存檔的清理見 PRO-02：`Registered` 改為 drop 時刪檔之後，停止路徑會在正常展開時刪掉暫存檔，訊號路徑仍由 `remove_all` 處理，兩者互不衝突。

  **邊界情況：**

  - (a) 主 thread 可能正阻塞在 `protocol::receive` 上，無法被旗標打斷。它要等下一個 packet 或 EOF 才會看到停止。Error 送出後 Source 會停下並關閉 connection（Source 審查會讓 Source 停得更快），所以會等到 EOF。
  - (b) 停止後，已寫完、已驗證、正在等批次重新命名的檔案，建議照常 `flush` 重新命名。它們依 overview 的規則已經完整且正確，可以減少下次執行的工作量。例外是失敗正是發生在 rename 或 barrier 本身，這時丟棄它們（由 PRO-02 的修法刪除暫存檔）。
  - (c) 被 `let_in` 改過權限的目錄（compare thread 為了能進入而替擁有者補上 rwx），現在失敗路徑上永遠不會收尾，會帶著多出的權限留下。停止後雖不再收尾一般目錄，仍要把這些目錄的權限還原。這是現有失敗路徑本來就有的問題（已核對程式碼：`write_files` 失敗時，`finishing.take()`，也不呼叫 `finish_directories`）。
  - (d) dry run 沒有 parser，也沒有 `Done`，只有主 thread 會 `fail`。

  **其他做法：**

  - 只把 `reading_failed` 改成雙向，例如加一個 `writing_failed`。比較簡單，但仍是靠時間先後判斷因果，也解決不了「主 thread 不停」與「送兩次 Error」，不建議。
  - 用 cancellation token 加 `select`：std 的 mpsc 沒有 select，需要大改，不建議。
- **要加的測試**：
  - `src/stop.rs` 的 `mod tests`（單元測試）：
    - 先後兩次 `fail`，只留第一個，也只送一則 `Control::Error`（把 `mux::channel()` 的 outlet 交給 `mux::pump` 寫進 `Vec`）；
    - `Stopped` 不會成為原因；
    - 停止後 `done()` 不送 `Done`；
    - `result()` 在 `told` 時回傳 `Reported`。
  - `src/destination.rs` 的 `mod tests`：見 PRO-04、PRO-05 的測試。另外，對現有的 `refused`（`:2288-2321`）與 `a_source_that_fails_stops_the_destination`（`:2591-2622`）補一個斷言：輸出中至多一則 `Control::Error`；收到 Source 的錯誤時一則都沒有。
- **驗收條件**：
  - [ ] 任一 thread 失敗後，主 thread 在下一個 packet 之前停止，不再建立、刪除或改權限（PRO-04 的測試通過）。
  - [ ] `run` 回傳的是第一個原因，不是 "the workers stopped" 或 EOF 類錯誤（PRO-05 的測試通過）。
  - [ ] 一次失敗，Destination 的輸出（含 `serve`）恰好一則 `Control::Error`；錯誤來自 Source 時為零則。
  - [ ] `Done` 絕不出現在 `Error` 之後。
  - [ ] 被 `let_in` 的目錄在失敗後權限已還原。
  - [ ] `reading_failed` 已移除；`cargo test` 全部通過。
- **文件要同步的地方**：
  - `docs/design/pipeline.md`「Principles」與 `pipeline.zh-TW.md`「原則」：加一條「一個 peer 的任一 stage 失敗，整個 peer 立刻停下：只回報第一個原因，只送一次，之後不再改動目的地」。
  - `docs/design/architecture.md`「Transport」中「No message goes first…the side that sent it stops either way」那段，與 `architecture.zh-TW.md`「Transport」的對應段落：補充 peer 端如何保證只送一次、且送出後即停止。
- **相關項目**：PRO-04、PRO-05（由本項修好）、PRO-02（停止時清理暫存檔）、PRO-07（若新增 apply thread，它也要 `fail` 與 `check`）、PRO-08（EOF 的分類）。Source 審查的「Source 失敗後仍掃完整棵樹」可以沿用同一個 `Stop`。

---

> 2026-10-20 補記：CI 上兩次（`5ac5ab4` 的 Ubuntu、`0efc4a3` 的 Rocky Linux 8）出現 `a_manifest_with_another_root_or_ids_out_of_turn_is_refused` 失敗，回報的是「the record stream ended before its end」，不是不合法的 manifest。原因：主 thread 在 `receive` 回傳錯誤時，`frames` 隨之丟棄，parser 讀到 EOF；這發生在主 thread 呼叫 `stop.fail` 之前，parser 就把 EOF 當成原因。修正：`receive` 先把錯誤交給 `stop`，才丟棄 `frames` 與 `parts_in`。本機以 8 個並行跑 320 次，修正前後都沒有重現，所以這個修正只依推理，沒有測試證明。

## 較小的項目

以下各項篇幅較短，每項都可以單獨處理。
