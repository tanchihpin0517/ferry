# 線路協定與訊息處理：待修項目交接文件

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
| PRO-01 | 高 | `Needs::Deletes` 一個目錄只送一則訊息，超過 16 MiB 時 `sync` 先刪光再失敗 |
| PRO-02 | 高 | record 串流中途中斷時，暫存檔留在 Destination |
| PRO-03 | 中 | manifest 的目錄 part 只依項目數切分，symlink target 很長時 Source 直接失敗 |
| PRO-04 | 中 | parser 已致命失敗，主 thread 仍繼續套用 manifest，包括 `sync` 的刪除 |
| PRO-05 | 中 | 真正的失敗原因被 "the workers stopped" 這類後果蓋掉 |
| PRO-06 | 中 | 需要的檔案始終沒送來，也沒被回報，傳輸仍算成功 |
| PRO-07 | 中 | 比較完的目錄要等下一個 packet 到了才套用、才送出 needs |
| PRO-08 | 低 | Destination 把 manifest 結束前的 EOF 當成正常結束 |
| PRO-09 | 低 | client 不檢查 server 送來的 Metrics 狀態編號，超出範圍就 panic；Profile report 無上限累積 |
| PRO-10 | 低 | 交握在比對 build 之前就解碼整個 `Hello`，布局不同時看不到 build 不符的說明 |
| PRO-11 | 低 | `Needs::Through(u64::MAX)` 讓 `Known::compared_through` 溢位 |
| PRO-12 | 低 | 同一 ID 的第二個 record 在第一個還沒 settled 時被接受 |
| PRO-13 | 低 | `frame::Assembler` 每個 frame 都重新長大緩衝區，frame 帶著過大的容量 |
| PRO-14 | 低 | relay 遇到無法解碼的 report 時記下失敗，卻讓整個傳輸跑完 |
| PRO-15 | 低 | striped 接收端的 early piece 以個數限制，而不是以位元組 |

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

### PRO-01：`Needs::Deletes` 一個目錄只送一則訊息，超過 16 MiB 時 `sync` 先刪光再失敗

- **嚴重度**：高
- **驗證狀態**：已實測重現。
  - 用 debug build 建立 `dst/d/`，內含 70,000 個空檔，檔名各 250 bytes；`src/d/` 為空目錄。
  - `ferry sync --dry-run src/ dst/`：結束狀態 1，stderr 為 `ferry: the Destination on this machine failed: a Needs packet is too large`；stdout 已列出 70,000 行。
  - `ferry sync src/ dst/`：同一個錯誤，結束狀態 1，而 `dst/d/` 內 70,000 個檔案已全部刪除。
- **位置**：
  - `src/destination.rs`
    - `:702-761`：`take_compared` 把整個目錄的 `deletes` 一次交給 `send_needs`（`:761`）。
    - `:777-832`：`apply_extras`。把每個名字接成完整路徑；非 dry run 時先刪除（`:828`）再回傳。
    - `:678-693`：`send_needs`，`mux.message(&Needs::Deletes(deletes))?`（`:691`）。
  - `src/protocol.rs`
    - `:16-18`：`MAX_PAYLOAD`。
    - `:116-132`：`enum Needs`，`Deletes(Vec<(Vec<u8>, bool)>)` 在 `:126`。
    - `:179-189`：`packet()` 拒絕過大的 payload。
  - `src/source.rs:371`：Source 收到 `Needs::Deletes(_)` 時什麼都不做。
  - `src/destination.rs:2130-2182`：測試 `a_directory_in_parts_deletes_only_what_no_part_has` 依靠這則訊息來斷言找到哪些多餘項目。
- **現象**：
  - 目的地有某個目錄，其中 Source 沒有的項目夠多（250 bytes 的檔名約 65,800 個；30 bytes 左右的路徑約 50 萬個），`ferry sync` 就會失敗。
  - 非 dry run 時，失敗前已經把那些項目全刪了；之後的目錄都沒處理，必須再跑一次。
  - `ferry sync --dry-run` 對這樣的樹永遠失敗。
- **根本原因**：
  1. 多餘項目以「整個目錄」為單位算出。`apply_extras` 把每個名字接成完整路徑，postcard 編碼後每項為 varint 長度 + 路徑 + 1 byte 的 bool，全部放進同一個 `Vec`。
  2. `take_compared` 把它整個交給 `send_needs`，編成一則 `Needs::Deletes`。
  3. `protocol::packet()` 拒絕超過 `MAX_PAYLOAD` 的 payload，這個錯誤一路回到 `read_packets`，Destination 停止。
  4. 非 dry run 時，刪除（`:828`）發生在送出（`:761`）之前。
  5. 而 Source 根本不使用這則訊息（`source.rs:371`）。

  ```rust
  // src/destination.rs:758-761
  for done in waiting.drain(..) {
      apply_compared(done, mux, root, state, options, reporter)?;
  }
  send_needs(mux, Vec::new(), Vec::new(), deletes)?;
  ```

  ```rust
  // src/protocol.rs:179-183
  fn packet(kind: PacketType, payload: &[u8]) -> Result<Vec<u8>> {
      let length = u32::try_from(payload.len())
          .ok()
          .filter(|&length| length <= MAX_PAYLOAD)
          .with_context(|| format!("a {kind:?} packet is too large"))?;
  ```

  dry run 列出的「deleting …」是走 `reporter.listed`，有以 1 MiB 分段（`src/report.rs:32`、`:289-302`），所以沒問題。出問題的只有這則送給 Source 的訊息。
- **重現方式**：

  ```text
  B=/path/to/scratch/exp; mkdir -p $B/src/d $B/dst/d
  python3 -I -c "
  import os
  for i in range(70000):
      open(os.path.join('$B/dst/d', '%06d' % i + 'x' * 244), 'w').close()
  "
  ferry sync --dry-run $B/src/ $B/dst/ > /dev/null; echo $?   # 1，a Needs packet is too large
  ferry sync $B/src/ $B/dst/; echo $?; ls $B/dst/d | wc -l     # 1，然後 0
  ```

- **修法**：
  - **做法 A（建議）：移除 `Needs::Deletes`。**
    - 刪掉 `protocol.rs:126` 的變體、`send_needs` 的 `deletes` 參數與 `:690-692`，以及 `source.rs:371` 的分支。
    - `apply_extras` 只在 dry run 需要完整路徑來列出；非 dry run 的 `delete_extras` 其實只用名字（`:1909-1913` 又把目錄前綴切掉），可以順便改成直接傳名字。
    - 協定變更不需相容（同一個 build）。
  - **做法 B：保留，但依位元組分段。** 例如在 `send_needs` 中以 `report.rs` 的 `LISTED_BYTES`（1 MiB）同樣的方式切成多則 `Needs::Deletes`。只有在 Source 將來需要知道刪了什麼時才值得，例如做摘要或 itemize 輸出。若選 B，Source 端每則都照原樣處理即可。
  - **做法 C：調高 `MAX_PAYLOAD`。** 不建議。它隨目錄大小無上限，而且 striped 接收端的最壞記憶體會跟著變大（見 PRO-15）。
  - 不論選哪個：Destination 的「一則訊息不得隨目錄長大」原則（pipeline.md §1）應明確適用於 needs。建議在 `protocol.rs` 加一段註解列出所有會隨輸入長大的訊息，以及各自的上限：manifest part 見 PRO-03；`Needs::Files` / `Attributes` 每則至多一個 part 的 ID；`Report::Listed` 1 MiB。
  - 測試 `a_directory_in_parts_deletes_only_what_no_part_has`（`:2130-2182`）：
    - 若選 A，改成在執行後檢查目的地的樹（`tree()` helper，`:2324`），確認只刪了 `extra`；
    - 若選 B，改成蒐集所有分段再比較。
- **要加的測試**：
  - 若選 A：上面改寫的單元測試即可，另外刪掉對 `Needs::Deletes` 的所有引用（`cargo build` 會指出）。
  - 若選 B：在 `send_needs` 或新的分段函式上做單元測試，分段上限比照 `mux::PACED_BYTES` 用 `cfg!(test)` 縮小，斷言每則編碼後不超過上限、串起來與原清單相同。
  - 兩者都建議在 `tests/sync.rs` 加一個整合測試：目的地某個目錄有約 70,000 個長檔名的多餘檔案，`ferry sync --dry-run` 與 `ferry sync` 都成功，後者刪光那些檔案。建檔約需數秒；若嫌慢，可只在 `ferry test` 的實際規模情境（`src/testkit/scale.rs`）中涵蓋。
  - harness 沒有對應情境，用上面的 shell 重現即可。
