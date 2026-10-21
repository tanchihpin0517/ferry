# Destination 與檔案系統層：待修項目

- 對象：HEAD `3c17a2f`（version 0.14.4）。以下所有行號都對照這個 commit 重新核對過。
- 範圍：`src/destination.rs`、`src/fsys.rs`、`src/compare.rs`、`src/owners.rs`、`src/names.rs`、`src/record.rs`、`src/interrupt.rs`（暫存檔登記），以及它們如何使用 `src/manifest.rs`。
- 不在此文件中：`sync` 防範打錯的 DESTINATION（`$HOME`、`/`、`host:`）屬於 CLI 審查；Source 掃描每層祖先目錄占一個 fd 屬於 Source 審查。
- 每一項都可單獨閱讀，不需要先讀其他項目。
- 實驗程式：[`materials/destination/exp/`](materials/destination/exp/) 是一個以 path 依賴 ferry 的小 crate，直接把手工組出的 packet 餵給 `destination::run`。在 repo 根目錄執行 `CARGO_TARGET_DIR=$PWD/.scratch/review-dest/target cargo run --release --manifest-path docs/review/2026-10-17/materials/destination/exp/Cargo.toml`，或在最後加上 `-- dfs`，只跑 DST-08。下面每一項也都附有不依賴它的重現方式或測試草稿。
- 驗證狀態的意思：「已實測重現」是實際跑過並看到現象；「已核對程式碼」是逐行追過程式碼路徑，但沒有實際觸發；「推測」是可能，但沒有完整追到。

## 總表

| ID | 嚴重度 | 標題 |
|---|---|---|
| DST-01 | 高 | 設定權限時會跟隨目的地中的符號連結（chmod 的 TOCTOU） |
| DST-02 | 高 | 傳輸因 Source 失敗、connection 中斷或串流錯誤而失敗時，暫存檔留在目的地 |
| DST-03 | 高 | 每個等待重新命名的檔案占 3 個 fd，開檔上限 768 以下時大量檔案失敗 |
| DST-04 | 中 | Destination 有一個目錄打不開或一個項目 stat 失敗，整個傳輸就中止 |
| DST-05 | 中 | 一個目錄收尾失敗，之後的目錄全部不再收尾，整個傳輸以錯誤結束 |
| DST-06 | 中 | 檔案的擁有者、權限或 mtime 設不上時，已驗證的檔案整個被丟棄 |
| DST-07 | 低 | 這次執行建立的根目錄，其父目錄沒有 `fsync` |
| DST-08 | 低 | manifest 檢查不要求目錄依深度優先的順序出現 |
| DST-09 | 低 | 全域 `State` 鎖在路徑走訪、套用目錄與 `getent` 期間一直持有 |
| DST-10 | 低 | `getent` 的參數是 Source 給的名稱，以 `-` 開頭的名稱會被當成選項 |
| DST-11 | 低 | 為了進入或刪除而放寬的目錄權限，在失敗時不會還原 |
| DST-12 | 低 | `remove_tree` 的遞迴深度沒有上限 |
| DST-13 | 低 | 根目錄是指向檔案的符號連結時，比較跟隨連結，套用卻只部分跟隨 |
| DST-14 | 低 | 不是擁有者時，群組不同仍被當成差異 |
| DST-15 | 低 | manifest 不檢查 mtime 的奈秒是否小於 10^9 |
| DST-16 | 低 | 每則 `Manifest::Names` 都複製整份 ownership 對照表 |
| DST-17 | 低 | 回報失敗時，每個失敗都線性搜尋整段的項目 |
| DST-18 | 低 | 把兩個名稱視為同一個的檔案系統上，Source 的名稱相撞時 `sync` 會刪掉別名所指的目錄樹 |
| DST-19 | 低 | `sync` 刪除多餘的目錄時會進入其中的掛載點 |

DST-01 到 DST-10 採完整格式；DST-11 到 DST-19 是較小的項目，以簡短格式列在最後。

---

### DST-01：設定權限時會跟隨目的地中的符號連結（chmod 的 TOCTOU）

- 嚴重度：高
- 驗證狀態：已實測重現（原語層級）。在一個目錄中放一個指向目錄外檔案（模式 0600）的符號連結 `f`，呼叫 `fsys::set_mode_at(&dir, b"f", 0o4777)`：回傳 `Ok(())`，目錄外的檔案變成 `4777`。「比較之後、套用之前把檔案換成連結」這個競爭本身沒有實測，是逐行追程式碼推得。
- 位置：
  - `src/fsys.rs:465-474` `set_mode_at`：`chmodat(..., AtFlags::empty())`
  - `src/destination.rs:1150-1162` `apply_entry` 的 `Verdict::Attributes` 分支（chmod 在 1158）
  - `src/destination.rs:1191-1205` `apply_entry` 的 `Verdict::Content` 分支：FIFO、socket、裝置的暫存名稱（chmod 在 1199）
  - `src/compare.rs:363-376` 比較時的 stat（不跟隨，但與套用相隔一段時間）
  - 對照：`src/fsys.rs:120-153` `let_owner_in_at` 已經用了不跟隨連結的做法（`O_PATH` + `/proc/self/fd`）
- 現象：以 root（或 `--remote-sudo`）把資料寫進別的使用者也能寫入的目錄樹時（例如 `ferry sync backup:/home /home` 還原家目錄），該使用者可以讓 Ferry 對目的地之外的任意檔案 chmod，模式取自 Source 項目的模式，可能含 setuid。這違反概觀「接收端在寫入時，絕不跟隨目的地中已存在的符號連結」。擁有者與 mtime 不受影響，因為它們用的是不跟隨連結的呼叫。
- 根本原因：
  1. stat thread 在比較時以 `AT_SYMLINK_NOFOLLOW` stat 一個項目，發現它是內容相同、只有權限不同的一般檔案，判定為 `Verdict::Attributes`。
  2. 套用在之後的 main thread 進行，中間可能隔著整個佇列。`apply_entry` 再 stat 一次，但不檢查類型，接著依序呼叫 `set_owner_at`（不跟隨）、`set_mode_at`（跟隨）、`set_mtime_at`（不跟隨）：

     ```rust
     // src/destination.rs:1150-1162
     Verdict::Attributes => {
         let stat = fsys::stat(&directory, name)?;
         #[allow(clippy::unnecessary_cast)]
         let (uid, gid) = (stat.st_uid as u32, stat.st_gid as u32);
         set_owner(&state.ownership, uid, gid, entry, |uid, gid| {
             fsys::set_owner_at(&directory, name, uid, gid)
         })?;
         if !entry.kind.is_symlink() {
             fsys::set_mode_at(&directory, name, entry.mode & 0o7777)?;
         }
         fsys::set_mtime_at(&directory, name, entry.mtime)?;
         Ok(())
     }
     ```
  3. `set_mode_at` 以 flag 0 呼叫 `fchmodat`，會跟隨連結：

     ```rust
     // src/fsys.rs:465-474
     /// Sets the permissions of `name` in `directory`, which is not a symlink.
     pub fn set_mode_at(directory: impl AsFd, name: &[u8], mode: u32) -> io::Result<()> {
         one_name(name)?;
         Ok(rustix::fs::chmodat(
             directory,
             OsStr::from_bytes(name),
             Mode::from_raw_mode(mode as _),
             AtFlags::empty(),
         )?)
     }
     ```
     註解說「which is not a symlink」，但呼叫端沒有任何保證。Linux 的 `fchmodat` 在 6.6 之前不支援 `AT_SYMLINK_NOFOLLOW`，這可能是當初選 `AtFlags::empty()` 的原因。
  4. 特殊檔案也一樣：`mknodat` 以暫存名稱建立之後、`set_mode_at` 之前，能寫入該目錄的人可以把暫存名稱換成連結（暫存名稱可經由 readdir 看到）。
- 重現方式：
  - 原語（任何使用者都能跑），寫成 `src/fsys.rs` 的測試即可，見「要加的測試」。
  - 攻擊情境（只在自己的測試機上做）：root 執行 `ferry sync backup:/home /home`。mallory 事先在備份中留下 `/home/mallory/x`（模式 04755），目的地的 `/home/mallory/x` 大小、mtime 相同但模式 0644，所以它是 `Verdict::Attributes`。比較之後、套用之前，mallory 把 `/home/mallory/x` 換成指向 `/bin/bash` 的符號連結（以迴圈 `mv` 或 `inotifywait` 盯著時機）。結果 `/bin/bash` 變成 04755。
- 修法：
  - 方案 A（建議）：讓 `fsys::set_mode_at` 絕不跟隨連結，遇到連結就回錯。
    - Linux：以 `openat(dir, name, O_PATH | O_NOFOLLOW | O_CLOEXEC)` 取得指向該 inode 的 fd，`fstat` 它；若是符號連結，回傳 `ELOOP`；否則對 `/proc/self/fd/N` 呼叫 `chmod`。fd 固定住 inode，所以 fstat 與 chmod 之間沒有競爭。`/proc` 不存在時（某些 chroot 或容器）會得到 `ENOENT`，直接回錯，絕不退回會跟隨的呼叫。可選：先試 `fchmodat2(dir, name, mode, AT_SYMLINK_NOFOLLOW)`（Linux 6.6+，用 `libc::syscall(libc::SYS_fchmodat2, …)`，先確認 `libc` crate 在 gnu 與 musl 目標上都有這個常數），它對連結回 `EOPNOTSUPP`；收到 `ENOSYS` 再走 `O_PATH` 的路。
    - macOS：`fchmodat` 支援 `AT_SYMLINK_NOFOLLOW`，但它對連結是改連結本身的模式並回傳成功。為了讓兩個平台行為一致，先以 `fstatat(…, AT_SYMLINK_NOFOLLOW)` 檢查；是連結就回 `ELOOP`；不是就以 `AT_SYMLINK_NOFOLLOW` 呼叫 `fchmodat`。兩次呼叫之間若被換成連結，改到的也只是連結本身，不會出目錄。
    - `let_owner_in_at`（`src/fsys.rs:120-153`）的 Linux 分支可以改用同一個輔助函式。
  - 另外在 `apply_entry` 的 `Verdict::Attributes` 分支，檢查再 stat 得到的類型與 `entry.kind` 相符（Source 不是符號連結時，這裡就不能是連結）；不符時讓這個項目失敗，訊息如「changed since it was compared」。這只縮小時間窗，並不能取代方案 A。
  - 方案 B（不建議單獨使用）：以 `O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_NOCTTY` 開啟項目，再對 fd 做 `fchown`、`fchmod`、`futimens`。對一般檔案與目錄能完全關掉時間窗，但非 root 時打不開沒有讀取權限的檔案，而開啟裝置檔可能有副作用（例如磁帶機會倒帶），所以不能用在特殊檔案上。
  - 不在此修法範圍：攻擊者若把檔案換成指向別處的**硬連結**，chmod/chown 仍會作用到那個 inode（方案 A、B 都一樣）。Linux 大多數發行版預設 `fs.protected_hardlinks=1`，不能硬連結自己不擁有的檔案；macOS 沒有這個保護。應在文件中說明。
  - 程式碼草稿（Linux 分支）：

    ```rust
    pub fn set_mode_at(directory: impl AsFd, name: &[u8], mode: u32) -> io::Result<()> {
        one_name(name)?;
        use std::os::fd::AsRawFd;
        let file = rustix::fs::openat(&directory, OsStr::from_bytes(name),
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())?;
        if file_type(&rustix::fs::fstat(&file)?) == FileType::Symlink {
            return Err(rustix::io::Errno::LOOP.into());
        }
        let named = format!("/proc/self/fd/{}", file.as_raw_fd());
        Ok(rustix::fs::chmod(named.as_str(), Mode::from_raw_mode(mode as _))?)
    }
    ```
