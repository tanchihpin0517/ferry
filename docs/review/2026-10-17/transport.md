# Transport 與遠端安裝審查：交接文件

> 2026-10-22：已完成的項目（22 項）移到 [done/transport.md](done/transport.md)，這裡只留還沒完成或決定不做的。

範圍：`src/transport/`（`mod.rs`、`ssh.rs`、`start.rs`、`join.rs`、`striped.rs`、`heartbeat.rs`）、`src/remote/`（`probe.rs`、`install.rs`、`release.rs`、`sudo.rs`、`shell.rs`、`location.rs`、`mod.rs`）、`src/update.rs`、`src/home.rs`、`src/build_info/`、`build.rs`、`scripts/install.sh`、`.github/workflows/release.yml`。

所有位置都以 HEAD `3c17a2f`（工作樹乾淨）重新核對過。每一項都可單獨閱讀，不需要先看過這次審查。

用詞依 `docs/design/*.zh-TW.md`：heartbeat channel、SSH connection、control connection、path、片段（piece）、結尾標記（end marker）、開頭標記（start mark）、探測（probe）、交握（handshake）。

## 目錄

| ID | 嚴重度 | 標題 |
|---|---|---|
| TRN-03 | 中 | release 只以同一來源的 `SHA256SUMS` 檢查，沒有簽章（端到端簽章方案） |
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

- **2026-10-22 補記**：再次出現，在 CI run 39758050556（commit `5332b2e`）的 macOS 26 arm64，這次不是卡住，而是 10.18 秒後以「a path did not end」失敗：200 個訊息都送出也都收回，`sending.close()` 也完成，之後 `Paths::wait` 等某條 path 在對端關閉，超過了 WP-ARC 12a（TRN-18）加的 10 秒上限 `LET_GO`。所以 10-18 那兩次 60 秒的卡住，很可能是同一件事：資料都到了，但伺服器那端有一條 path 沒有放開；當時 `Paths::wait` 沒有上限，就一直等。這種失敗不是逾時，不經過 `within`，所以 10-21 加的 `sample`／`lsof` 沒有執行，這次仍沒有證據。這次加的：`Paths::wait` 的錯誤說出是哪一條（「socket 1 of 2」或「program 1 of 2」）；`echoes` 在 `paths.wait()` 失敗時先呼叫 `say_where_threads_are`，再失敗。以「回聲 server 對第 2 條 path 多留一個 fd」驗證過：10 秒後印出「a path did not end: socket 1 of 2」與每條 thread 的位置。下次在 macOS 上出現時，`lsof` 會列出仍開著的那個 socket，`sample` 會顯示回聲 server 的 thread 停在哪裡，可以依第 3 步分辨假設。

---

## 較小的項目

以下各項都是低嚴重度，寫得較短；每項仍可單獨處理。