- **驗收條件**：
  - [ ] 上述 70,000 個長檔名的情境，`sync --dry-run` 與 `sync` 都成功，結束狀態 0。
  - [ ] 程式中不再有任何一則訊息的大小會隨單一目錄的多餘項目數無上限成長。
  - [ ] `cargo test` 通過，包括改寫後的 `a_directory_in_parts_deletes_only_what_no_part_has`。
- **文件要同步的地方**：
  - 若選 A：`docs/design/architecture.md`「Packets on one connection」表格的 needs 一列不必改，但可以確認文中沒有提到「Destination 把刪除告訴 Source」；`architecture.zh-TW.md`「一條 connection 上的 packet」同理。
  - 若選 B：在 `docs/design/pipeline.md` §2 Compare 與 `pipeline.zh-TW.md` §2 比較中說明 sync 的刪除清單分段送出。
- **相關項目**：PRO-03（同一個 16 MiB 上限，發生在 manifest）、PRO-15。

> 2026-10-18 補記：WP-08 照做法 A 修正。`Needs::Deletes` 已移除；`delete_extras` 也照建議改成直接收名字，只在顯示與回報 `Results::NotDeleted` 時才接成完整路徑。`a_directory_in_parts_deletes_only_what_no_part_has` 改成非 dry run，執行後檢查目的地的樹；為了讓兩段的檔案都不需要傳輸，測試先把目的地檔案的 mtime 與權限設成和 manifest 相同。整合測試放在 `tests/sync.rs`（`deletes_more_extras_in_one_directory_than_a_message_could_name`），debug build 約 5 秒，沒有移到 `ferry test` 的規模情境。改回舊的程式碼，這個測試會以 `a Needs packet is too large` 失敗。`protocol.rs` 的 `MAX_PAYLOAD` 註解列出各種訊息的上限。

> 2026-10-20 補記：上述整合測試在 CI 的 ext4-casefold 上逾時：180 秒內連 70,000 個檔案都還沒建完，ferry 還沒開始執行；本機 kernel 5.15 的 casefold 上同樣只要約 20 秒。macOS 上也要 33–60 秒。與上面的建議不同，改為新增只在 debug build 生效的 `FERRY_TEST_MAX_PAYLOAD`（`inject.rs`），測試把封包上限設成 2 MiB，多餘檔案從 70,000 個降為 9,000 個（共約 2.2 MB，超過一則訊息），本機約 0.75 秒。舊的程式碼在這個上限下，同樣會因為 `Needs::Deletes` 超過上限而失敗。這個上限不能低於 2 MiB：dry run 每則列表報告最多 1 MiB，manifest part 最多 4 MiB。另外發現：報告太大而無法編碼時，`Reporter::send_open` 會默默丟掉它，傳輸仍然成功；把上限設成 100 KB 時，dry run 以結束狀態 0 結束，卻一行都沒列出。在真正的 16 MiB 上限下，這不會發生，所以這次沒有修改，只記在這裡。另外新增單元測試 `no_message_grows_with_the_extras_deleted`（`destination/tests.rs`）：在同一個行程裡跑 Destination，分別同步 100 個與 2,000 個多餘檔案，斷言它送出的最大封包一樣大。在修正之前的程式上，它以 25,502 對 510,003 bytes 失敗。

---

### PRO-02：record 串流中途中斷時，暫存檔留在 Destination

（與 [DST-02](destination.md) 是同一個問題。修法以 DST-02 為準，兩者一起做；本項補充了端到端的實測，以及 `docs/cli.md` 要同步的地方。）

- **嚴重度**：高
- **驗證狀態**：已實測重現，兩種方式。
  1. **端到端，真的 binary。**
     - 來源：100 個小檔，加一個 8 GiB 的 sparse 檔 `zbig`。
     - 執行 `ferry copy --no-compress src/ dst/`，3 秒後用 `kill -9` 殺掉 Source 的 `ferry server`（模擬 connection 中斷或 Source 當機）。
     - client 印出 `ferry: the Source on this machine: a path ended before its end`，結束狀態 1。
     - `dst/` 裡留下 97 個 `.ferry-` 暫存檔，其中 `.zbig.ferry-…` 有 3,774,611,456 bytes；沒有任何一個檔案在最終名稱上。
  2. **harness 情境 G（`truncated`）。**
     - 先送 50 個完整的小 record，再送一個 1 MiB 的 record，只送 header 與前 100 KiB，然後 EOF。
     - `run` 回傳 `read the record stream: failed to fill whole buffer`，目錄裡留下 51 個暫存檔，0 個最終名稱。
- **位置**：
  - `src/destination.rs`
    - `:1627-1672`：`write_file`。讀串流的兩處 `?`（`:1655-1657` 的 `read_exact`，`:1666` 的 `read_trailer`）提早回傳，丟棄 `opened`（內含暫存檔名與 `Registered`），但沒有刪檔。
    - `:1252-1272`：`Ready` 以 `_registered` 持有登記，註解寫著 "Unregistered when dropped: once renamed into place, or removed"。
    - `:1348-1518`：`write_files` 以 `?` 提早回傳時，`batch: Vec<Batched>`（已寫完、等著整批重新命名的檔案）與 writer 尚未交回的 `Written::Ready` 一併被丟棄。
    - `:1525-1535`：`rename_batch` 的 barrier 失敗以 `?` 回傳，整批被丟棄（只在 macOS 有實際的 barrier）。
  - `src/interrupt.rs:188-198`：`Registered` 的 `Drop` 只取消登記，不刪檔。`:225-236` 的 `remove_all` 只在訊號或 `interrupt::stop` 時執行（`:93-97`、`:155-161`）。
  - `src/peer.rs:55-63`、`:106-114`：control connection 關閉或 heartbeat 中斷時，只有在 `!DONE` 才呼叫 `interrupt::stop`。`:198` 在 `run` 回傳後設 `DONE`，所以一般的失敗路徑是正常回傳、正常結束，不清理。
  - `src/cli.rs:378-389`：server 失敗時直接以 `ExitCode::FAILURE` 結束。
- **現象**：
  - 傳輸在檔案傳到一半時失敗，任何原因都算：connection 中斷、Source 失敗、Destination 的其他 stage 失敗。之後，目的地留下 `.名稱.ferry-XXXXXXXXXXXXXXXX` 暫存檔。
  - 被留下的包括正在寫的那個（可能有數 GB 的部分內容），以及所有已寫完、已驗證、但還在等批次（`BATCH = 256`）重新命名的檔案。
  - `ferry copy` 重跑永遠不會刪掉它們，它們只是目的地多出的檔案；`ferry sync` 重跑才會當成多餘項目刪除。
  - 這違反 `docs/design/overview.md`「Every file is complete, correct, and durable」第 1 點：「When a run is interrupted, the Destination deletes the temporary files of files it did not finish. Only a crash or a killed process can leave one.」上面的實測中被殺的是 Source，Destination 本身既沒當機也沒被殺。
- **根本原因**：暫存檔的生命週期沒有綁在擁有它的值上。
  1. `interrupt::create` 回傳一個 `Registered`，它 drop 時只把檔名從 `TEMPORARY` 表中拿掉，不刪檔：

     ```rust
     // src/interrupt.rs:188-198
     pub struct Registered(u64);

     impl Drop for Registered {
         fn drop(&mut self) {
             if let Some(files) = TEMPORARY.lock().expect("not poisoned").as_mut() {
                 files.remove(&self.0);
             }
         }
     }
     ```

  2. `write_file` 只在走到函式結尾時才刪檔。讀串流失敗時是以 `?` 直接返回：

     ```rust
     // src/destination.rs:1653-1666
     while remaining > 0 {
         let count = buffer.len().min(remaining as usize);
         stream
             .read_exact(&mut buffer[..count])
             .context("read the record stream")?;   // 返回，暫存檔留著
         hasher.update(&buffer[..count]);
         /* 寫入 */
         remaining -= count as u64;
     }
     let trailer = record::read_trailer(stream)?;    // 同上
     ```

  3. 串流在 record 中間結束時（主 thread 收到 EOF 或錯誤，`drop(frames)`，feeder 結束，parser 的 `ChannelReader` 回傳 0），parser 的 `forward_record` 失敗。parser 返回，丟棄 per-file 的 `forward`，writer 的 `read_exact` 失敗，暫存檔留下。
  4. 同時 `write_files` 的 `?` 讓 `batch` 與尚未取回的 outcomes 被丟棄。每個 `Ready` 的 `_registered` 隨之取消登記，檔案留下。
  5. server 接著正常回傳（`DONE = true`），不會執行 `remove_all`。