- 要加的測試：
  - `src/fsys.rs` 的 `mod tests`：`a_mode_is_never_set_through_a_symlink`。建立 `root/link -> outside`，`outside` 模式 0600；`set_mode_at(&root, b"link", 0o4777)` 必須回 `Err`，`outside` 仍是 0600。Linux 與 macOS 都要通過。
  - `src/destination.rs` 的 `mod tests`：`attributes_are_never_set_through_a_symlink`。直接呼叫 `apply_entry(&dir, b"f", b"f", &entry, Verdict::Attributes, &mut State::new(false))`，其中 `f` 是指向目錄外檔案的連結、`entry` 是 `Kind::File`、模式 0o4777；必須回 `Err`，目錄外檔案的模式不變。
  - 特殊檔案的暫存名稱：同上，對 `set_mode_at` 的單元測試已涵蓋。
- 驗收條件：
  - [ ] `src/` 中沒有任何以 `AtFlags::empty()` 呼叫 `chmodat` 的地方（`grep -n "chmodat" src/` 檢查）。
  - [ ] 上面兩個測試在 Linux 與 macOS 上通過。
  - [ ] `cargo test` 全部通過，包含 `a_change_never_reaches_out_of_its_directory` 與 `a_directory_is_let_into_never_through_a_symlink`。
  - [ ] 沒有 `/proc` 時，`set_mode_at` 回錯，而不是跟隨連結。
- 文件要同步的地方：`docs/design/overview.md`「The first version covers at least `rsync -a`」最後的符號連結段落（`overview.zh-TW.md`「第一版至少涵蓋 `rsync -a`」中「接收端在寫入時，絕不跟隨…」那段）：補一句設定擁有者、權限與 mtime 時同樣不經過連結，並說明硬連結的限制。
- 相關項目：DST-13（根目錄是連結時的另一種跟隨）、DST-09（共用目錄 fd 時應沿用同樣的不跟隨原則）。

---

### DST-02：傳輸因 Source 失敗、connection 中斷或串流錯誤而失敗時，暫存檔留在目的地

- 嚴重度：高
- 驗證狀態：已實測重現。
  1. 真實執行（Linux，本機兩側）：`ferry copy` 一棵含 8 個 200 MB 檔案與 300 個小檔案的目錄樹，1 秒後 `kill -9` Source 的 server。client 結束碼 1，訊息 `the Source on this machine: read a packet payload: failed to fill whole buffer`；目的地留下 5 個 `.bigN.ferry-*`，共 880 MB。
  2. 直接餵 `destination::run`：5 個完整、雜湊正確的 record 之後接一個 `Control::Error`，5 個暫存檔全部留下，既沒有重新命名也沒有刪除；record 在內容中途截斷時，連截斷的那個也留下。
  3. 對照：client 收到 SIGINT 時，三次不同時間點的測試都沒有殘留（server 經由 `interrupt::stop` 清除）。
- 位置：
  - `src/interrupt.rs:189-199` `Registered` 的 `Drop` 只取消登記，不刪檔
  - `src/interrupt.rs:204-212` `create`：`create()` 成功後若 `register` 失敗，剛建立的檔案沒有刪除
  - `src/destination.rs:1653-1666` `write_file`：`read_exact(...)?` 與 `read_trailer(stream)?` 提早返回，跳過 `1701-1704` 的清除
  - `src/destination.rs:1259-1272` `Ready` 沒有 `Drop`
  - `src/destination.rs:1402` 的 `batch`：`write_files` 中任何 `?`（1462、1465、1473、1501、1504、1509-1512）返回時被丟棄
  - `src/destination.rs:1572-1587` `rename_into_place` 與 `1191-1210` `apply_entry`：修改後需要在重新命名成功時解除
- 現象：只要不是訊號造成的失敗，例如 Source 那邊掃描失敗、Source 的 server 被殺或 OOM、connection 斷掉、資料串流損壞，或開檔時遇到 EMFILE（見 DST-03），目的地都會留下隱藏的 `.<name>.ferry-<hex>` 檔案，大小可達原檔。`copy` 永遠不會刪掉它們，因為它們是多餘的項目，而 `copy` 保留多餘的項目；只有之後的 `sync` 才會刪。概觀說「執行中斷時，Destination 會刪除它未完成之檔案的暫存檔。只有當機或行程被終止，才可能留下暫存檔」，指的是 Destination 本身當機或被終止，與現況不符。
- 根本原因：
  1. 暫存檔在 `interrupt::create` 中建立並登記。登記只為了訊號處理：`Registered` 被 drop 時只把自己從表中移除，不刪檔：

     ```rust
     // src/interrupt.rs:189-199
     /// A registered temporary file. Dropping it unregisters the file, once it is
     /// renamed into place or removed.
     pub struct Registered(u64);

     impl Drop for Registered {
         fn drop(&mut self) {
             if let Some(files) = TEMPORARY.lock().expect("not poisoned").as_mut() {
                 files.remove(&self.0);
             }
         }
     }
     ```
  2. `write_file` 讀 record 內容時，串流提早結束 → `read_exact` 回錯 → `?` 直接返回。`opened`（含 `registered`）被 drop，檔案留下，登記也沒了：

     ```rust
     // src/destination.rs:1653-1666
     while remaining > 0 {
         let count = buffer.len().min(remaining as usize);
         stream
             .read_exact(&mut buffer[..count])
             .context("read the record stream")?;
         …
     }
     let trailer = record::read_trailer(stream)?;
     ```
     真正的清除只在函式尾端（1701-1704），上面的 `?` 到不了那裡。
  3. 已寫完、驗證並 `fsync` 的檔案以 `Ready` 放在 `batch` 裡，等湊滿 256 個才批次重新命名。`write_files` 因串流錯誤以 `?` 返回時，`batch` 被丟棄，而 `Ready` 沒有 `Drop`，檔案留下。
  4. 這些 thread 在錯誤中結束時已經 drop 了所有 `Registered`。之後即使 server 因 client 離開而呼叫 `interrupt::stop`，表中也已經沒有它們。Source 失敗時，Destination 的 server 則是正常地回傳錯誤並結束，根本不經過 `interrupt::stop`。
  5. `interrupt::create` 先建立，再 `register`（其中 `try_clone_to_owned` 會 dup 目錄 fd）。遇到 EMFILE 時 `register` 失敗，`?` 返回，已建立的檔案沒有刪除：

     ```rust
     // src/interrupt.rs:204-212
     pub fn create<T>(
         directory: impl AsFd,
         create: impl FnOnce() -> io::Result<(T, Vec<u8>)>,
     ) -> io::Result<(T, Vec<u8>, Registered)> {
         let _gate = GATE.read().expect("not poisoned");
         let (made, name) = create()?;
         let registered = register(directory, &name)?;
         Ok((made, name, registered))
     }
     ```
- 重現方式（Linux，在 repo 根目錄）：

  ```sh
  cargo build --release && F=$PWD/target/release/ferry
  mkdir -p .scratch/dst02/src
  for i in 1 2 3 4 5 6 7 8; do head -c 200000000 /dev/urandom > .scratch/dst02/src/big$i; done
  $F copy --no-ui .scratch/dst02/src .scratch/dst02/dst & P=$!
  sleep 1
  for s in $(pgrep -f "$F server"); do
    ls -l /proc/$s/fd 2>/dev/null | grep -q dst02/src && kill -9 $s   # 只殺 Source 的 server
  done
  wait $P; echo "exit=$?"
  ls -A .scratch/dst02/dst | grep ferry-     # 現況：列出數個 .bigN.ferry-*
  ```

  單元測試草稿（放進 `src/destination.rs` 的 `mod tests`，可沿用其中的 `entry()` 輔助函式）：

  ```rust
  #[test]
  fn a_stream_that_ends_inside_a_record_leaves_no_temporary_file() {
      // Root, then "" with f (5 bytes) and big (1_000_000 bytes), then End.
      // Record stream: f whole, then big's header and 300_000 bytes, then nothing
      // (no Header::End): the connection broke.
      let error = run(&mut &packets[..], &mut Vec::new(), &options).unwrap_err();
      let left: Vec<String> = std::fs::read_dir(&root).unwrap()
          .map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
      assert!(left.iter().all(|name| !name.contains(".ferry-")), "{left:?}");
  }
  ```
  第二個變體：5 個完整的 record 之後、`Header::End` 之前，送一個 `Control::Error { message }` packet。
- 修法：
  - 方案 A（建議）：讓登記成為擁有暫存檔的 guard。沒有被明確解除就被 drop 時，刪除檔案。
    - `src/interrupt.rs`：

      ```rust
      pub struct Registered { id: u64, armed: bool }

      impl Registered {
          /// The file took its final name: it is no longer to be removed.
          pub fn disarm(&mut self) { self.armed = false; }
      }

      impl Drop for Registered {
          fn drop(&mut self) {
              let entry = TEMPORARY.lock().ok().and_then(|mut f| f.as_mut()?.remove(&self.id));
              if self.armed && let Some((directory, name)) = entry {
                  let _ = rustix::fs::unlinkat(&directory, OsStr::from_bytes(&name), AtFlags::empty());
              }
          }
      }
      ```
      `create` 中 `register` 失敗時，先 `unlinkat(directory, name)` 再回錯。若同時做了 DST-03 的「登記表共用 `Arc<OwnedFd>`」，`register` 就不會再因 EMFILE 失敗。
    - `src/destination.rs`：
      - `Ready` 的欄位 `_registered` 改名為 `registered`。`rename_into_place` 改收 `&mut Ready`，`fsys::rename` 成功後**立刻** `ready.registered.disarm()`，然後才做根目錄是檔案時的 `fsync`（`1578-1580`），以免 `fsync` 失敗時把已就位的檔案當成暫存檔處理。`rename_batch` 中改為傳 `&mut done.ready`。
      - `apply_entry` 的 `Verdict::Content` 分支：`fsys::rename(&directory, &temporary, name)?` 成功後呼叫 `registered.disarm()`。原本失敗時的 `fsys::remove` 可以保留（重複刪除只會得到 `ENOENT`），也可以拿掉。
      - `write_file` 不必改：提早返回時，`opened` 中的 guard 會刪檔。1701-1704 的明確刪除可以保留。
    - 與訊號 thread 的互動：`remove_all` 會拿走整張表並刪檔，之後的 `Drop` 查不到項目，什麼都不做；行程接著就結束。`Drop` 中做 I/O 不需要 `GATE`。
    - 暫存名稱每次都唯一（`temporary_name` 含計數器），所以重新命名之後，不會有別的檔案剛好取得同一個暫存名稱而被誤刪。
  - 方案 B（可加做）：串流在兩個 record 之間出錯時，先 `flush(&mut batch)` 把已驗證、已 `fsync` 的檔案重新命名就位，再回報錯誤，省下重送。這不破壞單檔原子性（這些檔案是完整的）；代價是失敗路徑變長，而且它們所在的目錄不會收尾，mtime 要等下一次執行才對。可選。
  - 方案 C（不建議）：在 `write_files` 的每個錯誤路徑手動清除。很容易再漏掉新的路徑。
  - 不處理：Destination 本身被 `SIGKILL` 或當機時留下的暫存檔，這是設計上接受的。之後的執行要不要認出並刪除舊的暫存名稱，需另外設計（使用者也可能有剛好長這樣的檔案），不在本項範圍。
- 要加的測試：
  - `src/interrupt.rs` 的 `mod tests`：`a_registered_file_not_renamed_is_removed_when_dropped`（建立、登記、drop → 檔案不在）、`a_disarmed_file_is_kept`（建立、登記、`disarm`、drop → 檔案還在）。表是全域的，但以 ID 為鍵，與其他平行執行的測試互不干擾。
  - `src/destination.rs` 的 `mod tests`：上面草稿中的兩個測試（截斷的 record、完整 record 之後的 `Control::Error`），`Options` 的 `dry_run: false`。
  - 可選，`tests/copy.rs`（`#[cfg(target_os = "linux")]`）：如重現方式，以 `/proc/*/fd` 找出 Source 的 server 並 `kill -9`，檢查目的地沒有 `.ferry-`。依時間點而定，可能不穩，必要時以大檔放慢傳輸。
- 驗收條件：
  - [ ] 上述單元測試通過：串流截斷、Source 回報錯誤之後，目的地沒有任何 `.ferry-` 名稱。
  - [ ] 重現方式中殺掉 Source 的 server 後，目的地沒有暫存檔。
  - [ ] 正常完成的傳輸，所有檔案都在最終名稱上（沒有被 guard 誤刪）；`cargo test` 全部通過。
  - [ ] Ctrl-C 的行為不變：client 收到 SIGINT 後，目的地沒有暫存檔。
- 文件要同步的地方：`docs/design/overview.md`「Every file is complete, correct, and durable」第 1 點（`overview.zh-TW.md`「每個檔案都完整正確並持久保存」的「以檔案為單位的原子性」）：改成「執行中斷或失敗時（包括 Source 失敗、connection 中斷），Destination 都會刪除它未完成之檔案的暫存檔；只有 Destination 本身當機或被終止，才可能留下暫存檔」。
- 相關項目：DST-03（EMFILE 是觸發途徑之一，`register` 失敗的漏洞也在那裡）、DST-04 與 DST-05（它們讓傳輸以錯誤結束，也會走到這裡）。

---

### DST-03：每個等待重新命名的檔案占 3 個 fd，開檔上限 768 以下時大量檔案失敗

- 嚴重度：高
- 驗證狀態：已實測重現（Linux，以 `ulimit -n` 模擬較低的上限）。
  - 本審查：同一目錄中 400 個小檔案，`ulimit -n 256`：324 個失敗（`cannot copy fNNN: Too many open files (os error 24)`），結束碼 1，兩次分別殘留 26 與 31 個暫存檔。`ulimit -n 1024` 時全部成功；1,500 個目錄、每個 3 個檔案，在 1024 時也成功。
  - 彙整者：`ulimit -n 256`，2,000 個小檔案中 1,921 個失敗，目的地殘留 62 個暫存檔。
  - Source 審查者：8,000 個 100 B 的檔案，`ulimit -n` 256 / 512 / 768 時分別有 7,922 / 7,836 / 7,751 個失敗；896 以上全部成功。
  - macOS 的預設軟上限是 256（`launchctl limit maxfiles`，以及 Terminal 與 ssh 工作階段的 `ulimit -n`），所以這在 macOS 上預設就會發生。沒有在 macOS 上實測。
  - 另外，Source 的掃描每個開啟中的祖先目錄占一個 fd，深 400 層的目錄樹在 `ulimit -n 256` 時約在第 230 層失敗；那部分由 Source 審查處理。Destination 這邊的比較（`src/compare.rs` 的 `Tree.open`）同樣每層祖先保留一個 fd，深的目錄樹也會在 Destination 用掉 fd（已核對程式碼）。
- 位置：
  - `src/destination.rs:1259-1272` `Ready`：`directory: OwnedFd`、`file: File`、`_registered`
  - `src/destination.rs:1691-1699` `write_file` 中兩次 `try_clone`
  - `src/interrupt.rs:214-223` `register` 再 dup 一次目錄 fd
  - `src/destination.rs:1326` `BATCH = 256`；`1448-1450` 滿 256 個才 flush
  - `src/destination.rs:1843-1857` `target_directory`：每個檔案重新走一次路徑、開一個新的目錄 fd
  - `src/compare.rs:74-101`、`205-272` `Tree`：每層祖先保留一個 fd
  - 整個 repo 沒有呼叫 `setrlimit`（`grep -rn RLIMIT src` 沒有結果），Rust 標準函式庫也不會調高。
- 現象：在 macOS 上，或在開檔上限較低的 Linux 上，複製許多小檔案時，大多數檔案以 `Too many open files` 失敗，並經由 DST-02 留下暫存檔。
- 根本原因：
  1. 每個寫完、等待批次重新命名的檔案（`Ready`）占 3 個 fd：

     ```rust
     // src/destination.rs:1691-1699（write_file）
     Ok(Written::Ready(Ready {
         directory: directory.try_clone()?,   // 1：目錄 fd 的副本
         name: name.clone(),
         temporary: temporary.clone(),
         file: file.try_clone()?,             // 2：檔案 fd 的副本
         device,
         is_root: target.is_root,
         _registered: registered,             // 3：register() 中 dup 的目錄 fd
     }))
     ```
     即使同一目錄中的檔案，也各自持有目錄 fd，因為 `target_directory` 每個檔案都重新以 `open_path` 走一次並開新的 fd。
  2. 批次要湊滿 `BATCH = 256` 才重新命名，所以最多 255 × 3 = 765 個 fd 在等待；再加上 16 個寫入中的 writer（每個有目錄 fd、暫存檔 fd、登記用的 dup）、比較用的祖先目錄 fd、等待中各段持有的目錄 fd、transport 的 pipe 與 socket，以及標準輸入、輸出、錯誤，總數超過 850。這與實測的分界（768 失敗、896 成功）相符。
  3. 用盡之後，`create_temporary`、`try_clone` 或 `register` 回 EMFILE，該檔案失敗；`register` 失敗時暫存檔不會被刪（DST-02）。
  4. 沒有任何地方把軟上限調到硬上限。
- 重現方式（在 repo 根目錄，不需 root）：

  ```sh
  cargo build --release && F=$PWD/target/release/ferry
  mkdir -p .scratch/dst03/src && for i in $(seq 1 400); do echo $i > .scratch/dst03/src/f$i; done
  (ulimit -n 256; $F copy --no-ui .scratch/dst03/src .scratch/dst03/dst); echo "exit=$?"
  ls .scratch/dst03/dst | wc -l              # 現況：遠少於 400
  ls -A .scratch/dst03/dst | grep -c ferry-  # 現況：數十個暫存檔
  ```
- 修法：三部分都做；第 1 部分最重要，也最簡單。
  1. **啟動時調高開檔上限。** 在 `cli::run` 的最開頭（client 與 `ferry server` 都經過這裡）呼叫一個新函式，例如 `raise_open_file_limit()`：
     - Linux：軟上限設為硬上限（systemd 預設的硬上限常是 524288）。
     - macOS：硬上限常是 `RLIM_INFINITY`，但 `setrlimit` 的軟上限超過 `kern.maxfilesperproc` 會回 `EINVAL`。取 `min(硬上限, sysctlbyname("kern.maxfilesperproc"))`；查不到時用 `OPEN_MAX`（10240）；還是 `EINVAL` 就放棄。
     - 調不上去不是錯誤，但若最後軟上限低於某個門檻（例如 1024），在開始時警告一次。
     - 子行程（`ssh`、`getent`）會繼承調高後的上限。有些程式會依軟上限關閉所有 fd，上限極大時會很慢；Go 1.21 起會為子行程還原原本的上限。建議同樣在 `Command` 的 `pre_exec`（例如 `interrupt::unblock_in_child` 旁）還原原本的軟上限。
     - 先確認沒有任何地方對大於 1023 的 fd 使用 `select()`：目前只看到 crossterm 的 `event::poll`（`src/dashboard.rs:144`），在 Linux 上它走 mio；macOS 上的實作請再確認。

     ```rust
     pub fn raise_open_file_limit() {
         let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
         // SAFETY: `limit` is a valid rlimit for the call to fill.
         if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 { return; }
         let mut wanted = limit.rlim_max;
         #[cfg(target_os = "macos")]
         { wanted = wanted.min(max_files_per_process().unwrap_or(10240)); }
         if wanted > limit.rlim_cur {
             limit.rlim_cur = wanted;
             // SAFETY: as above.
             unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) };
         }
     }
     ```
  2. **每個等待重新命名的檔案最多占 1 個 fd。**
     - 共用目錄 fd：在 `State` 加 `directories: HashMap<Vec<u8>, Weak<OwnedFd>>`（目錄路徑 → 已開啟的目錄）。`target_directory` 先查表並 `upgrade`，失敗才 `open_under` 並放入。`Ready.directory` 改為 `Arc<OwnedFd>`。每次 `flush` 後以 `retain(|_, w| w.strong_count() > 0)` 清掉失效的項目。
       不要把 fd 存在 `Target` 或 `Open` 裡：`targets` 可能同時有多達 10 萬個等待中的檔案（Source 的 backlog 上限），分散在很多目錄，那樣會持有大量 fd。用 `Weak` 的話，只有寫入中與批次中的檔案才會讓目錄 fd 活著。
       重用較早開啟的目錄 fd，語意與設計的「刪除的對象，就是列出的那個目錄」一致：路徑途中被換掉時，寫入的仍是當初從根目錄 fd 以 `O_NOFOLLOW` 走到的那個目錄。
     - 登記表共用同一個 `Arc`：`interrupt::Temporaries` 改為 `HashMap<u64, (Arc<OwnedFd>, Vec<u8>)>`，`create` 與 `register` 改收 `&Arc<OwnedFd>`，只 clone `Arc`，不 dup。這也消除了 DST-02 中 `register` 遇到 EMFILE 的漏洞。
     - 檔案 fd：只有 macOS 的屏障需要它，而且每台磁碟機一個就夠。`Ready.file` 改為 `Option<File>`。在 Linux 上，或 `--no-fsync` 時，writer `fsync` 之後就不保留。在 macOS 上，`write_files` 的 `settle` 把檔案放進批次時，若批次中已有同一 `device` 且仍持有 fd 的檔案，就把這個的 `file` 設為 `None`。`rename_batch` 只對 `Some` 的檔案下屏障。
  3. **依 fd 預算限制批次大小**：啟動時算出 `budget = (軟上限 - 保留量) / 每檔 fd 數`（保留量要涵蓋 writer、比較的祖先目錄、transport），`settle` 中的 flush 條件改為 `batch.len() >= BATCH.min(budget)`。
  - 取捨：只做第 1 部分，就能解決大多數環境（macOS 可到 10240，Linux 硬上限通常很高），但對硬上限本身就低的環境（例如硬上限 1024 的容器）無效。第 2 部分永久降低用量；第 3 部分保證批次本身不會用盡 fd。
- 要加的測試：
  - `tests/copy.rs`：`many_small_files_copy_under_a_low_open_file_limit`。以 `Command` 執行 `ferry copy`，在 `pre_exec` 中 `setrlimit(RLIMIT_NOFILE)`；複製 2,000 個 100 B 的檔案；斷言結束碼 0、2,000 個檔案都在、沒有 `.ferry-`。兩個變體：
    - 軟上限 256、硬上限不變：驗證第 1 部分。
    - 軟、硬上限都是 256：驗證第 2、3 部分。
    Linux 與 macOS 都跑。
  - 調高上限的函式：單元測試，呼叫後 `getrlimit` 的軟上限等於預期值（硬上限或 macOS 的上限）。
  - 可選，`src/testkit/`：在 `ferry test --scale` 的說明或 CI 中，以 `ulimit -n 256` 跑一次小檔案的規模測試。