- **重現方式**：
  - 端到端（Linux，需要 `/proc`）：

    ```text
    B=/path/to/scratch/e2e; F=/path/to/target/debug/ferry
    mkdir -p $B/src; for i in $(seq -w 1 100); do echo "file $i" > $B/src/a$i; done
    truncate -s 8G $B/src/zbig
    ($F copy --no-compress $B/src/ $B/dst/ 2> $B/err.txt; echo "exit=$?" >> $B/err.txt) &
    sleep 3
    for p in /proc/[0-9]*; do
      c=$(tr '\0' ' ' < $p/cmdline 2>/dev/null)
      case "$c" in *"ferry server"*)
        ls -l $p/fd 2>/dev/null | grep -q "$B/src" && kill -9 ${p#/proc/};;
      esac
    done
    sleep 3; cat $B/err.txt; ls -a $B/dst | grep -c ferry-     # 數十個暫存檔
    ```

    實測後記得刪掉 `$B`，其中會有數 GB 的部分檔案。
  - harness：`$H /path/to/scratch/hx truncated`。可以直接改寫成單元測試。
- **修法**（建議：讓 `Registered` 在 drop 時刪檔，只有確定重新命名成功才「放手」）：
  1. 在 `src/interrupt.rs` 中：
     - `Registered::drop` 改為：在 `TEMPORARY` 表中移除這一筆；若還在，對其 `(directory, name)` 執行 `unlinkat`，錯誤一律忽略（`ENOENT` 是正常的）。
     - 新增 `pub fn renamed(self)`：只取消登記、不刪檔。內部以 flag 或 `std::mem::forget` 搭配手動移除實作。
     - 不需要拿 `GATE`。`remove_all` 會先 `take()` 整張表，之後的 drop 找不到項目就什麼都不做；兩邊都嘗試 unlink 也無害。
  2. 在 `src/destination.rs` 中：
     - `rename_into_place` 成功後呼叫 `ready._registered.renamed()`。需要把 `Ready` 以值的方式交進去，或改為回傳後由 `flush` 呼叫。
     - `apply_entry` 中 symlink 與特殊檔的暫存名稱（`:1163-1211`）：重新命名成功後呼叫 `registered.renamed()`，失敗時照舊 `fsys::remove`，再 drop（重複 unlink 無害）。
     - `write_file` 不必再在結尾自己刪檔：失敗時 `opened` 被丟棄就會刪。保留現有的顯式刪除也可以。
  3. 有了 1 與 2，`write_file` 的 `?`、`write_files` 的 `?`、`rename_batch` 的 `?`、writer 交不回的 outcome 都會自動清理，不必逐一修改。
  4. 若同時實作 PRO-00，停止時建議照常把已寫完的 `batch` 重新命名（見 PRO-00 的邊界情況 (b)）；沒有 PRO-00 時，本修法會把它們刪除，這也符合 overview 的保證。

  其他做法：

  - 在 `destination.rs` 另寫一個 `Temporary` guard，持有 `(OwnedFd, Vec<u8>, Registered)`。效果一樣，但 `apply_entry` 的暫存名稱要另外處理。
  - 每個 `?` 改成手動清理：容易漏，不建議。

  不在本項範圍：Destination 自己當機或被 `kill -9` 時留下的暫存檔仍無法清理。這是 overview 允許的；若要清，得辨識 `.名稱.ferry-` 加 16 位 hex 的檔名，有誤刪使用者檔案的風險，另案討論。
- **要加的測試**：
  - `src/destination.rs` 的 `mod tests`：把 harness 的 `truncated` 改寫成測試。50 個完整小 record 加一個被截斷的大 record，`run` 回傳錯誤後，目的地沒有任何名字含 `.ferry-`。另一個變體：完整 record 之後接一個未要求的 ID（parser 致命失敗），斷言同樣乾淨。
  - `tests/copy.rs`（整合測試，Linux）：仿照 `an_interrupted_copy_leaves_no_temporary_files`（`tests/copy.rs:205-253`），等暫存檔出現後，從 `/proc` 找出開著來源目錄的 `ferry server` 並 `kill -9`，斷言 client 失敗，且 20 秒內目的地沒有 `.ferry-`。
  - `src/interrupt.rs`：`Registered` drop 會刪檔、`renamed()` 不會。
- **驗收條件**：
  - [ ] 上述端到端重現跑完，目的地沒有任何 `.ferry-` 檔名。
  - [ ] harness 的 `truncated` 改寫的單元測試通過。
  - [ ] Ctrl-C 的既有測試（`tests/copy.rs:205`）仍通過。
  - [ ] 成功的傳輸不會誤刪已重新命名的檔案（既有的 copy 與 sync 測試全過）。
- **文件要同步的地方**：
  - `docs/design/overview.md`「Every file is complete, correct, and durable」第 1 點，與 `overview.zh-TW.md`「每個檔案都完整正確並持久保存」第 1 點：把「執行中斷時」擴大為「執行中斷或失敗時」。
  - `docs/cli.md`「Synopsis」中 "On Ctrl-C, or `SIGTERM` or `SIGHUP`, the temporary files … are removed" 那段（約 `:114`），與 `cli.zh-TW.md`「概要」的對應段落（`:54`）：補上「傳輸失敗時也一樣」。
- **相關項目**：PRO-00（停止時已驗證的檔案怎麼處理）、PRO-08（EOF 正是造成串流截斷的路徑之一）。

---

### PRO-03：manifest 的目錄 part 只依項目數切分，symlink target 很長時 Source 直接失敗

- **嚴重度**：中
- **驗證狀態**：已實測重現。在一個目錄裡建立 10,000 個 symlink，各指向 2,000 bytes 的 target；`ferry copy --dry-run src/ dst/` 結束狀態 1，訊息為 `ferry: the Source on this machine failed: a Manifest packet is too large`。
- **位置**：
  - `src/scan.rs`
    - `:37`：`PART = 10_000`。
    - `:172-205`：`list()` 依名字數量切 part。
    - `:290-330`：`send_found` 把 stat 完的 part 原樣交給 `sink.manifest(Manifest::Directory { … })`。
  - `src/source.rs:493-524`：`ManifestSink::manifest` 以 `message_paced` 送出。
  - `src/mux.rs:148-149`：`message_paced` 先 `protocol::encode`。
  - `src/protocol.rs:16-18`、`:179-189`：16 MiB 上限。
  - `src/manifest.rs:17-50`：`Entry` 與 `Kind::Symlink { target }`。
- **現象**：
  - 一個目錄的 10,000 個項目，平均編碼大小超過約 1,678 bytes 時，`copy`、`sync`，以及 dry run，都會每次失敗。實際上只有 symlink target 能做到：Linux 的 `NAME_MAX` 是 255，target 最長可到 4,095。
  - 使用者沒有任何方法避開：`part_entries` 只是測試用的 `Hello` 欄位。
- **根本原因**：part 的大小只以項目數限制，而項目本身可能很大。最壞情況是 10,000 × (255 + 4,095 + 約 80) ≈ 44 MB，超過 `MAX_PAYLOAD`。`protocol::packet()` 拒絕後，`ManifestSink::manifest` 回傳錯誤，掃描失敗。

  ```rust
  // src/scan.rs:319-330
  for entry in &mut entries {
      entry.id = next_id;
      next_id += 1;
      if entry.kind == Kind::Directory {
          expected.insert(manifest::join(&path, &entry.name));
      }
  }
  sink.manifest(Manifest::Directory {
      path: path.to_vec(),
      entries,
      more: !last,
  })?;
  ```

- **重現方式**：

  ```text
  B=/path/to/scratch/exp2; mkdir -p $B/src
  python3 -I -c "
  import os
  t = '/' + 't' * 1999
  for i in range(10000):
      os.symlink(t, '$B/src/l%05d' % i)
  "
  ferry copy --dry-run $B/src/ $B/dst/; echo $?   # 1，a Manifest packet is too large
  ```

- **修法**（建議：在 `send_found` 依位元組再切一次）：
  - 在 `src/scan.rs` 新增 `PART_BYTES`，例如 4 MiB，遠低於 16 MiB；測試時可比照 `mux::PACED_BYTES` 用 `cfg!(test)` 縮小。
  - 每個 entry 的編碼大小以上界估算：`name.len() + target.len() + 80`。固定欄位都是 varint：id、size、mtime、mode、uid、gid、rdev，加上變體與長度前綴，合計不超過 80 bytes。
  - `send_found` 在指定 ID 之後，把 `entries` 依累計上界切成數段，每段送一則 `Manifest::Directory`。前面各段 `more: true`，最後一段 `more: !last`。
  - 一段至少要有一個項目，不要產生空的中間段。單一項目不可能超過上限（最大約 4.4 KB）。
  - Destination 的 `manifest::Check::directory`（`src/manifest.rs:141-201`）本來就接受同一目錄連續的多個 part，ID 也是連續的，不需改動。Source 的 `Known::record` 每則訊息記一段，也沒問題。`ManifestSink` 為每段各自送 `Manifest::Names` 也照常。
  - 這只改變 part 的邊界，不改線路格式，所以不涉及相容性。
  - 其他做法：把 `MAX_PAYLOAD` 調到 64 MiB。最簡單，但每個 packet 的配置與 striped 接收端的最壞記憶體（PRO-15）都會變大，不建議。
- **要加的測試**：
  - `src/scan.rs` 的 `mod tests`（單元測試）：在暫存目錄建數百個 target 為 2 KB 的 symlink，以縮小的 `PART_BYTES` 掃描。斷言每則 `Manifest::Directory` 編碼後不超過上限，而且整串訊息能通過 `manifest::Check`。
  - `tests/copy.rs`（整合測試）：複製上述 10,000 個長 symlink 的目錄，成功並保留 target。
  - harness 沒有對應情境，用上面的 shell 重現即可。
- **驗收條件**：
  - [ ] 上述 10,000 個長 symlink 的 `copy`、`copy --dry-run`、`sync` 都成功。
  - [ ] 每則 manifest 訊息編碼後不超過 `PART_BYTES` 加一個項目的上界。
  - [ ] 既有的掃描、比較測試不變。
- **文件要同步的地方**：`docs/design/pipeline.md` §1 Scan 中 "in parts of at most 10,000" 那句，與 `pipeline.zh-TW.md` §1 掃描的對應句，改為「至多 10,000 個項目，也至多 N MiB」。
- **相關項目**：PRO-01（同一個上限）、PRO-15。

> 2026-10-18 補記：WP-08 照建議修正，`PART_BYTES` 是 4 MiB，測試時以 `cfg!(test)` 縮小為 64 KiB。與建議不同之處：
> - 估算時也算進目錄的路徑（每段從路徑長度加 80 bytes 起算），所以每則訊息編碼後就不超過 `PART_BYTES`，比驗收條件的「`PART_BYTES` 加一個項目」更嚴。
> - 縮小的 `PART_BYTES` 對 crate 內所有單元測試都生效，不只新測試；其他測試照舊通過。
> - 單元測試（`a_part_of_long_symlinks_is_sent_in_pieces_of_at_most_its_bytes`）掃描 300 個 symlink，並以每段 100 個項目列出，讓切段發生在好幾個 lister 的段裡。
> - 整合測試 `a_directory_of_long_symlinks_is_copied` 放在 `tests/copy.rs`，依序跑 `copy --dry-run`、`copy`、`sync`。改回舊的程式碼，它會以 `a Manifest packet is too large` 失敗。

---

### PRO-04：parser 已致命失敗，主 thread 仍繼續套用 manifest，包括 `sync` 的刪除

- **嚴重度**：中。原審查列為高；重新核對後，Linux 上 `fsys::barrier` 是 no-op（`src/fsys.rs:567-574`），parser 的致命錯誤在 Linux 上主要來自串流損毀或 Source 的 bug，觸發條件比原先估的窄，因此改列中。macOS 上 `F_BARRIERFSYNC` 或 `fsync` 的 I/O 錯誤（EIO）是實際會遇到的觸發點，而這正是最不該繼續寫入的時候。
- **驗證狀態**：已實測重現（harness 情境 D）。
  - 目的地 `later/` 內有 5 個 Source 沒有的檔案，`delete = true`（sync）。
  - 先送 Root 與根目錄的 part，再送一個 ID 99 的 record（從未要求），parser 失敗為 "the Source sent unneeded ID 99"，並送出 `Control::Error`。
  - 之後再送 `later` 的 part 與 `Manifest::End`。
  - 結果：`later/` 內 5 個檔案全被刪除，輸出依序為 `…, Control::Error { "the Source sent unneeded ID 99" }, Needs::Deletes(…), Needs::End`。
- **位置**：
  - `src/destination.rs`
    - `:505-634`：`read_packets` 迴圈。
    - `:702-770`：`take_compared`。
    - `:777-832`：`apply_extras`。
    - `:1895-1942`：`delete_extras`。
    - `:1070-1137`：`apply_directory`。
    - `:334-358`：parser 的失敗處理。
    - `:1466-1473`：「unneeded ID」。
    - `:377-387`：feeder，解壓縮錯誤時同樣只是自己結束。
  - `src/fsys.rs:567-574`：`barrier`。
- **現象**：
  - Destination 的 parser（或 feeder）已經致命失敗，也已告訴 Source 傳輸失敗，主 thread 卻照常處理之後收到的每一則 manifest：建立目錄、symlink、特殊檔，改權限與 mtime，`sync` 時刪除多餘項目。
  - 它只有在下一個 data frame 到達、`frames.submit` 失敗時才會停。若之後沒有 data，例如剩下的樹都沒變只是有多餘項目，或 Source 已不再送 data，它會一路做到 `Manifest::End`。
  - Source 失敗後仍會把 manifest 送完（Source 審查的項目），兩者疊加，整棵樹的剩餘部分都會在已失敗的傳輸中被套用。
- **根本原因**：
  1. parser 失敗時，只有兩個動作：送 `Control::Error`，以及在返回時丟棄自己的 `ChannelReader`。主 thread 沒有任何旗標可看。
  2. 主 thread 只在 `frames.submit(frame)?` 失敗時才會察覺（feeder 下一次 `data.send` 失敗，丟棄 `decoded`，pool 關閉）：

  ```rust
  // src/destination.rs:625-631
  PacketType::Data => {
      let payload = packet.into_payload();
      reporter.add(Count::Wire, payload.len() as u64);
      for frame in assembler.push(&payload)? {
          frames.submit(frame)?;
      }
  }
  ```

  3. manifest 的處理路徑（`take_compared` → `apply_extras` → `delete_extras`，以及 `apply_compared` → `apply_directory`）完全不檢查別的 thread 是否已失敗。
- **重現方式**：harness `$H /path/to/scratch/hx`，看 `D writer failed:` 那一行（`extras left in later/: 0 of 5`）。
  - 情境 D 用 `std::io::pipe` 與 sleep 控制時序。
  - 改寫成測試時，建議改用確定性的做法，不用 sleep：自訂一個 `Write`，看到 `Control::Error` 就設旗標；再自訂一個 `Read`，先交出前半段，等旗標設了才交出 `later` 的 part 與 `End`。
- **修法**：依 PRO-00 加入 `Stop`，在下列位置檢查：
  - `read_packets` 迴圈開頭與每次 `take_compared` 之前；
  - `take_compared` 的 `apply_extras` 與每個 `apply_compared` 之前；
  - `delete_extras` 每個 `remove_tree` 之前；
  - compare thread 的 `tree.part` 之前；
  - parser 每個 header 之前。

  feeder 的解壓縮錯誤也要經 `stop.fail` 記錄，主 thread 才能在下一個 packet 前停下。

  若暫時無法做 PRO-00：最小修法是一個共享的 `AtomicBool`。parser 與 feeder 失敗時設定，`read_packets` 在每個 packet 與每次 `take_compared` 之前檢查，`delete_extras` 在每個項目之前檢查。但「誰是原因」的問題仍在（PRO-05），建議直接做 PRO-00。
- **要加的測試**：在 `src/destination.rs` 的 `mod tests` 加上述確定性的情境 D：斷言 `later/` 的多餘檔案都還在、`run` 回傳 "unneeded ID 99"、輸出中恰好一則 `Control::Error`，而且其後沒有 `Needs::Deletes` 或 `Needs::Files`。再做一個變體：把未要求的 ID 換成損毀的 zstd frame（feeder 失敗），斷言相同。
- **驗收條件**：
  - [ ] parser 或 feeder 失敗後，主 thread 在下一個 packet 之前停止，目的地不再有任何改變。
  - [ ] 上述兩個測試通過。
  - [ ] 正常傳輸的效能不變：檢查只是 atomic load。