- 驗收條件：
  - [ ] 重現方式在 `ulimit -n 256` 下結束碼 0，400 個檔案都在，沒有暫存檔。
  - [ ] 軟、硬上限都是 256 時，2,000 個小檔案的複製成功（第 2、3 部分）。
  - [ ] 在 macOS 預設的 shell 中，複製 8,000 個小檔案成功。
  - [ ] 子行程（`ssh`）拿到的是原本的軟上限（若實作了還原）。
- 文件要同步的地方：
  - `docs/design/pipeline.md`「6. Write」（`pipeline.zh-TW.md`「6. 寫入」）：記下每批最多 256 個檔案、每個等待中的檔案最多占一個 fd、批次會依開檔上限縮小。
  - `docs/cli.md`「Environment」（`cli.zh-TW.md`「環境」）或「Options」附近：說明 Ferry 啟動時會把開檔上限調到系統允許的最大值。
- 相關項目：DST-02（EMFILE 會留下暫存檔）、DST-09（共用目錄 fd 也省掉每個檔案的路徑走訪與鎖）。Source 掃描每層祖先占一個 fd 的問題由 Source 審查處理。

---

### DST-04：Destination 有一個目錄打不開或一個項目 stat 失敗，整個傳輸就中止

- 嚴重度：中
- 驗證狀態：已實測重現。
  1. `ferry copy --dry-run src2 /var/cache`，其中 `src2` 含 `ldconfig/aux-cache` 與 `zz`，而 `/var/cache/ldconfig` 屬於 root、模式 0700。輸出 `ldconfig/`、`zz` 之後是 `ferry: the Destination on this machine failed: compare ldconfig/: Permission denied (os error 13)`，結束碼 1。
  2. 不需 root、也不碰系統目錄的版本：目的地放一個自己擁有、模式 000 的目錄，以 `--dry-run` 執行（dry run 不會補權限）。同樣中止：`compare locked/: Permission denied`。
  非 dry run 時，Ferry 會替自己擁有的目錄補上權限，所以第 2 種不會重現；但別人擁有的目錄（例如在使用者目錄樹中以 `sudo` 建立的 root 目錄、共用目錄樹中別人的私人目錄）情況與第 1 種相同。這一點已核對程式碼，沒有以第二個使用者實測。
- 位置：
  - `src/compare.rs:344-361` `Comparer::part`（351：`let directory = directory?;`）
  - `src/compare.rs:363-376` `Comparer::entry`：除了 `NOENT`、`NOTDIR`、`LOOP`，任何 stat 錯誤都回 `Err`
  - `src/compare.rs:145-149`、`275-282`：打不開的目錄成為 `Opened::Failed`，之後每段都回 `Err`
  - `src/destination.rs:457-460` stat pool 呼叫 `comparer.part`
  - `src/destination.rs:895-900` `apply_compared` 中的 `verdicts.with_context(...)?`
  - `src/destination.rs:506-519` `take_compared(...)?` 讓 `read_packets` 失敗
- 現象：目的地目錄樹中只要有一個這樣的目錄，或一個項目 stat 失敗（目錄可讀但不能進入時的 `EACCES`、`EIO`、`EOVERFLOW` 等），整個傳輸就以 `the Destination … failed: compare X/: …` 結束。還沒套用的一切都不會複製，寫到一半的暫存檔也會留下（DST-02）。這違反管線「7. 結果」的「單一檔案失敗不會中止傳輸」。
- 根本原因：
  1. 比較時，Destination 的掃描 thread 開不了某個目錄，記為 `Opened::Failed`，該目錄每一段的 `Prepared.directory` 都是 `Err`。
  2. `Comparer::part` 的第一行就把這個錯誤整段傳出去；每個項目的 stat 錯誤也一樣，任何一個項目失敗，整段都是 `Err`：

     ```rust
     // src/compare.rs:351-360
     let directory = directory?;
     entries
         .iter()
         .zip(present)
         .map(|(entry, &has)| match &directory {
             _ if entry.kind == Kind::Unreadable => Ok(Verdict::Same),
             Some(directory) if has => self.entry(&**directory, entry, ownership),
             _ => Ok(Verdict::Content),
         })
         .collect()
     ```
  3. main thread 在 `apply_compared` 中以 `?` 把它變成整個傳輸的錯誤：

     ```rust
     // src/destination.rs:897-900
     let mut state = state.lock().expect("not poisoned");
     let verdicts = verdicts.with_context(|| {
         format!("compare {}", String::from_utf8_lossy(&display(&path, true)))
     })?;
     ```
- 重現方式（在 repo 根目錄，非 root）：

  ```sh
  cargo build --release && F=$PWD/target/release/ferry
  mkdir -p .scratch/dst04/src/locked .scratch/dst04/dst/locked
  echo a > .scratch/dst04/src/locked/f; echo b > .scratch/dst04/src/zz
  chmod 000 .scratch/dst04/dst/locked
  $F copy --dry-run --no-ui .scratch/dst04/src .scratch/dst04/dst; echo "exit=$?"
  # 現況：locked/、zz，然後 "the Destination on this machine failed: compare locked/: Permission denied"
  chmod 755 .scratch/dst04/dst/locked
  ```
- 修法：
  - `src/compare.rs`：`Comparer::part` 改為逐項回傳結果，例如 `Vec<Result<Verdict, String>>`：
    - 目錄是 `Err` 時，除了 `Kind::Unreadable`（仍是 `Ok(Verdict::Same)`），每個項目都是 `Err(format!("cannot open its directory: {error}"))`。
    - 單一項目 stat 失敗，只有那一項是 `Err(format!("cannot compare it: {error}"))`。
  - `src/destination.rs`：
    - `Compared.verdicts` 改成新的型別。`apply_compared` 中把結果拆成兩份：失敗的項目當成 `Verdict::Same`（什麼都不做：不需要傳送，也不算多餘，如同 Source 讀不到的項目），並收集 `(id, message)`，併入現有的 `failures`（`tell`、`Results::Failed`、`Count::Failed`）。
    - dry run 也要回報這些失敗（目前 dry run 不執行 `apply_directory`，所以失敗清單永遠是空的），讓 `--dry-run` 的結束碼反映它們。
    - 比較失敗的目錄項目照常 `announce`（它的各段之後仍會來，`Check` 也期待它們），但不標記收尾。它的各段照常交給 `Tree` 比較；`Opened::Failed` 之下的子目錄本來就會是 `Failed`。
    - 減少雜訊：整個目錄打不開時，只 `tell` 一次「cannot compare in DIR/: …」，每個項目仍送 `Results::Failed`，但不每項各印一行。Source 會以 `Results::Failed` 計數（`src/source.rs:383`）。
  - 多餘的項目：`Opened::Failed` 或列不出的目錄，`extras` 本來就是 `Err`（`src/compare.rs:183-191`），會回報 `NotDeleted` 而不刪任何東西。保留現狀。
  - 根目錄本身的比較（`src/destination.rs:574-576`）失敗時仍可整個中止，因為沒有根目錄什麼都做不了。
  - 平台：Linux 與 macOS 的錯誤碼相同，沒有特別差異。
- 要加的測試：
  - `src/compare.rs` 的 `mod tests`：`a_directory_that_could_not_be_opened_fails_each_entry_alone`。呼叫 `comparer.part(Err(io::Error::from_raw_os_error(libc::EACCES)), &entries, &present, &Ownership::here())`；每個一般項目都是 `Err`，`Kind::Unreadable` 是 `Ok(Verdict::Same)`。
  - `tests/dry_run.rs`：`a_destination_directory_that_cannot_be_read_fails_alone`（root 時略過）。如重現方式；斷言結束碼 1，列出 `zz`，標準錯誤輸出有「cannot compare in locked/」，沒有「the Destination … failed」。
  - 別人擁有的目錄需要第二個使用者，不放進自動測試；可在有 sudo 的機器上手動以 `sudo mkdir -m 700 dst/rootdir` 確認非 dry run 也只失敗那一部分。
- 驗收條件：
  - [ ] 重現方式結束碼 1，但標準輸出列出 `zz`，沒有 `the Destination … failed`。
  - [ ] 非 dry run、目的地含一個 root 擁有的 0700 目錄：其餘檔案都複製完成，只有那個目錄的項目回報失敗，Destination 送出 `Control::Done`。
  - [ ] `sync` 時，打不開的目錄中什麼都不刪，回報 `NotDeleted`。
  - [ ] `cargo test` 全部通過。
- 文件要同步的地方：`docs/design/pipeline.md`「2. Compare」（`pipeline.zh-TW.md`「2. 比較」）：加一點，Destination 無法開啟、列出或 stat 的目錄或項目，會單獨回報為失敗，其餘照常比較與複製。
- 相關項目：DST-02（目前中止時會留下暫存檔）、DST-05（同一種「一處失敗、整體中止」的問題，在收尾）、DST-14。

> 2026-10-20 補記：WP-07 照修法做。`Comparer::part` 改為回傳 `Verdicts`（每項的 verdict、無法比較的 `(id, message)`、目錄打不開的原因），無法比較的項目當成 `Verdict::Same`，不傳送也不算多餘；打不開的目錄只說一次「cannot compare in DIR/: …」，每個項目仍送 `Results::Failed`，dry run 也一樣。測試：`a_directory_that_could_not_be_opened_fails_each_entry_alone`、`tests/dry_run.rs` 的 `a_destination_directory_that_cannot_be_read_fails_alone`。驗收條件「非 dry run、目的地含 root 擁有的 0700 目錄」以 `sudo mkdir -m 700` 手動確認：其餘都複製完成，只有那個目錄的一項失敗，結束狀態 1。`sync` 時打不開的目錄不刪任何東西，這部分沒有改動，沒有另外測。

---

### DST-05：一個目錄收尾失敗，之後的目錄全部不再收尾，整個傳輸以錯誤結束

- 嚴重度：中
- 驗證狀態：已實測重現（以人為的錯誤觸發）。送出一個 manifest：根目錄下有 `a`（mtime 的奈秒是 2,000,000,000，使 `futimens` 回 `EINVAL`）、`b`（模式 0750）、`c`（模式 0711），三者 mtime 都是 1,600,000,000。結果 `run` 回 `Err("finish a/: Invalid argument (os error 22)")`；`b`、`c` 與根目錄的 mtime 都停在當下，根目錄的模式仍是 umask 之後的 775，沒有設成 0755。實際情境中的 `EPERM`（目錄屬於別人）需要第二個使用者，沒有實測，是依程式碼推得。
- 位置：
  - `src/destination.rs:1738-1749` `finish_as_done`（1746 的 `?`）
  - `src/destination.rs:1780-1807` `finish_directory`：chown、chmod、utimens、fsync 放在同一個閉包中，第一個失敗就跳過其餘
  - `src/destination.rs:1755-1776` `finish_directories`、`334-344`：錯誤成為整個寫入 thread 的錯誤，送出 `Control::Error`
  - `src/destination.rs:324` 收尾的 thread 在這裡啟動（沒有 `Mux`）
  - `src/compare.rs:415-428` `attributes_differ`：模式不同就算差異，不論能不能設定
  - `src/destination.rs:1099-1102`、`1113-1115`：目錄被標記為要收尾的地方