- **文件要同步的地方**：
  - `docs/design/pipeline.md` §2 Compare 中 "Anything else stops the transfer at once." 那條，與 `pipeline.zh-TW.md` §2 比較中「任何其他情況都會立刻停止傳輸」：可以補上任一 stage 失敗也一樣。
  - 主要的文件變更見 PRO-00。
- **相關項目**：PRO-00（修法所在）、PRO-05、PRO-02。Source 審查的「Source 失敗後仍掃完整棵樹」：那邊修好後，manifest 會更快停止送來，但 Destination 仍須自己停。

---

### PRO-05：真正的失敗原因被 "the workers stopped" 這類後果蓋掉

- **嚴重度**：中
- **驗證狀態**：已實測重現（harness 情境 A）。
  - 送一個合法的 record frame，再送一個 kind 為 zstd、body 為亂碼的 frame，之後再送 60 個合法的 frame。
  - `run` 回傳 `the workers stopped`，Destination 的輸出中沒有任何 `Control::Error`。
  - 真正的原因「decompress a frame: …」完全沒有出現。
  - 在真正的 server 中，`serve` 接著送出 `Control::Error { "the workers stopped" }`，使用者看到的是 `the Destination … failed: the workers stopped`。
- **位置**：
  - `src/destination.rs`
    - `:377-387`：feeder 在 `bytes?` 返回，丟棄 `data` 與 `decoded`。
    - `:399-413`：`reading_failed` 與 `read? → fed? → written?` 的回傳順序。
    - `:348-358`：parser 在 `reading_failed` 時不送 Error。
    - `:629`：`frames.submit(frame)?`。
  - `src/pool.rs:90-101`、`:105-118`：Collector 被丟棄時，`submit` 回傳 "the workers stopped"。
  - `src/record.rs:108-125`：`ChannelReader` 在 sender 被丟棄時回傳 0，變成 "failed to fill whole buffer"。
- **現象**：Destination 的各種真實原因，在使用者看到的訊息裡被換成 "the workers stopped" 或 "read a record: failed to fill whole buffer"，難以診斷。被蓋掉的原因包括：
  - 損毀的 frame；
  - parser 的致命錯誤（"the Source sent unneeded ID"、macOS 上的 barrier I/O 錯誤 "order the batch's writes: …"）。
- **根本原因**：
  1. feeder 遇到解壓縮錯誤就返回，丟棄 `decoded` Collector，pool 關閉。
  2. 主 thread 下一次 `frames.submit` 得到 "the workers stopped"，`read_packets` 失敗，`reading_failed = true`。
  3. parser 的串流隨之結束，它讀到 EOF 而失敗。因為 `reading_failed` 為真，它不送 Error。
  4. `run` 先回傳 `read?`，也就是 "the workers stopped"，真正原因所在的 `fed?` 根本輪不到。
  5. parser 自己的致命錯誤也有同樣的競爭：主 thread 正卡在 `submit` 等空位時（parser 是瓶頸時很常見），pool 一關它就立刻醒來並設 `reading_failed`，可能比 parser 檢查旗標還早。這時 parser 不送 Error，而 `run` 回傳的又是 `read?`。

  ```rust
  // src/destination.rs:377-386（feeder）
  let feeding = scope.spawn(move || -> Result<()> {
      let _thread = profile::thread("feeder");
      for bytes in decoded {
          let bytes = bytes?;   // 返回：data 與 decoded 被丟棄
          if profile::wait(profile::State::ToParser, || data.send(bytes)).is_err() {
              break;
          }
      }
      Ok(())
  });
  ```

- **重現方式**：harness `$H /path/to/scratch/hx`，看 `A corrupt frame:` 那一行。這個情境是確定性的（後面有 60 個 frame，主 thread 必定卡在 `submit`），可以直接改寫成測試。
- **修法**：依 PRO-00：
  - `pool::Submitter::submit` 的錯誤型別化為 `Stopped`；
  - feeder 在 `break` 之前 `stop.fail(e)`；
  - parser 在 stopped 狀態下讀到的 EOF 轉為 `Stopped`；
  - 移除 `reading_failed`；
  - `run` 回傳 `stop.result()`。

  不做 PRO-00 的最小修法：feeder 失敗時設一個 `feeding_failed`，`run` 改成先看 `fed`、再看 `written`、最後看 `read`。但 parser 與主 thread 的競爭仍在，不建議停在這裡。
- **要加的測試**：在 `src/destination.rs` 的 `mod tests` 加入情境 A。斷言錯誤字串含 "decompress a frame"，且（做完 PRO-00 後）輸出恰好一則 `Control::Error`，內容同樣含 "decompress a frame"。再加一個 parser 致命錯誤的變體：未要求的 ID 後面接大量 data frame，斷言回傳的是 "unneeded ID"，不是 "the workers stopped"。
- **驗收條件**：
  - [ ] 兩個測試都通過，而且連續跑 100 次都穩定（`cargo test -- --test-threads=1` 搭配迴圈）。
  - [ ] 程式中不再有 `reading_failed`。
  - [ ] "the workers stopped" 不再出現在任何回傳給使用者的錯誤中。
- **文件要同步的地方**：無單獨的文件變更，併入 PRO-00。
- **相關項目**：PRO-00、PRO-04。

---

### PRO-06：需要的檔案始終沒送來，也沒被回報，傳輸仍算成功

- **嚴重度**：中。防禦性：正確的 Source 不會這樣，但一旦有 bug，檔案會被默默漏掉。
- **驗證狀態**：已實測重現（harness 情境 C）。Destination 送出 `Needs::Files([1])`，而串流只有 `Header::End`。`run` 回傳 `Ok`，`Summary { failed: 0, problems: 0 }`，輸出含 `Control::Done`，檔案 `a` 不存在。
- **位置**：
  - `src/destination.rs`
    - `:1506-1516`：`write_files` 處理 `Header::End` 時，不檢查 `state.targets` 是否還有沒來的檔案。
    - `:1727-1734`：`settled`。
    - `:1755-1778`：`finish_directories` 把仍開著的目錄默默收尾。
    - `:1117-1128`、`:1049-1058`：`targets` 在送 needs 之前插入（目錄中的檔案、根本身是檔案）。
  - `src/source.rs:352-366`：Source 唯一會不送的情況（不是檔案，或 `Needs::End` 之後）。
  - `src/source.rs:680-693`：讀不到時送 `Manifest::Problem { file: Some(id) }`。
- **現象**：Destination 要求的檔案若始終沒有 record，也沒有 `Manifest::Problem { file: Some(id) }`，傳輸照樣成功（結束狀態 0）：
  - 檔案不存在（首次複製）；
  - 或維持舊版本（有變動的檔案）。
- **根本原因**：沒有「串流結束時對帳」這一步。pipeline.md §8 甚至明寫「Only a directory with a file that never came, not even so, is finished at the end」：只把目錄收尾，不讓檔案失敗。

  ```rust
  // src/destination.rs:1506-1516
  Header::End => {
      drop(jobs);
      while in_flight > 0 {
          let outcome = profile::wait(profile::State::Outcome, || outcomes.recv())
              .context("the file writers stopped")?;
          in_flight -= 1;
          settle(outcome, &mut batch)?;
      }
      flush(&mut batch)?;
      return Ok(());   // state.targets 可能還有沒來的檔案
  }
  ```

- **重現方式**：harness `$H /path/to/scratch/hx`，看 `C needed file never sent:` 那一行。可以直接改寫成測試，沿用 `src/destination.rs` 測試裡的 `run_on` 與 `decoded`。
- **修法**：
  - 在 `write_files` 的 `Header::End` 分支，`flush(&mut batch)?` 之後，取出 `state.targets` 中剩下的每一項（`drain()`）。對每一項：
    - 呼叫既有的 `fail(id, &shown, "the Source never sent it".to_owned())`，它會送 `Results::Failed`、回報 "cannot copy …"、累計 failures；
    - 再對非根的項目呼叫 `state.file_settled(&target.directory)`，讓目錄能依 §8 照常收尾。
  - 這是單檔失敗，不是致命錯誤，符合 §7「One file failing does not stop the transfer」，結束狀態會因 failures 而為 1。
  - 可以另外加一行 warning 說明這代表 bug。
  - 時序上不會誤判，已核對程式碼：
    - Source 讀不到檔案時，在 `send_files` 中以 `mux.message` 送 `Manifest::Problem { file: Some(id) }`（messages 佇列，優先於 data），時間早於它把 `Header::End` 寫進串流；
    - Destination 的主 thread 依序處理，在把含 `Header::End` 的 frame 交給解壓縮之前，就已 `settled` 掉該 ID。
  - 請在 `send_files` 加註解，保持「Problem 一定早於 `Header::End` 送出」這個不變式。
  - 協定不變。
- **要加的測試**：在 `src/destination.rs` 的 `mod tests` 加入情境 C：
  - 斷言 `decoded::<Results>` 為 `[Results::Failed { id: 1, .. }]`；
  - 斷言 `decoded::<Control>` 為 `[Control::Done]`；
  - 斷言 `Summary.failed == 1`；
  - 斷言目錄已收尾（mtime 已設）。
- **驗收條件**：
  - [ ] 情境 C 的測試通過。
  - [ ] Source 讀不到的檔案（`Manifest::Problem { file: Some(id) }`）不會被重複算成失敗，既有測試 `a_directory_is_finished_once_its_file_the_source_could_not_open_is_settled`（`:2556`）照過。
- **文件要同步的地方**：
  - `docs/design/pipeline.md` §8 Finish 中 "Only a directory with a file that never came, not even so, is finished at the end." 一句，與 `pipeline.zh-TW.md` §8 收尾中「只有連這樣的告知都沒有、檔案始終沒來的目錄，才在最後收尾」：補上「而那個檔案失敗」。
  - 也可以在 §7 Results（`pipeline.zh-TW.md` §7 結果）提一句。
- **相關項目**：無直接相依。

---

### PRO-07：比較完的目錄要等下一個 packet 到了才套用、才送出 needs

- **嚴重度**：中（效能）
- **驗證狀態**：已實測重現（harness 情境 E，`latency`）。
  - 送出 Root 與根目錄的 part（內含一個需要的檔案），暫停 2 秒，再送 `Manifest::End`。
  - 根本身的 `Needs::Attributes([0])` 在 2 ms 時送出（根是直接套用的）。
  - `Needs::Files([1])` 卻到 2,001 ms 才送出，正好是 `End` 到達的時候。
- **位置**：
  - `src/destination.rs`
    - `:505-524`：迴圈先 `try_next` 取出已比較完的結果，再阻塞在 `protocol::receive`。
    - `:437-469`：`pool::ordered_unbounded` 與註解 "They are taken as each packet is"。
  - `src/pool.rs:198-215`：`ordered_unbounded` 的說明。
  - `src/scan.rs:172-205`：Source 的 lister 要把整個目錄列完，才交出第一個 part。
- **現象**：
  - Source 在兩則 manifest 訊息之間停頓時，Destination 已比較完的目錄不會套用，needs 也不會送出，Source 的 sender 可能因此閒置。
  - 會停頓的情況：在 NFS 上列出一個很大的目錄（lister 先讀完整個列表）、等 backlog、stat 很慢。
  - 典型例子：根目錄有幾個需要的檔案，下一個子目錄有上百萬個項目。根目錄的 needs 要等那個子目錄列完才送出。
  - data 持續流動時，每個 data packet 都會喚醒迴圈，所以影響不大。問題出在管線正需要新 needs 的時候。
- **根本原因**：迴圈只有一個等待點，就是 connection 的阻塞讀取。比較結果由 stat 池非同步產生，但沒有任何東西能把主 thread 從 `receive` 中喚醒。

  ```rust
  // src/destination.rs:505-524
  loop {
      while let Some(done) = compared.try_next() {
          profile::wait(profile::State::Apply, || {
              take_compared(/* ... */)
          })?;
      }
      let Some(packet) =
          profile::wait(profile::State::ReadConnection, || protocol::receive(reader))?
      else {
          return Ok(());
      };
      /* ... */
  }
  ```

- **重現方式**：`$H /path/to/scratch/hx latency`。情境 E 用 sleep；改寫成測試時，建議用「閘門式」的 `Read`：先交出 Root 與根目錄 part，然後等測試在輸出中看到 `Needs::Files([1])`（逾時例如 5 秒就判失敗），才交出 `End`。
- **修法**：
  - **做法 A（建議）：新增一個 apply thread。**
    - 它以阻塞的方式依序消費 `compared`，執行現在的 `take_compared`（連同 `waiting`、`through` 等狀態）。
    - 主 thread 只負責讀 packet、做 `manifest::Check`、分派：Directory 交給 compare thread；Root、Names、Problem、Data 照舊。
    - `Manifest::End`：主 thread 丟棄 `parts_in`；apply thread 在 Collector 結束時執行 `leave_all`，送出 `Needs::End`。也可以由主 thread join apply thread 後再送，二擇一，但要確保 `Needs::End` 在所有 needs 之後。
    - 時序不變式：
      - `Manifest::Root` 仍由主 thread 同步套用，而根目錄的 part 一定在它之後才進入 compare，所以 apply thread 套用根目錄時，根已處理過；
      - `Manifest::Problem { file: Some(id) }` 只會在 Source 收到該 ID 的 needs 之後才來，而 apply thread 是先插入 `targets` 再送 needs，所以主 thread 的 `settled` 不會早於插入。
    - apply thread 的錯誤經 PRO-00 的 `stop.fail` 回報；主 thread 每個 packet 前 `stop.check()`。
    - 附帶好處：結果被持續取走，compare 的池可以改回有界的 `pool::ordered`，消除 `ordered_unbounded` 註解中所說「結果的空間沒有上限」的問題。dry run 的 listing 仍由單一 apply thread 依序產生，順序不變。
  - **做法 B：讀 packet 的 thread 加單一事件 channel**，packet 與比較完成的通知走同一個 channel。注意：通知不能和 packet 共用有界 channel，否則會死結。例如 stat worker 卡在送通知，主 thread 卡在 `parts_in.send`，compare thread 卡在 `submit`。較複雜，不建議。
- **要加的測試**：在 `src/destination.rs` 的 `mod tests` 加上述閘門式測試：`Needs::Files([1])` 必須在 `End` 交出之前出現。既有的 dry run 列出順序測試（`tests/dry_run.rs`）照過。
- **驗收條件**：
  - [ ] 閘門式測試通過。
  - [ ] dry run 列出的順序與 manifest 順序相同（既有測試）。
  - [ ] `sync` 仍先刪多餘項目、再套用各段（既有測試 `:2130`、`:2187`、`:2243`）。
  - [ ] 比較用的池改為有界後，`ferry test` 的實際規模情境記憶體不增加。
- **文件要同步的地方**：
  - `docs/design/pipeline.md`「Principles」表格中 Comparing 那一列，與 `pipeline.zh-TW.md`「原則」的「比較 | 一個列出 Destination 的目錄，8 個 `stat` | 依 manifest 的順序套用」：加上 apply thread。
  - 圖 `docs/assets/pipeline-workers.svg` 與 `docs/assets/pipeline-scan.svg` 中 Destination 主 thread 的部分要重畫。
  - `pipeline.md` §2 Compare（`pipeline.zh-TW.md` §2 比較）中「在 manifest 的順序套用」的描述，順帶說明由哪個 thread 套用。
- **相關項目**：PRO-00（apply thread 的錯誤與停止）、PRO-04（檢查點搬到 apply thread）。

---

### PRO-08：Destination 把 manifest 結束前的 EOF 當成正常結束

- **嚴重度**：低
- **驗證狀態**：
  - 已實測重現（harness 情境 B）：dry run，只送 Root 與一個 `more: true` 的 part 就 EOF，`run` 回傳 `Ok(Summary)`。
  - 情境 G 顯示，非 dry run 時同樣的截斷只得到 `read the record stream: failed to fill whole buffer`。
  - client 端的影響是已核對程式碼的推論，未實測。
- **位置**：
  - `src/destination.rs:520-524`：`receive` 回傳 `None` 就 `return Ok(())`，不看 `check` 是否已到結尾。
  - `src/manifest.rs:110-214`：`Check` 有 `ended`，但沒有公開的查詢方法。
  - `src/peer.rs:125-146`：inward thread 在 transport 失敗時，把錯誤放進 `failed`，再關閉 peer 的輸入，於是 peer 看到的是正常 EOF。`:153-168`：outward 只有在 peer 送出 `Control::Error` 時，才把原因換成 connection 的錯誤。
  - `src/relay.rs:205-210`：轉送給某個 server 失敗時默默設 `to = None`，不記錄失敗。註解寫著 "its own end says why"。