- 現象：目的地目錄樹中有一個不屬於執行者、但可寫入的目錄（例如叢集上群組共用的專案目錄），或目的地是 FAT、exFAT、SMB 等 chmod 或 utimens 會失敗的檔案系統，或傳輸期間有目錄被換成符號連結（`open_under` 以 `O_NOFOLLOW` 開啟失敗），收尾就停在那個目錄。之後收尾的每個目錄都保留錯的權限與 mtime，也沒有 `fsync`。writer 卻繼續把所有檔案寫完，最後整個傳輸以 `finish X: Operation not permitted` 失敗，而不是回報一項失敗。由於比較把「無法設定的模式差異」也算進去，同樣的目錄樹每次執行都會這樣失敗。
- 根本原因：
  1. 收尾 thread 的迴圈對每個目錄呼叫 `finish_directory`，遇到錯誤就以 `?` 結束整個 thread：

     ```rust
     // src/destination.rs:1738-1749
     fn finish_as_done(
         root: &Path,
         state: &Mutex<State>,
         done: mpsc::Receiver<(Vec<u8>, Entry)>,
         fsync: bool,
     ) -> Result<()> {
         let _thread = profile::thread("finisher");
         for (path, entry) in done {
             finish_directory(root, state, &path, &entry, fsync)?;
         }
         Ok(())
     }
     ```
  2. 之後要收尾的目錄仍由 `done_if_so` 以 `let _ = finishing.send(...)` 送出，接收端已經不在，錯誤被忽略，於是這些目錄靜靜地沒有收尾。
  3. 最後 `finish_directories` 的 `finisher.join()` 回傳這個錯誤，寫入 thread 送出 `Control::Error`，整個傳輸失敗。
  4. `finish_directory` 中任一步失敗（例如 chmod 的 `EPERM`），同一目錄的 mtime 與 `fsync` 也被跳過。
  5. `attributes_differ` 只對擁有者與群組判斷「能不能設定」，對模式則不判斷：

     ```rust
     // src/compare.rs:422-427
     let mode_differs = !entry.kind.is_symlink() && mode != entry.mode;
     let wanted_uid = ownership.uid(entry.uid);
     let wanted_gid = ownership.gid(entry.gid);
     let user_differs = wanted_uid != uid && ownership.can_set_user(wanted_uid, uid);
     let group_differs = wanted_gid != gid && ownership.can_set_group(wanted_gid);
     mode_differs || user_differs || group_differs
     ```
     所以別人的目錄只要模式不同，就每次都被標記為要收尾，每次都失敗。
- 重現方式：測試草稿，放進 `src/destination.rs` 的 `mod tests`（沿用其中的 `entry()`、`run_on()`）：

  ```rust
  #[test]
  fn a_directory_that_cannot_be_finished_fails_alone() {
      let mut bad = entry(1, b"a", Kind::Directory, 0);
      bad.mtime.nanoseconds = 2_000_000_000; // futimens: EINVAL
      // Root; "" with a and b; "a" empty; "b" empty; End; a data packet with Header::End.
      // 現況：run() 回 Err("finish a/: …")，b 與根目錄沒有收尾。
      // 修好後：run() 成功，有一個 id 1 的 Results::Failed，b 與根目錄的 mtime 是 1_600_000_000。
  }
  ```
  若 DST-15 之後讓 `manifest::Check` 拒絕這種奈秒，這個觸發方式就不能用了。屆時改成直接測 `finish_as_done`：送進一個不存在的路徑（`open_under` 失敗），再送一個存在的目錄，斷言後者有收尾。
- 修法：
  1. `finish_as_done` 不在錯誤時返回。每個目錄失敗時：`state.failures += 1`、`reporter.add(Count::Failed, 1)`、`tell` 出「cannot finish DIR/: …」、`mux.message(&Results::Failed { id: entry.id, message })`，然後繼續下一個。要做到這點，需在 `src/destination.rs:324` 啟動收尾 thread 時傳入 `mux.clone()` 與 `reporter.clone()`。只有協定層的錯誤（`mux.message` 失敗）才回傳 `Err`。
  2. `finish_directory` 的各步驟各自執行：chown（只在能設定時）、chmod、utimens、`fsync` 都嘗試；記下第一個錯誤，但不跳過其他步驟。chmod、utimens 在執行者不是擁有者、也不是 root 時得到 `EPERM`，依概觀「若 Ferry 缺少設定某個項目的權限，它會略過該項並發出警告」，記為彙總的警告（在 `Warning` 加一種，例如 `Warning::Attributes`），不算失敗。`fsync` 失敗仍算失敗，因為它關係到持久保存。
  3. `src/compare.rs` 的 `verdict` 與 `attributes_differ`：只有在能設定時才把模式差異算進去。在 `Ownership` 存下 `euid`，加 `can_set_mode(current_uid) -> bool`（`root || current_uid == euid`）。mtime 也一樣：以明確時間呼叫 `utimensat` 需要是擁有者（或有 `CAP_FOWNER`），所以不能設定時，`Verdict::Times` 應改為 `Verdict::Same`。這與「無法設定的擁有者不算差異」是同一個原則。
  - 邊界情況：為了進入而放寬過權限（`let_in`）的目錄若收尾失敗，放寬的權限會留著，回報時要說明（另見 DST-11）。
  - 平台：Linux 與 macOS 上，chmod 與以明確時間呼叫 utimens 都需要是擁有者。Linux 的 vfat、exfat 在沒有以 `quiet` 掛載時，無法表示的模式會回 `EPERM`；macOS 上 FAT、exFAT 的 chmod 多半默默忽略。
- 要加的測試：
  - `src/destination.rs` 的 `mod tests`：上面的 `a_directory_that_cannot_be_finished_fails_alone`；另加一個直接測 `finish_as_done` 的版本（不存在的路徑之後接一個存在的目錄）。
  - `src/compare.rs` 的 `mod tests`：`a_mode_that_cannot_be_set_is_not_a_difference`。以 `std::mem::zeroed::<Stat>()` 建立一個 `st_uid` 不是 euid 的 `Stat`、模式不同，搭配一個非 root 的 `Ownership`，`attributes_differ` 必須回 `false`。root 時略過。
- 驗收條件：
  - [ ] 一個目錄收尾失敗時，其他目錄都照常收尾（mode、mtime 正確，有 `fsync`），該目錄回報為一項失敗，傳輸送出 `Control::Done`，結束碼 1。
  - [ ] 目的地含一個別人擁有、群組可寫入、模式與 Source 不同的目錄時，非 root 的執行不再失敗；只有一則彙總的警告，而且第二次執行時，它不再被列為有差異。
  - [ ] `cargo test` 全部通過，包含 `directories_are_finished_as_they_are_done_deepest_first`。
- 文件要同步的地方：
  - `docs/design/pipeline.md`「8. Finish」（`pipeline.zh-TW.md`「8. 收尾」）：補上收尾失敗的目錄單獨回報為失敗，其他目錄照常收尾；權限不足時改為警告。
  - `docs/design/overview.md`「Behaviour of `-a` metadata」（`overview.zh-TW.md`「`-a` 中繼資料的行為」）：「無法設定的擁有者不算差異」擴充為也包含不屬於自己的項目的權限與 mtime。
  - `docs/cli.md`「What is compared」（`cli.zh-TW.md`「比較什麼」）：「若其權限或 Ferry 能設定的擁有者不同」改為「若 Ferry 能設定的權限或擁有者不同」。
- 相關項目：DST-04（同類問題，在比較）、DST-06（同類問題，在檔案）、DST-14、DST-15、DST-11。

> 2026-10-20 補記：WP-07 照修法做，與修法不同之處：重現方式用的「mtime 奈秒 2×10^9，`futimens` 回 `EINVAL`」現在依 DST-06 的分類算成警告（無法設定的時間），不再讓目錄失敗，所以測試改用修法提到的備案，直接測 `finish_as_done`：一個已經不存在的目錄之後接一個存在的目錄（`a_directory_that_cannot_be_finished_fails_alone`）。收尾失敗以 `file_failed_in` 回報，訊息為「could not be finished: …」；磁碟滿或唯讀時（D6）停止傳輸。擁有者、權限、mtime 與 `fsync` 各自嘗試；權限與 mtime 只在能設定時才算差異（`Ownership::can_set_mode`，`a_mode_that_cannot_be_set_is_not_a_difference`）。收尾失敗時，曾為了進入而放寬的權限不會還原：權限本來就設不上，還原也會失敗。

---

### DST-06：檔案的擁有者、權限或 mtime 設不上時，已驗證的檔案整個被丟棄

- 嚴重度：中
- 驗證狀態：已核對程式碼（未實測；需要以 root_squash 匯出的 NFS，或 Linux 上的 FAT、exFAT 掛載）。
- 位置：
  - `src/destination.rs:1686` `apply_file_attributes(&file, &target.entry, state)?;`
  - `src/destination.rs:1708-1723` `apply_file_attributes`
  - `src/destination.rs:1220-1235` `set_owner`：root 一律嘗試 chown
  - `src/owners.rs:54-62` `can_set_user`、`can_set_group`
  - `src/destination.rs:1150-1162`、`1191-1205`：只需屬性的項目與特殊檔案，同樣以 `?` 傳遞這類錯誤
- 現象：以 `sudo ferry copy` 或 `--remote-sudo` 寫進以 root_squash 匯出的 NFS（叢集上很常見），`fchown` 回 `EPERM`，每個檔案都以「cannot copy X: Operation not permitted」失敗，暫存檔被刪除，一個檔案都沒複製到。寫到 Linux 上沒有以 `quiet` 掛載的 FAT、exFAT，或某些 FUSE、SMB 檔案系統時，chmod 失敗的效果相同。rsync 遇到這些情況會保留檔案並回報屬性錯誤；Ferry 的概觀也說「若 Ferry 缺少設定某個項目的權限，它會略過該項並發出警告……檔案本身仍會傳輸」。
- 根本原因：
  1. 檔案的內容驗證通過後，`write_file` 在 `fsync` 與重新命名之前套用屬性，任何錯誤都讓這個檔案失敗並刪除暫存檔：

     ```rust
     // src/destination.rs:1710-1723
     fn apply_file_attributes(file: &File, entry: &Entry, state: &Mutex<State>) -> Result<()> {
         let metadata = file.metadata()?;
         let ownership = Arc::clone(&state.lock().expect("not poisoned").ownership);
         set_owner(
             &ownership,
             metadata.uid(),
             metadata.gid(),
             entry,
             |uid, gid| fsys::set_owner(file, uid, gid),
         )?;
         fsys::set_mode(file, entry.mode & 0o7777)?;
         fsys::set_mtime(file, entry.mtime)?;
         Ok(())
     }
     ```
  2. `set_owner` 只在「這個行程不能設定」時（非 root 且不是同一使用者，或不屬於該群組）才略過；root 一律嘗試，而 root_squash 下 root 的 chown 會被拒絕。
- 重現方式：
  - root_squash：在一台以 `root_squash` 匯出 NFS 的機器上，`sudo ferry copy src /nfs/dst`。現況：每個檔案都失敗。
  - FAT（Linux，需要 polkit 允許 `udisksctl`）：

    ```sh
    truncate -s 64M .scratch/dst06/fat.img && mkfs.vfat .scratch/dst06/fat.img
    udisksctl loop-setup -f .scratch/dst06/fat.img && udisksctl mount -b /dev/loopN
    chmod 4755 .scratch/dst06/src/a; chmod 600 .scratch/dst06/src/b
    $F copy --no-ui .scratch/dst06/src /media/$USER/<label>/dst
    ```
  - 不需特殊環境的單元測試：`set_owner` 已經以閉包接收 chown，可傳入一個回傳 `EPERM` 的閉包，見「要加的測試」。
- 修法：
  - 加一個分類函式，例如 `fn is_unsettable(error: &io::Error) -> bool`：`EPERM`、`ENOTSUP`、`EOPNOTSUPP`（Linux 上兩者同值，rustix 可能有兩個同值常數，用 `||` 比較即可），以及 utimens 的 `EINVAL`（某些檔案系統無法表示該時間，例如 FAT 不能存 1980 年以前）。
  - `apply_file_attributes`：chown、chmod、utimens 各自執行，順序不變（先擁有者，再模式，最後 mtime）。遇到可略過的錯誤時，`state.warnings.add(Warning::Owner | Warning::Mode | Warning::Times, shown)` 並繼續；其他錯誤仍讓檔案失敗。之後照常 `fsync` 與重新命名。函式需多收一個 `shown` 參數，供警告列出路徑範例。
  - `apply_entry` 的 `Verdict::Attributes` 與特殊檔案分支採同樣的分類。
  - 可選最佳化：同一台裝置上第一次 chown 得到 `EPERM` 之後，記在 `State` 中，之後該裝置上的檔案不再嘗試。
  - 可另外考慮（與 rsync 不同，需要決定）：擁有者沒能設成 Source 的擁有者時，清掉 `S_ISUID`；群組沒能設定時，清掉 `S_ISGID`，以免產生一個由接收端使用者擁有、Source 上並不存在的 setuid 檔案。
  - 平台：Linux 的 vfat、exfat 在模式無法表示時回 `EPERM`（掛載時加 `quiet` 則默默成功）；macOS 上 FAT、exFAT、SMB 的 chmod 多半被默默忽略；NFS root_squash 兩個平台都會回 `EPERM`。
- 要加的測試：
  - `src/destination.rs` 的 `mod tests`：`an_owner_that_cannot_be_set_is_a_warning`。在 `set_owner` 改成能記錄警告之後（例如回傳一個結果，或收 `&mut Warnings`），以回傳 `io::Error::from_raw_os_error(libc::EPERM)` 的閉包呼叫它，斷言回傳 `Ok`，且 `Warnings` 中有一筆。
  - 分類函式的單元測試：`EPERM`、`EOPNOTSUPP` 可略過，`EIO`、`ENOSPC` 不可。
  - 手動：如重現方式，在 FAT 映像上複製，檔案都在，警告彙總為一則。
- 驗收條件：
  - [ ] chown、chmod、utimens 因權限或不支援而失敗時，檔案仍以驗證過的內容就位，並以彙總的警告回報。
  - [ ] `EIO`、`ENOSPC` 等其他錯誤仍讓該檔案失敗，並刪除暫存檔。
  - [ ] 屬性套用的順序不變：擁有者、模式、mtime。
  - [ ] `cargo test` 全部通過。
- 文件要同步的地方：
  - `docs/design/overview.md`「Behaviour of `-a` metadata」的「Privilege warnings are summarised」（`overview.zh-TW.md`「權限警告會彙總」）：列出警告的種類（擁有者、權限、mtime、特殊檔案）。
  - `docs/cli.md`「Output」（`cli.zh-TW.md`「輸出」）：說明這類警告不讓檔案失敗。
- 相關項目：DST-05（目錄的同類問題）、DST-14。

> 2026-10-20 補記：WP-07 照修法做。檔案、只需屬性的項目、特殊檔案與目錄收尾共用 `set_attributes`：擁有者、權限、mtime 依序各自嘗試；`EPERM`、`EOPNOTSUPP`、`ENOTSUP`，以及 mtime 的 `EINVAL`，記為彙總的警告（`Warning::Owner`、`Mode`、`Times`），檔案照樣就位；其他錯誤在三者都試過之後，回傳第一個。沒有做的：同一裝置第一次 `EPERM` 之後不再嘗試的最佳化；擁有者或群組沒能設定時清掉 setuid 或 setgid，這需要使用者決定，沒有做，行為與 rsync 相同。測試：`what_cannot_be_set_is_a_warning_and_the_rest_is_set`、`only_privilege_and_support_make_an_attribute_unsettable`。沒有在 FAT 或 root_squash 的 NFS 上實測。

---

### DST-07：這次執行建立的根目錄，其父目錄沒有 `fsync`

- 嚴重度：低
- 驗證狀態：已核對程式碼
- 位置：
  - `src/destination.rs:1030-1044` `apply_root`：以 `std::fs::create_dir(root)` 建立根目錄
  - `src/destination.rs:1811-1822` `make_durable`：只 `fsync`（macOS 上 `F_FULLFSYNC`）根目錄本身
  - 對照：`src/destination.rs:1576-1580` 根目錄是檔案時，會 `fsync` 它的父目錄
- 現象：`ferry copy src newdir` 回報成功之後不久斷電，在對目錄 `fsync` 不保證提交父目錄項目的檔案系統上（嚴格的 POSIX 語意，例如 btrfs 的 log tree），`newdir` 在父目錄中的項目可能消失，整棵複製好的目錄樹也就找不到了。ext4 與 XFS 通常會因為 journal 一併提交，但這不是保證。
- 根本原因：根目錄的建立是在其父目錄中新增一個項目；之後只有根目錄本身（以及其下的目錄）被 `fsync`，沒有任何地方 `fsync` 根目錄的父目錄。

  ```rust
  // src/destination.rs:1034-1037
  Err(error) if error.kind() == io::ErrorKind::NotFound => {
      std::fs::create_dir(root)
          .with_context(|| format!("create {}", root.display()))?;
  }
  ```
- 重現方式：無法以測試觀察斷電。以 strace 確認現況（Linux）：

  ```sh
  strace -f -e trace=mkdir,mkdirat,fsync $F copy --no-ui .scratch/dst07/src .scratch/dst07/new 2>&1 | grep -E 'mkdir|fsync'
  # 現況：根目錄的 mkdir 之後，沒有對其父目錄 fd 的 fsync
  ```
- 修法：
  - 在 `State` 加 `root_made: bool`，`apply_root` 中 `create_dir` 成功時設為 `true`。
  - `make_durable` 在 `fsync` 開啟時，若 `root_made`，先開啟根目錄的父目錄（與 `root_parent` 相同的規則：`Path::parent`，空的就用 `.`，沿用使用者給的路徑，可跟隨連結），`fsys::fsync(parent)`，再做原本的 `sync_durable`。在 macOS 上，接著的 `F_FULLFSYNC` 會把磁碟機快取一併清空。
  - 邊界情況：根目錄只在不存在時才建立，所以 `.`、`..` 不會走到這裡；`dst/` 這種結尾有斜線的路徑，`Path::parent` 會給出正確的父目錄。
- 要加的測試：`src/destination.rs` 的 `mod tests`：`a_root_made_is_remembered`（根目錄不存在時執行一次，斷言 `State.root_made`），以及父目錄路徑的輔助函式的單元測試（`a/b` → `a`、`b` → `.`、`b/` → `.`）。持久性本身以上面的 strace 手動確認。
- 驗收條件：
  - [ ] strace 中，根目錄的 `mkdir` 之後、傳輸結束之前，有一次對其父目錄的 `fsync`。
  - [ ] `--no-fsync` 時沒有這次 `fsync`。
  - [ ] 根目錄原本就存在時，行為不變。
- 文件要同步的地方：`docs/design/overview.md`「Every file is complete, correct, and durable」第 3 點（`overview.zh-TW.md`「預設即持久保存」）：補一句，這次執行建立的根目錄，其父目錄也會 `fsync`。
- 相關項目：無。
- **2026-10-21 補記（WP-19a）**：照修法做。`State.root_made` 是 `AtomicBool`（`State` 由各 stage 共用）；`make_durable` 在 `fsync` 開啟且 `root_made` 時，先開啟根目錄所在的目錄並 `fsync`，再做原本的 `sync_durable`。父目錄的規則抽成 `parent_of`，與 `root_parent` 共用。與「要加的測試」不同：沒有 `a_root_made_is_remembered`，因為 `run` 內部的 `State` 從外面看不到，`fsync` 本身也觀察不到；只加了 `the_directory_a_root_is_in_is_as_its_path_names_it`（`a/b`、`b`、`b/`、`/b`、`a/b/`）。驗收以 strace 在 fatcat 上手動確認（本機沒有 strace）：根目錄的 `mkdir` 之後開啟 `dst07` 並 `fsync`；`--no-fsync` 時沒有任何 `fsync`；根目錄已存在時不開啟它的父目錄。

---

### DST-08：manifest 檢查不要求目錄依深度優先的順序出現

- 嚴重度：低
- 驗證狀態：已實測重現。依序送出 `""`、`a`、`b`、`a/x` 的各段（`a/x` 在 `a` 的項目中宣告，裡面有一個要建立的符號連結）。`manifest::Check` 全部接受，`run` 回 `Ok`；`a/x/f` 建立了，但 `a/x` 從未收尾（mtime 停在當下），而 `a` 在 `a/x` 的內容建立之前就已經收尾。
- 位置：
  - `src/manifest.rs:110-121` `Check` 的欄位（`expected: HashSet<Vec<u8>>`）
  - `src/manifest.rs:141-201` `Check::directory`：只檢查「已宣告、還沒來過」
  - `src/destination.rs:210-224` `State::enter`：假設深度優先，把不是祖先的目錄都當成已離開
  - `src/destination.rs:194-198` `file_needed`：目錄已不在 `open` 中時默默忽略
  - `src/compare.rs:227-231` `Tree::enter`：找不到父目錄時把整個堆疊彈空，子目錄成為 `Opened::Missing`
- 現象：只有 Source 有 bug 或惡意時才會發生。目錄太早收尾或永遠不收尾；順序錯亂之後的目錄被當成不存在，裡面的一切都重送，`sync` 時也不刪它們多餘的項目。影響停留在根目錄之下，而且只是中繼資料；但設計說 manifest 在被採取行動之前會先檢查，而 `State` 與 `Tree` 都依賴深度優先的順序。
- 根本原因：
  1. `Check` 只記一個「已宣告、還沒來」的集合，任何已宣告的目錄都可以在任何時候出現：

     ```rust
     // src/manifest.rs:154-161
     None => {
         ensure!(
             self.expected.remove(path),
             "the directory {:?}, which is not one announced, or came before",
             shown()
         );
         None
     }
     ```
  2. `State::enter` 在進入 `b` 時把 `a` 彈出並離開（`leave`），`a` 中尚未出現的 `a/x` 也被標記為已離開而收尾、移出 `open`。之後 `a/x` 的段到達，它的檔案在 `file_needed` 中找不到目錄而不被計數，它也永遠不會再收尾：

     ```rust
     // src/destination.rs:210-217
     fn enter(&mut self, path: &[u8]) {
         while let Some(top) = self.stack.last() {
             if is_under(path, top) {
                 break;
             }
             let top = self.stack.pop().expect("one is there");
             self.leave(top);
         }
     ```
- 重現方式：`src/manifest.rs` 的 `mod tests` 中加：

  ```rust
  let mut check = Check::default();
  check.root(&named(0, b"", Kind::Directory)).unwrap();
  check.directory(b"", &[named(1, b"a", Kind::Directory), named(2, b"b", Kind::Directory)], false).unwrap();
  check.directory(b"a", &[named(3, b"x", Kind::Directory)], false).unwrap();
  check.directory(b"b", &[], false).unwrap();
  assert!(check.directory(b"a/x", &[], false).is_err()); // 現況：Ok
  ```
- 修法：
  - 把 `expected` 換成一個深度優先的堆疊：`stack: Vec<(Vec<u8>, VecDeque<Vec<u8>>)>`，每層是目錄的路徑，以及它宣告了、還沒出現的子目錄名稱（依名稱順序）。正在送各段的目錄，在 `open` 中多存一個「到目前為止的子目錄」。
  - 一個目錄的第一段（路徑 `P`）到達時：
    1. 彈出堆疊，直到頂端是 `parent(P)`；彈空了就是錯誤。
    2. 從頂端的待出現清單前端一直取出，直到取到 `name(P)`。前面略過的名稱是永遠不會來的目錄（Source 列不出它，或 stat 後發現它已不是目錄），這是允許的；取不到 `name(P)` 就是錯誤（「out of depth-first order」或「came before」）。
  - 一個目錄的最後一段檢查完時，把 `(P, 它的子目錄)` 推入堆疊。
  - 根目錄：`root()` 為目錄時，第一個目錄段必須是 `""`；`""` 的最後一段之後，推入 `("", 子目錄)`。
  - Source 的順序與此相符：`src/scan.rs` 的 `list_tree` 把子目錄反向推入 `waiting`，再依名稱順序取出，是依名稱的深度優先。
  - 改完後更新 `Check` 的文件註解。