- **現象**：
  - dry run：client 到 Destination 這個方向的 connection 失敗，而另一方向正常時，Destination 收到乾淨的 EOF，回報成功，server 以 0 結束。relay 又不記錄轉送失敗，於是被截斷的 listing 以結束狀態 0 收場。這要單方向失敗，例如 client 端某條 path 的寫入失敗；SSH connection 整條斷掉時兩個方向都會失敗，client 抓得到。因此是低。
  - 非 dry run：同樣的截斷會失敗，但訊息是費解的 "read a record: failed to fill whole buffer" 或 "read the record stream: …"。
- **根本原因**：dry run 的正常結束本來就是 EOF：Destination 送出 `Needs::End` 之後一直讀到 EOF。但程式沒有區分「`Manifest::End` 之後的 EOF」與「之前的 EOF」。peer.rs 的原因替換又依賴 peer 主動送出 `Control::Error`。

  ```rust
  // src/destination.rs:520-524
  let Some(packet) =
      profile::wait(profile::State::ReadConnection, || protocol::receive(reader))?
  else {
      return Ok(());
  };
  ```

- **重現方式**：harness `$H /path/to/scratch/hx`，看 `B dry run, EOF before Manifest::End:` 那一行；`$H … truncated` 看非 dry run 的訊息。兩者都可以直接改寫成測試。
- **修法**：
  1. 在 `manifest::Check` 加 `pub fn is_ended(&self) -> bool`。`read_packets` 收到 `None` 時：

     ```rust
     else {
         ensure!(check.is_ended(), "the connection closed before the manifest ended");
         return Ok(());
     };
     ```

  2. parser：`read_header` 在 record 的邊界讀到 `UnexpectedEof` 時，回報 "the record stream ended before its end"；`forward_record` 與 `write_file` 在 record 中間讀到時，回報 "the record stream ended in the middle of {path}"。做了 PRO-00 後，若已停止，則一律為 `Stopped`。
  3. peer.rs：`serve` 在 `run` 回傳 `Ok` 之後，若 inward 曾經失敗（`cause` 裡有錯誤），仍以該錯誤失敗。實作上把 `cause` 由 outward thread 改為共享的 `Arc<Mutex<Option<anyhow::Error>>>`，或由 outward 在結束時一併取出並回傳。這是最穩的一層：不管 peer 自己有沒有發現，connection 失敗就是失敗。
  4. relay：把 `sending.send` 的失敗記到另一個「備用」位置，例如 `fallback: Mutex<Option<String>>`。只有在整個轉送結束時沒有其他失敗，才以它回報，避免蓋掉對方自己的原因。

  協定不變。
- **要加的測試**：
  - `src/destination.rs` 的 `mod tests`：
    - 情境 B（dry run 與非 dry run 各一）：預期錯誤含 "closed before the manifest ended"；
    - 情境 G：預期錯誤指出串流在檔案中間結束。
  - `src/relay.rs`：`pass` 的單元測試較難寫；若要測，用 `transport` 的測試工具建一對 connection，讓送出端提早關閉，斷言 `relay` 回傳錯誤。可列為選做。
- **驗收條件**：
  - [ ] 情境 B 的 dry run 回傳錯誤。
  - [ ] 正常的 dry run（`Manifest::End` 之後 EOF）仍成功（既有 `tests/dry_run.rs`）。
  - [ ] 非 dry run 的截斷訊息清楚指出 connection 在何處結束。
- **文件要同步的地方**：`docs/design/pipeline.md` §1 Scan 中 "A scan that fails stops the transfer at once … the Destination waits for the rest of the manifest" 那條，與 `pipeline.zh-TW.md` §1 掃描的對應條：補一句「manifest 結束前 connection 就關閉，也算失敗」。
- **相關項目**：PRO-00、PRO-02（截斷正是留下暫存檔的路徑）。

---

### PRO-09：client 不檢查 server 送來的 Metrics 狀態編號，超出範圍就 panic；Profile report 無上限累積

- **嚴重度**：低。同一個 build 正常運作時編號一定合法，只有資料損毀或惡意 host 才會觸發。
- **驗證狀態**：已核對程式碼。
- **位置**：
  - `src/control.rs:63-77`：`busiest` 對每個 `(index, nanos, _)` 呼叫 `State::from_index(index)`。
  - `src/profile.rs:148-150`：`from_index` 直接 `State::ALL[index as usize]`。
  - `src/profile.rs:693-701`：`RoleMetrics.states: Vec<(u8, u64, u64)>`，來自線上。
  - `src/relay.rs:333-355`：`listen` 從 control connection 解碼 Metrics。
  - `src/relay.rs:235-327`、`:83-115`：control thread 在 `std::thread::scope` 中，panic 會在 scope 結束時重新拋出。
  - `src/relay.rs:176-181`：每個 `Report::Profile` 都 `push` 進 `profiles`。
- **現象**：
  - server 送來超出範圍的狀態編號時，client 的 control thread panic；relay 跑完後整個 client 以 panic 結束，結束狀態 101，即使傳輸本身已完成。
  - server 反覆送 `Report::Profile` 時，client 記憶體無上限成長，每則最多 16 MiB。
- **根本原因**：對線上資料使用未檢查的索引；`profiles` 沒有每側只留一份的限制。

  ```rust
  // src/profile.rs:148-150
  pub(crate) fn from_index(index: u8) -> State {
      State::ALL[index as usize]
  }
  ```

- **重現方式**：測試草稿。在 `src/control.rs` 的 `mod tests` 建一個 `Metrics`，其 `roles[0].states = vec![(255, 1_000_000_000, 1)]`，呼叫 `busiest(&view)`；目前會 panic。
- **修法**：
  - 新增 `State::try_from_index(index: u8) -> Option<State>`，實作為 `State::ALL.get(index as usize).copied()`。`busiest` 改用它，遇到 `None` 就跳過。本機自己的兩處（`profile.rs:261`、`:817`）可以保留 `from_index`，或改為 `.expect("an index this process set")`。
  - `relay.rs` 的 `profiles`：同一個 side 名稱已有一份時就取代（或忽略後來的），每側只留一份。
- **要加的測試**：`src/control.rs` 的 `mod tests`：上述 255 的情況回傳 `None` 而不 panic。`src/relay.rs` 的 `mod tests`：同一側兩則 Profile 只留一份。可以把 `pass` 的 Report 分支抽成小函式來測。
- **驗收條件**：
  - [ ] 不合法的狀態編號不會讓 client panic。
  - [ ] 每側至多保留一份 Profile。
- **文件要同步的地方**：無。`docs/design/profiling.md` 與 `profiling.zh-TW.md` 的行為描述不變。
- **相關項目**：無。

> 2026-10-20 補記：WP-14 照修法做：新增 `State::try_from_index`，`busiest` 遇到不存在的編號就跳過；本機自己的兩處保留 `from_index`。`relay` 的 `keep_profile` 讓每側只留一份，後來的取代先前的。測試：`a_state_numbered_wrong_is_passed_over`、`one_profile_is_kept_for_each_side`。

---

### PRO-10：交握在比對 build 之前就解碼整個 `Hello`，布局不同時看不到 build 不符的說明

- **嚴重度**：低。安裝路徑 `~/.ferry/hosts/<machine-id>/bin/<build>/ferry` 已確保執行的是同一個 build，要碰到這個問題，該路徑上得是損毀或被換掉的 binary。
- **驗證狀態**：已核對程式碼。
- **位置**：
  - `src/session.rs:63-93`：`server_handshake` 先 `protocol::expect::<Control>` 解碼完整的 `Control::Hello(Hello)`，才比對 `hello.build`。
  - `src/protocol.rs:57-108`：`Control` 與 `Hello` 的布局。
  - `src/peer.rs:36-50`：handshake 失敗時 `serve` 直接回傳錯誤。
  - `src/cli.rs:378-389`：server 失敗時什麼都不印。
  - `src/transport/mod.rs:451-467`：client 在沒有收到答覆時回報 "the server did not start ({status})"。