- 要加的測試（`src/manifest.rs` 的 `mod tests`）：
  - 拒絕 `""`、`a`、`b`、`a/x`。
  - 允許略過：`a`、`b` 都宣告了，只送 `""` 與 `b`（`a` 是 Source 列不出的目錄）。
  - 拒絕回頭：送了 `b` 之後再送 `a`。
  - 現有的 `a_manifest_as_a_scan_sends_it_passes` 仍通過；另加一個以 `scan::scan` 掃描一棵有多層、有略過的真實目錄樹，再把結果逐一餵給 `Check` 的測試（可放在 `src/scan.rs` 的 `mod tests`）。
- 驗收條件：
  - [ ] 上述測試通過。
  - [ ] `tests/copy.rs`、`tests/sync.rs` 全部通過（真實 Source 的 manifest 仍被接受）。
  - [ ] 用實驗程式的 `dfs` 情境時，`run` 回錯「out of depth-first order」，不再建立任何東西。
- 文件要同步的地方：`docs/design/pipeline.md`「2. Compare」中「The manifest is checked before anything of it is acted on」（`pipeline.zh-TW.md`「manifest 在被採取任何行動之前先檢查」）：加上「目錄依深度優先、依名稱的順序出現，可以略過，但不能回頭」。
- 相關項目：DST-05（收尾的順序依賴它）、DST-15（同樣是加強 `Check`）。
- **2026-10-21 補記（WP-19a）**：照修法做。`expected` 改成 `Vec<(路徑, VecDeque<子目錄名稱>)>`，`open` 多存目前這個目錄到目前為止的子目錄；目錄的最後一段檢查完才推入。與修法的小差異：根目錄不放進堆疊，而以 `root_expected` 記著，第一個目錄段必須是 `""`。錯誤訊息是「which is not one announced, or came before, or out of depth-first order」，三種情況共用一句，沒有分開。測試：`manifest` 的 `directories_come_depth_first`（拒絕 `""`、`a`、`b`、`a/x`；允許略過；拒絕回頭；拒絕第二次送根目錄），以及 `scan` 的 `what_a_scan_sends_passes_the_check`（多層、每段 2 個項目、有一個列不出的目錄，真實掃描的結果逐一餵給 `Check`）。「實驗程式的 `dfs` 情境」沒有重跑，那個實驗程式不在 repo 中；同樣的順序由 `directories_come_depth_first` 涵蓋。

---

### DST-09：全域 `State` 鎖在路徑走訪、套用目錄與 `getent` 期間一直持有

- 嚴重度：低（效能）
- 驗證狀態：已核對程式碼（未量測）
- 位置：
  - `src/destination.rs:1843-1857` `target_directory`：`open_under(&state.lock()…, …)` 的暫時 guard 一直活到整個路徑走訪結束
  - `src/destination.rs:1787-1790` `finish_directory`：同上
  - `src/destination.rs:895-937` `apply_compared`：持有鎖呼叫 `apply_directory`，其中有 `mkdir`、`symlinkat`、`mknodat`、chown、chmod、`rename`、`remove_tree`
  - `src/destination.rs:527-531` `Manifest::Names`：持有鎖呼叫 `learn`，其中會執行 `getent` 子行程
- 現象：16 個 writer 每個檔案都要從根目錄逐層 `openat`，而這些走訪彼此之間、以及與收尾 thread、main thread 的套用之間，全被同一把鎖串行化。在每次 `openat` 都是一次往返的 NFS 或 Lustre 上，這限制了寫入的平行度；一次慢的 `getent`（例如走 LDAP）會讓所有 writer 停下。
- 根本原因：

  ```rust
  // src/destination.rs:1851-1856
  let directory = open_under(
      &state.lock().expect("not poisoned"),
      root,
      &target.directory,
  )?;
  Ok((directory, target.entry.name.clone()))
  ```
  `state.lock()` 產生的 guard 是這個述句中的暫時值，要到述句結束才釋放，所以 `open_path` 整段都在鎖內。
- 重現方式：以 `--profile-file` 在 NFS 上複製一棵目錄深、小檔案多的樹，觀察 writer 的時間分布；或以 `perf lock`、`strace -f -T` 看 writer 之間的 `futex` 等待。
- 修法：
  - `target_directory` 與 `finish_directory`：在鎖內只取出 `root_directory` 的 `Arc`，放開鎖，再走路徑：

    ```rust
    let held = state.lock().expect("not poisoned").root_directory.clone();
    let directory = match held {
        Some(root_directory) => fsys::open_path(&*root_directory, &target.directory)?,
        None => fsys::open_path_from(root, &target.directory)?,
    };
    ```
  - `Manifest::Names`：在鎖外 clone 一份 `Ownership`、呼叫 `learn`，再在鎖內換上新的 `Arc`。只有 main thread 會改 ownership，所以不會遺失更新。
  - `apply_directory`：較大的改動。把它分成兩步：在鎖內決定要做什麼（`announce`、`targets.insert`、`mark_to_finish`），在鎖外做系統呼叫，最後再回到鎖內記錄失敗。
  - 最好與 DST-03 的「共用目錄 fd」一起做：大多數檔案就完全不需要走路徑。
- 要加的測試：沒有功能上的測試。以基準測試前後比較：本機與 NFS 上，深層目錄中 10 萬個小檔案的 `ferry copy` 時間與 profile。
- 驗收條件：
  - [ ] 程式碼審查：沒有任何系統呼叫或子行程在持有 `State` 鎖時執行（`apply_directory` 若暫不拆，至少記錄為已知例外）。
  - [ ] 在 NFS 上的基準測試，寫入的吞吐量不比修改前差。
- 文件要同步的地方：無；若 profile 加了新的狀態，更新 `docs/design/profiling.md` 與 `profiling.zh-TW.md`。
- 相關項目：DST-03、DST-10、DST-16。

---

### DST-10：`getent` 的參數是 Source 給的名稱，以 `-` 開頭的名稱會被當成選項

- 嚴重度：低
- 驗證狀態：已實測（只測了 `getent` 本身）。Ubuntu 的 glibc 2.39：`getent passwd tanch -s nosuchservice` 沒有輸出、結束碼 2，表示選項被重新排列而 `-s` 生效；`getent passwd -- tanch` 正常回傳。
- 位置：
  - `src/names.rs:68-84` `getent`
  - 呼叫端：`src/owners.rs:65-73` `learn`，來自 `src/destination.rs:527-531`
- 現象：Source（有 bug 或惡意）在 `Manifest::Names` 中送一個 `-s` 之類的名稱，後面接任何其他名稱，這一整批的查詢就改走另一個 NSS 服務而全部查不到，這則訊息中所有名稱都不對應，擁有者保留數字。送 `--help` 時，輸出是說明文字，解析出零筆，但因為回傳的是 `Some`，也不會退回 libc 查詢。這不會讓寫入出根目錄，只影響名稱對應的正確性。
- 根本原因：

  ```rust
  // src/names.rs:75-81
  let output = Command::new("getent")
      .arg(database.getent_name())
      .args(keys)
      .stdin(Stdio::null())
      .stderr(Stdio::null())
      .output()
      .ok()?;
  ```
  glibc 的 `getent` 以 argp 解析參數，預設會把出現在位置參數後面的選項重新排列而照樣解析。
- 重現方式：`getent passwd "$USER" -s nosuchservice; echo $?`（沒有輸出、結束碼 2）。單元測試見下。
- 修法：
  - 在呼叫 `getent` 之前，濾掉不可能是帳號名稱的名稱：空的、以 `-` 開頭、含 `:`、空白、`/` 或 NUL、或長度超過 256 bytes。被濾掉的名稱視為查不到（保留數字）。這在任何 `getent` 實作上都有效。
  - 另外可在資料庫名稱之後加 `--`。glibc 的 `getent` 支援；靜態 musl 的 Ferry 會在各種發行版上呼叫系統的 `getent`，例如 Alpine 的 musl-utils 版本，請先確認它們都接受 `--`。若有一個不接受，只做過濾即可。
- 要加的測試：`src/names.rs` 的 `mod tests`：`a_name_like_an_option_is_not_taken_as_one`。`ids(Database::Users, &["-s".into(), "nosuchservice".into(), own_name.clone()])` 仍包含自己的 uid（自己的名稱以 `names()` 取得；root 時略過，因為 ID 0 不對應）。
- 驗收條件：
  - [ ] 上述測試在 Linux 上通過。
  - [ ] 正常名稱的對應不變（`this_user_round_trips` 通過）。
- 文件要同步的地方：可在 `docs/design/architecture.md`「Names are looked up with `getent`」（`architecture.zh-TW.md`「以 `getent` 查詢名稱」）提一句：不像帳號名稱的名稱不會被查詢。
- 相關項目：DST-09（`getent` 在鎖內執行）、DST-16。
- **2026-10-21 補記（WP-19a）**：照修法的第一部分做：`names::ids` 先濾掉不可能是帳號的名稱（`may_be_an_account`），濾掉的名稱 `getent` 與 libc 都不查，視為查不到。與修法不同：沒有加 `--`。修法要求先確認各種 `getent`（例如 Alpine 的 musl-utils）都接受，這裡沒有環境可以確認；過濾已經讓所有選項形式的名稱都到不了 `getent`，所以不需要。測試：`a_name_like_an_option_is_not_taken_as_one`（`-s`、`nosuchservice` 與自己的名稱一起查，仍找到自己的 uid；拿掉過濾時會失敗），並逐一檢查幾種不可能的名稱，以及 `svc-backup`、非 ASCII 名稱仍可查詢。

---

## 較小的項目

以下各項影響較小，以簡短格式列出；每一項仍可單獨閱讀。

### DST-11：為了進入或刪除而放寬的目錄權限，在失敗時不會還原
- 嚴重度：低。驗證狀態：已核對程式碼。
- 位置：`src/fsys.rs:50-61` `open_directory_to_work_in`、`src/fsys.rs:531-550` `remove_tree`；`src/compare.rs:206-218`、`241-243`；`src/destination.rs:749-751`。
- 說明：Destination 會替自己擁有、但不能讀、寫或進入的目錄補上擁有者的 rwx，原本的權限要等收尾才設回。若刪除多餘目錄時中途失敗（例如裡面有 root 擁有的檔案），或傳輸在收尾之前失敗（DST-04、DST-05），目錄就停在原模式加上 0700。
- 修法：`remove_tree` 在放寬之前記下原本的模式，刪除失敗時設回；傳輸失敗時，對已放寬、尚未收尾的目錄設回原本的權限（`Prepared.let_in` 已經知道是哪些）。
- 測試：`src/fsys.rs` 的 `mod tests`：一個 0o000 的多餘目錄，裡面有刪不掉的項目（以唯讀子目錄模擬），`remove_tree` 失敗後，模式仍是 0o000（root 時略過）。