- **現象**：若 host 上的 server 與 client 的 `Hello` 布局不同，server 解碼失敗，不寫任何答覆就結束，也不印任何東西。client 只看到 `the server did not start (exit status: 1)`，看不到設計所承諾的「A server of another build refuses the connection and says which two builds met」（docs/cli.md「Remote hosts」）。
- **根本原因**：build 比對寫在完整解碼之後；而 postcard 不是自我描述的格式，布局不同就無法解碼。

  ```rust
  // src/session.rs:69-79
  let hello = match protocol::expect::<Control>(reader).context("wait for the client's hello")? {
      Control::Hello(hello) => hello,
      other => bail!("expected a hello, got {other:?}"),
  };
  if hello.build != build {
      let reason = format!(
          "the server is build {build}, but the client is build {}",
          hello.build
      );
      return refuse(writer, reason);
  }
  ```

- **重現方式**：測試草稿。在 `src/session.rs` 的 `mod tests`，手工組出一個 Control packet：payload 為 postcard 的 variant 0、build 字串 `"dev-x"`，後面接隨意的位元組（模擬不同的布局）。交給 `server_handshake`，目前它回傳錯誤且 `to_client` 為空，`client_handshake` 得到 `Closed`。
- **修法**：
  - 先解碼前綴：
    ```rust
    #[derive(Deserialize)] enum ControlPrefix { Hello(HelloPrefix) }  // 只有 variant 0
    #[derive(Deserialize)] struct HelloPrefix { build: String }
    ```
    `postcard::from_bytes` 會忽略多出的位元組，所以不論後面的欄位怎麼變，都能取得 build。build 不符就以 `Control::Refused` 拒絕並說出兩個 build；相符才解碼完整的 `Control`。完整解碼失敗時，也以 `Refused { reason: "the client's hello could not be read: …" }` 答覆，而不是默默結束。
  - 把「`Control::Hello` 是 variant 0、`build` 是 `Hello` 的第一個欄位、`Control::Refused` 的 variant 編號與 `reason` 欄位」定為跨 build 都不得改變的布局，在 `protocol.rs` 加註解並以測試釘住。這是本文件「不需相容」原則的唯一例外，因為比對 build 之前雙方還不知道對方是不是同一個 build。
- **要加的測試**：`src/session.rs` 的 `mod tests`：
  - 上述「前綴合法、後面亂碼」的 hello，server 以 Refused 答覆，訊息含兩個 build，client 得到清楚的錯誤而非 `Closed`；
  - 一個釘住布局的測試：`postcard::to_stdvec(&Control::Hello(h))` 的第一個 byte 為 0，接著是 build 字串；`Refused` 的 variant 編號為 2。
- **驗收條件**：
  - [ ] 不同布局的 hello 得到「build 不符」或「無法讀取 hello」的明確拒絕。
  - [ ] 既有的交握測試照過。
- **文件要同步的地方**：
  - `docs/design/architecture.md`「Installing on a remote host」第一點 "The handshake still compares builds, and refuses a mismatch with a clear error"，與 `architecture.zh-TW.md`「在遠端 host 上安裝」第一點：補上「build 在 hello 的其他內容被讀取之前就比對；hello 的第一個欄位與拒絕訊息的布局跨 build 不變」。
  - `docs/cli.md`「Remote hosts」與 `cli.zh-TW.md`「遠端 host」的描述不變，只需確認仍正確。
- **相關項目**：無。

---

## 較小的項目

以下各項篇幅較短，每項都可以單獨處理。

### PRO-11：`Needs::Through(u64::MAX)` 讓 `Known::compared_through` 溢位

- **嚴重度**：低。只有有 bug 或惡意的 Destination 會觸發。
- **驗證狀態**：已核對程式碼。
- **位置**：`src/source.rs:129-133`（`part.end() <= id + 1`）、`:372`。
- **現象**：debug build 會 panic；release build 溢位成 0，什麼都不釋放，`Known` 只增不減。
- **修法**：改成 `part.end() <= id.saturating_add(1)`。也可以改成 `part.end() - 1 <= id`：part 不會是空的，所以 `end() ≥ first + 1`。
- **測試**：`src/source.rs` 的 `mod tests` 中，`known_entries_give_back_their_paths` 旁加一個 `compared_through(u64::MAX)` 清空全部的案例。
- **文件**：無。

### PRO-12：同一 ID 的第二個 record 在第一個還沒 settled 時被接受

- **嚴重度**：低。只有 Source 的 bug 會觸發。
- **驗證狀態**：已核對程式碼。
- **位置**：`src/destination.rs:1466-1473`，`targets.get(&id).cloned()` 不移除；`:1727-1734` 的 `settled`。
- **現象**：兩個 writer 同時寫同一個目標，各自用一個暫存檔，先後重新命名，結果無害。但 Source 的 bug 不會被抓到。
- **修法**：
  - 在 `State` 加 `arriving: HashSet<FileId>`。收到 header 時，若 ID 已在 `arriving`，或已不在 `targets`，就失敗為 "the Source sent ID {id} twice"。
  - `settled` 時一併移除。
  - 注意 `Manifest::Problem { file: Some(id) }` 也走 `settled`。
- **測試**：`src/destination.rs` 的 `mod tests`：同一 ID 連送兩個完整 record，預期錯誤。
- **文件**：無。

### PRO-13：`frame::Assembler` 每個 frame 都重新長大緩衝區，frame 帶著過大的容量

- **嚴重度**：低（效能與記憶體）。
- **驗證狀態**：推測。核對過程式碼，但沒有量測。
- **位置**：`src/frame.rs:56-76`，`split_off(HEADER + body)` 加 `mem::replace`。
- **現象**：
  - 完成的 frame 保留了整個成長過程中倍增出來的容量，可能接近實際長度的 2 倍。在解壓縮池中最多同時 16 個。
  - 剩下的部分成為新的小緩衝區，下一個 frame 又要從約 64 KiB 倍增到約 2.25 MiB，每個 frame 在唯一讀 connection 的主 thread 上多做約 2 倍 frame 大小的 memcpy。
- **修法**：讀到 frame header 後，以 `Vec::with_capacity(HEADER + body)` 建立該 frame 的緩衝區，只複製屬於它的位元組；跨 frame 的剩餘部分放在一個重複使用的緩衝區。修改前後用 `ferry test` 的實際規模情境量測主 thread 的 CPU 時間與峰值記憶體。
- **測試**：`src/frame.rs` 既有的 `frames_split_anywhere_are_put_back_together` 照過；可以另加一個斷言：`frame.capacity() <= frame.len() + 小常數`。
- **文件**：無。

### PRO-14：relay 遇到無法解碼的 report 時記下失敗，卻讓整個傳輸跑完

- **嚴重度**：低。同一個 build 只有 bug 會觸發。
- **驗證狀態**：已核對程式碼。
- **位置**：`src/relay.rs:176-197`，`Err(error) => fail(...)` 之後 `continue`。
- **現象**：傳輸照常完成，最後卻以那個解碼錯誤失敗，結束狀態 1。
- **修法**：二擇一：
  - 把它當成警告（`Report::Said`）而不讓整個執行失敗；
  - 或者既然代表兩側不一致，就關閉另一側的輸入以立即停止。

  建議前者：report 只給人看，不影響目的地的正確性。
- **測試**：可選。把 `pass` 的 Report 分支抽成函式後做單元測試。
- **文件**：無。

### PRO-15：striped 接收端的 early piece 以個數限制，而不是以位元組

- **嚴重度**：低。屬 transport 的範圍，本審查未深入，請 transport 的負責人確認。
- **驗證狀態**：推測，只核對了限制的寫法。
- **位置**：`src/transport/striped.rs:35-36`（`EARLY = 256`）、`:275-277`。
- **現象**：每個 piece 是一個 packet，最大可到 `MAX_PAYLOAD`（16 MiB），最壞情況是 256 × 16 MiB = 4 GiB 的早到 piece 停在記憶體裡。正常流量中，data packet 只有 64 KiB，manifest part 最多數 MB，所以實際風險取決於多條 path 時某條 path 停頓的情況。
- **修法**：除了個數，再加一個位元組上限，例如 64 MiB；超過時讀 path 的 thread 等待，下一個該到的 piece 除外，與現在的規則一致。
- **測試**：`src/transport/striped.rs` 的測試中，以大 packet 驗證位元組上限。
- **文件**：`docs/design/architecture.md`「Transport」原則 8（"A sender waits, and memory is bounded"），與 `architecture.zh-TW.md`「Transport」原則 8：可註明接收端的上限同時以個數與位元組計。