### DST-12：`remove_tree` 的遞迴深度沒有上限
- 嚴重度：低。驗證狀態：已核對程式碼。
- 位置：`src/fsys.rs:531-550`。
- 說明：每一層都持有一個開啟的目錄 fd 與一個 stack frame。一棵很深的多餘目錄樹（例如由之前某次執行、從惡意的 Source 建立）會用盡 fd 而刪不掉，或在開檔上限很高時讓 thread 的 stack 溢位而使行程崩潰。
- 修法：改成以明確的 `Vec` 當堆疊的迭代版本，並設深度上限（超過就回報 `NotDeleted`）；遇到 EMFILE 時以錯誤回報，不崩潰。
- 測試：`src/fsys.rs` 的 `mod tests`：建立 5,000 層的目錄樹，`remove_tree` 成功或回錯，但不崩潰。

### DST-13：根目錄是指向檔案的符號連結時，比較跟隨連結，套用卻只部分跟隨
- 嚴重度：低。驗證狀態：已核對程式碼。
- 位置：`src/compare.rs:329-338`（`stat_root` 跟隨）；`src/destination.rs:1060-1063` 以名稱呼叫 `apply_entry`；`src/destination.rs:1572-1587`。
- 說明：比較的是連結的目標，但 `Verdict::Attributes` 時，`set_owner_at` 與 `set_mtime_at` 作用在連結本身，`set_mode_at` 作用在目標（在 DST-01 修好之後會改為失敗）；`Verdict::Content` 時，重新命名會把連結本身換成一般檔案。屬性因此永遠不會一致，每次執行都重做。
- 修法：先決定語意（使用者指名的根目錄連結：一致地跟隨，或一致地取代連結）。若跟隨，就以 `open(root)`（跟隨）取得 fd，再以 `fchown`、`fchmod`、`futimens` 套用；若取代，比較時也不跟隨。
- 測試：`tests/copy.rs`：根目錄是指向檔案的連結，模式不同；第二次執行時什麼都不列出。

### DST-14：不是擁有者時，群組不同仍被當成差異
- 嚴重度：低。驗證狀態：已核對程式碼。
- 位置：`src/compare.rs:426`；`src/owners.rs:59-62` `can_set_group`。
- 說明：非 root、屬於目標群組、但不是檔案擁有者時，`can_set_group` 回 `true`，所以這個項目被判為 `Verdict::Attributes`；而 chown 對不屬於自己的檔案一定回 `EPERM`。結果這個項目每次執行都失敗；若是目錄，還會觸發 DST-05。
- 修法：`can_set_group(wanted, current_uid)` 改為 `root || (current_uid == euid && member_of.contains(&wanted))`，並更新所有呼叫端（`compare.rs:426`、`destination.rs:1230`）。
- 測試：`src/owners.rs` 加 `mod tests`：非 root 的 `Ownership`，對不同 `current_uid` 的結果。

> 2026-10-20 補記：WP-07 照修法做：`can_set_group(wanted, current_uid)`，非 root 時要是擁有者、而且屬於該群組。`Ownership` 另外記下 `euid`，加了 `can_set_mode`。測試：`owners.rs` 的 `who_is_not_root_sets_only_what_it_owns`。

### DST-15：manifest 不檢查 mtime 的奈秒是否小於 10^9
- 嚴重度：低。驗證狀態：已實測（DST-05 的重現就是用這個觸發的）。
- 位置：`src/manifest.rs:141-201`；`src/fsys.rs:411-422`。
- 說明：格式錯誤的時間戳記讓 `utimensat` 回 `EINVAL`：檔案會失敗；目錄則觸發 DST-05，讓之後的目錄都不再收尾。
- 修法：在 `Check::root` 與 `Check::directory` 中，對每個項目 `ensure!(entry.mtime.nanoseconds < 1_000_000_000)`。DST-05 的測試屆時要改用直接測 `finish_as_done` 的方式。
- 測試：`src/manifest.rs` 的 `mod tests`：`anything_else_does_not` 中加一個奈秒為 10^9 的項目，必須被拒絕。
- **2026-10-21 補記（WP-19a）**：照修法做，`Check::root` 與 `Check::directory` 共用 `has_a_time`。測試加在 `anything_else_does_not`：根目錄與一般項目的奈秒為 10^9 都被拒絕，999,999,999 被接受。DST-05 的測試（`a_directory_that_cannot_be_finished_fails_alone`）早已直接測 `finish_as_done`，以一個已消失的目錄觸發，不必改。

### DST-16：每則 `Manifest::Names` 都複製整份 ownership 對照表
- 嚴重度：低。驗證狀態：已核對程式碼。
- 位置：`src/destination.rs:530` `Arc::make_mut(&mut state.ownership).learn(...)`。
- 說明：只要還有任何一段持有舊的 `Arc`（幾乎總是如此），`make_mut` 就複製整個 `Ownership`（`users`、`groups`、`member_of` 三個 `HashMap`）。在帳號數以萬計的叢集上，總複製量隨 ID 數量的平方成長，而且是在鎖內（DST-09）。
- 修法：讓 `users`、`groups` 各自包在 `Arc` 中，只複製被修改的那一份；或改用持久化的資料結構；或在鎖外建好新的一份再換上（與 DST-09 一起做）。
- 測試：基準測試：送 10,000 則各帶一個新名稱的 `Names`，時間大致呈線性。

### DST-17：回報失敗時，每個失敗都線性搜尋整段的項目
- 嚴重度：低。驗證狀態：已核對程式碼。
- 位置：`src/destination.rs:939-952`（`entries.iter().find(|entry| entry.id == *id)`）。
- 說明：一整段都失敗時（例如它的目錄打不開），成本是 O(n²)：正常的 Source 一段最多 10,000 項，約 5×10^7 次比較；一個塞滿 16 MiB 的惡意段約 30 萬項，約 4.5×10^10 次，可卡住數十秒以上。
- 修法：`Check` 已保證一段中的 ID 連續，所以可以直接以 `(id - entries[0].id) as usize` 取索引；或先建 `HashMap<FileId, &Entry>`。
- 測試：單元測試：一段 10,000 項全部失敗，`apply_compared` 在合理時間內完成（或直接測索引的輔助函式）。

### DST-18：把兩個名稱視為同一個的檔案系統上，Source 的名稱相撞時 `sync` 會刪掉別名所指的目錄樹
- 嚴重度：低。驗證狀態：推測（依程式碼推得，未在 macOS 上實測）。
- 位置：`src/destination.rs:1877-1890` `clear_the_way`、`src/destination.rs:1240-1249` `make_directory`、`src/destination.rs:1572-1587` `rename_into_place`。
- 說明：
  - 大小寫相撞：Linux 的 Source 同一目錄有檔案 `A` 與目錄 `a`，目的地在不分大小寫的檔案系統上（macOS 的 APFS 預設如此）。`a` 先被建立並寫入內容；之後 writer 把 `A` 重新命名就位時，`clear_the_way` stat `A` 得到的是目錄 `a`。`sync` 時 `remove_tree` 刪掉它和剛寫進去的一切；`copy` 時 `A` 失敗，或刪掉空的 `a`。結果依時間而定，影響停留在根目錄之下。
  - 正規化：HFS+ 一律以 NFD 儲存名稱；從 Linux 以 NFC 名稱 `sync` 過去時，每次執行都會把這些檔案當成多餘的刪掉再重送。
- 修法：重新命名或建立之前，若目標名稱已存在、而它不是這個目錄中 Source 列出的同名項目，就以 inode 比對（如 `without_aliases_of_unreadable` 的做法）判斷是否為同一個別名，相撞時讓該項目失敗。另一種做法是在 macOS 上以 `pathconf(_PC_CASE_SENSITIVE)` 偵測不分大小寫的檔案系統，比對名稱時改為不分大小寫。
- 測試：macOS 上的 `tests/sync.rs`（`#[cfg(target_os = "macos")]`）：Source 有 `A`（檔案）與 `a/f`；`sync` 之後 `a/f` 仍在，`A` 回報失敗。
- **2026-10-21 補記（WP-19a）**：先在本機 ext4 casefold 上實測，比審查推測的更嚴重：
  - `sync` 時，Source 有檔案 `A` 與 `a/f`，目的地原本沒有 `a`：`a` 與 `a/f` 被默默刪掉，只剩 `A`，結束狀態 0，沒有任何訊息。
  - `copy` 時，空的 `a` 被刪掉，`a/f` 以「No such file or directory」失敗。目的地已有 `a` 時，`A` 也以同樣看不懂的訊息失敗。
  - 審查沒提到的：Source 有兩個目錄 `A/x` 與 `a/y` 時，兩者被合併成一個目錄；之後每次 `sync` 都刪掉 `a/x`、再重送 `A/x`。Source 有兩個檔案 `A` 與 `a` 時，後寫的默默覆蓋先寫的，兩者都算複製成功。
  - 修法採「以 inode 比對」的方向，但不比對 Source 列出的項目：只在比較時沒有列出這個名稱（`present`、`Target.exists`），而放置時發現名稱已被占用時，才列出目錄，看占用者在目錄中實際存的名稱（`fsys::alias`，以 `d_ino` 找出）。實際存的名稱與要放的不同，就是別名：這一項失敗，說明被當成了哪個名稱，什麼都不刪、不覆寫。檢查放在 `clear_the_way`（檔案、符號連結、特殊檔）與 `make_directory`（目錄）。目錄失敗時，`DirCache::refuse` 讓它與它底下的路徑都不能再開啟，否則它的內容會經由別名寫進另一個目錄。名稱照樣存在時（例如 hard link），不算別名。平常的傳輸不多做任何系統呼叫。
  - 審查擔心的「目的地有舊的 `a`，Source 只有 `A`」：`sync` 先刪除多餘的項目，才套用目錄的各段（pipeline.md「A directory is applied whole, extras first」），所以 `a` 先被刪掉，`A` 照常放上，實測正確。
  - 與審查的建議不同之處：`copy` 時，目的地有舊的檔案 `a`、Source 只有 `A`，以前是以 `a` 這個名稱覆寫內容，現在 `A` 失敗並說明原因（`copy` 不刪除，所以每次都一樣）。HFS+ 以 NFD 存名稱，以 NFC 名稱 `copy` 過去、目的地已有該檔時，也會這樣失敗；`sync` 時仍是每次刪掉再重送，第二部分（正規化）沒有修。另一種做法（以 `pathconf(_PC_CASE_SENSITIVE)` 偵測、不分大小寫比較）沒有採用：各檔案系統的規則不同。
  - 測試：`tests/sync.rs` 的 `names_a_destination_takes_for_one_fail_and_delete_nothing`（檔案對目錄、目錄對目錄，`copy` 與 `sync` 各跑兩次；只在暫存目錄不分大小寫、而建置目錄分大小寫時執行，即 CI 的 ext4-casefold；修正前會失敗）；`fsys` 的 `an_alias_is_the_name_kept_for_another`；`dirs` 的 `a_directory_refused_is_not_gone_into`。macOS 上 Source 無法同時有 `A` 與 `a`，所以整合測試在 macOS 上略過。

### DST-19：`sync` 刪除多餘的目錄時會進入其中的掛載點
- 嚴重度：低（資訊）。驗證狀態：已核對程式碼。
- 位置：`src/fsys.rs:531-550` `remove_tree`。
- 說明：`remove_tree` 不比較 `st_dev`，多餘的目錄若是掛載點（或其下有掛載點），會刪掉另一個檔案系統中的內容。這與 rsync 不加 `-x` 時相同，但文件沒有提到。
- 修法：至少在文件中說明；之後可加 `--one-file-system`，在 `remove_tree` 中遇到 `st_dev` 與根目錄不同的目錄時不進入，並回報 `NotDeleted`。
- 測試：若實作選項，以 Linux 的 tmpfs 掛載（需要 root）手動確認。
- 文件要同步的地方：`docs/design/overview.md`「Behaviour of `ferry sync`」（`overview.zh-TW.md`「`ferry sync` 的行為」）。
