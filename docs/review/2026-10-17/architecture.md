# 架構改動提案：交接文件

> 2026-10-22：已完成的項目（4 項）移到 [done/architecture.md](done/architecture.md)，這裡只留還沒完成或決定不做的。

- 對象：HEAD `3c17a2f`（version 0.14.4）。行號都以這個 commit 為準。
- 這份文件提的是架構改動的方案，不是缺陷清單。缺陷在其他五份檔案裡；這裡的每個方案，會一併吸收其中一部分項目，寫在各方案的「吸收的項目」。
- 結論先說：**不需要大翻修**。不換語言、不換成 async、不換 SSH 模型，也不拿掉 client relay。有四個結構性改動值得做（ARC-01 到 ARC-04），另有幾個小調整（ARC-05）。
- 每個方案都會改變某些已寫進設計文件的決定，或增加依賴。動手前，先請使用者確認 [README](README.md) 第 7 節列出的對應決定（D14 到 D18）。
- 這些方案最後收斂成的長期設計，寫在 [target-architecture.md](target-architecture.md)。它的第 15.2 節說明 ARC-01 到 ARC-04 在那條路上的位置。

## 總表

| ID | 方案 | 依據 | 建議時機 |
|---|---|---|---|
| ARC-04 | 大檔案的 record 改成可以分段：平行讀寫、可續傳，也是 sparse 與 delta 的基礎 | 已量測：單一大檔受限於一條 writer thread | 先量測工作負載，再決定 |

---

## 量測

### 環境與方法

- 機器：Linux 5.15，x86_64，96 核心。目錄樹放在 `/home`（md RAID 上的 ext4）。release build。兩側都在本機。
- 每次執行前，先把來源讀進 page cache，所以量到的是 pipeline 本身，不含讀取磁碟的時間。
- 各 stage 的時間取自 `--profile-file` 的 JSON，用附錄 A 的 `prof.py` 彙整。原始的 JSON 在 [`materials/arch/profiles/`](materials/arch/profiles/)。「busy」是不在等待的時間，多條 thread 的時間相加。
- 換成 NVMe、xfs、btrfs、NFS 或 macOS（APFS），數字會不同。每個方案在動手前後都要重新量測（見各方案的「量測計畫」）。

### 結果

| 情境 | `cp -a` | `rsync -a` | ferry `--no-fsync` | ferry（預設，含 fsync） |
|---|---|---|---|---|
| 20 萬個 4 KiB 小檔，200 個目錄，每個目錄 1,000 個 | 4.7 s（user 0.3 s，sys 4.5 s） | 10.9 s（user 1.6 s，sys 11.2 s） | 10.1 s（user 11.6 s，**sys 84.6 s**） | 38.7 s（user 19.0 s，sys 70.3 s） |
| 1 個 4 GiB 的隨機資料檔 | 2.5 s（sys 2.5 s） | — | 3.6 s（user 13.0 s，sys 17.1 s） | — |

**20 萬個小檔，`--no-fsync`，Destination 的 profile（10.05 s）：**

| role | thread 數 | busy | 說明 |
|---|---|---|---|
| writer | 16 | 69.7 s，其中 write files 69.5 s | 每個 4 KiB 的檔案約 350 µs |
| rename | 6,256 | 29.2 s | `rename_batch` 每批 256 個檔案就開 8 條 thread：782 批 × 8 |
| parser | 1 | 7.17 s（72%），其中 rename files 4.87 s | parser 有將近一半的時間在等批次改名 |
| finisher | 1 | 10.03 s（100%） | **失真**：它其實在等 channel，見 ARC-05b |
| receive | 1 | 0.74 s | |
| decompress | 8 | 0.44 s | |

同一次執行，Source 端的 16 個 reader 合計 11.6 s（每個檔案約 58 µs，見 SRC-06），compress 合計 1.9 s，其他 role 都很閒。

**20 萬個小檔，含 fsync（38.7 s）：** writer 有 16 條，合計 busy 491 s，等於每條 80%，是瓶頸，主要時間都在 fsync。rename 合計 63.8 s。parser 的 rename files 為 9.5 s。

**4 GiB 單一檔案，`--no-fsync`（3.57 s）：**

| 側 | role | busy | 說明 |
|---|---|---|---|
| Destination | writer | 3.45 s | 只有一條 writer 在寫這個檔案，忙了 97% 的時間 |
| Source | file sender | 2.24 s，其中 read files 2.20 s | 大於 1 MiB 的檔案由這一條 thread 讀取並雜湊（SRC-11） |
| Source | compress | 4.38 s，8 條合計 | 本機複製，而且是壓不下去的隨機資料 |
| client | relay from the Source | 0.07 s | relay 不是瓶頸 |

### 目錄鎖的微基準

用附錄 B 的 `dirbench.py`：每種情況寫 4 萬個 4 KiB 的檔案，量 wall time 與 process 的系統時間。這是 Python 寫的，有 GIL，吞吐量大概卡在每秒 5 萬個左右，所以要看的是**系統時間**的差距。

| 寫法 | 1 thread，同一目錄 | 16 threads，同一目錄 | 16 threads，分散到 16 個目錄 |
|---|---|---|---|
| 暫存名稱，再 `rename`（Ferry 現在的做法） | 1.27 s，sys 1.21 s，31.5k 檔/秒 | 1.37 s，**sys 18.73 s**，29.2k 檔/秒 | 0.77 s，sys 2.58 s，52.1k 檔/秒 |
| `O_TMPFILE`，再 `linkat` | 0.82 s，sys 0.77 s，48.9k 檔/秒 | 0.77 s，sys 2.92 s，52.2k 檔/秒 | 0.78 s，sys 2.27 s，51.5k 檔/秒 |

解讀：

- 在同一個目錄中，用暫存名稱再改名，開 16 條 thread 並不比 1 條快，系統時間卻是 15 倍。每個檔案要做兩次目錄操作：建立暫存名稱、改名。在 Linux 上，兩者都要獨占該目錄 inode 的 `i_rwsem`，16 條 thread 就在 kernel 裡搶這把鎖。
- 改用 `O_TMPFILE` 之後，建立檔案時不會新增目錄項目，每個檔案只剩 `linkat` 一次目錄操作，競爭幾乎消失。
- Ferry 的 manifest 依目錄順序前進，需要的檔案也是一個目錄接著一個目錄送來，所以 16 個 writer 與 8 個 renamer 幾乎總是擠在同一個目錄裡，正好落在上表最差的那一格。

---

## ARC-04：大檔案的 record 改成可以分段（看工作負載決定）

- 嚴重度：效能與功能，中。是否值得做，取決於「快速網路或本機 NVMe 上的大檔」是不是主要的工作負載。
- 依據：4 GiB 的檔案，Destination 唯一的 writer 忙了 97% 的時間，Source 的 file sender 讀了 2.2 s，每個檔案大約卡在 1.2 GB/s。`cp` 是 1.7 GB/s。
- 吸收的項目：SRC-11；也解決 DOC-07 提到的「中斷的大檔要從頭重送」；並為 sparse 與 delta 打底（README 第 10 節）。
- 需要使用者決定：**D16**（要不要做；要不要支援續傳，因為續傳需要有名稱的暫存檔，與 ARC-01 的沒有名稱的暫存檔衝突）。

### 現在

- 一個檔案是一個 record：header、全部內容、trailer（含 BLAKE3）。
- 大於 1 MiB 的檔案，由 Source 的 file sender 這一條 thread 依序讀取、雜湊，寫進 record 串流。
- Destination 由一個 writer 接收它的全部內容。
- 所以一個大檔的速度，上限是一條 thread 的「讀加雜湊」，以及一條 thread 的「寫加雜湊」。

### 提案

1. 大於門檻的檔案（例如 64 MiB），切成固定大小的段，例如 16 MiB 或 64 MiB，量測後決定。新增一種 record header，例如 `Header::Segment { id, index, offset, length, size, mtime }`，每段帶自己的 BLAKE3 trailer。
2. **Source**：
   - 開檔一次，以 `Arc<File>` 共用，只 fstat 一次，大小與 mtime 以這次為準。
   - 每段是 reader pool 的一個工作，用 `pread` 讀取，所以不再由 file sender 讀大檔（SRC-11）。
   - 任何一段讀不完整，就把整個檔案標為 unread，與現在的語意相同。
3. **Destination**：
   - 第一段到達時，建立暫存檔（ARC-01 的沒有名稱的檔案，或暫存名稱），並 `ftruncate` 到完整大小。
   - 每段由任一 writer 以 `pwrite` 寫到自己的 offset，並驗證自己的雜湊。
   - 每個檔案有一個計數，記錄已驗證的段數。全部到齊，就設定屬性、fsync，交給改名 stage。任何一段失敗，整個檔案失敗，暫存檔移除。
4. **完整性**：每一段各自驗證；再加上段的配置與 Source 宣告的大小一致，等同整個檔案驗證過。不需要合併 BLAKE3 的樹。
5. **串流順序**：不同檔案的段可以交錯，parser 本來就依 ID 分派。同一個檔案的不同段由不同的 writer 處理，所以可以平行寫入。
6. **記憶體**：上限由在途的段數決定，每個 writer channel 的容量也要限制。

### 延伸（各自另做，不在第一步）

- **續傳**：要能續傳，暫存檔必須有名稱，並在旁邊記下已驗證的段，以及 Source 的大小與 mtime，以確認是同一個版本。下次執行時只要求缺少的段。這與 ARC-01 的沒有名稱的檔案互斥，所以只對大檔使用有名稱的暫存檔（D16）。
- **sparse**：Source 以 `SEEK_DATA` 與 `SEEK_HOLE` 找出資料區段，只為資料區段產生段，洞由 Destination 的 `ftruncate` 自然形成。
- **粗粒度的 delta**：Destination 對已存在的同名檔案，先送出各段的雜湊；Source 對雜湊相同的段，只回「沿用」。這對就地修改的大檔有效，例如 VM 映像與資料庫檔案；對插入資料則無效（那需要 rsync 的滾動 checksum）。這是邁向 delta transfer 比較容易的第一步。

### 量測計畫（決定做不做之前）

- 用 `ferry test /tmp /tmp --scale 20G --kind large`（每個檔案 64 到 256 MiB）與 `--kind mixed`，在本機與一條快速網路上量測，記錄 Source 的 file sender 與 Destination 的 writer 的 busy 時間。
- 若 writer 或 file sender 是瓶頸，而且網路與磁碟都還有餘裕，就值得做。

### 驗收條件（若決定做）

- [ ] 單一 4 GiB 檔的本機複製，吞吐量隨 writer 數量上升，直到磁碟的上限。
- [ ] 段讀不完整、段的雜湊不符、中途中斷，都不會讓錯誤的內容就位。這三種情況都要有測試。
- [ ] 小檔的量測沒有變慢。

### 文件要同步的地方

- `docs/design/overview.md` 的「Terms」（record 的定義）與「Every file is complete, correct, and durable」第 2 點，與 `overview.zh-TW.md` 的對應段落。
- `docs/design/pipeline.md` 的「3. Read and hash」與「6. Write」，與 `pipeline.zh-TW.md` 的對應段落。

### 決定

2026-10-22 補記（D16）：使用者選擇先不做，也不量測，與量測計畫不同。理由：使用者目前沒有比約 1.2 GB/s 更快的網路，跨機器的傳輸都卡在網路，一個大檔由兩條 thread 處理的上限碰不到；剩下的好處只有本機複製大檔（約 1.2 GB/s，`cp` 約 1.7 GB/s），不值得動協定、Source 與 Destination。續傳（延伸的第一項）另作為之後的功能，等「大檔中斷後從頭重送」真的造成困擾時再單獨設計；overview 本來就把它列為非目標並註明「需要時再考慮」，所以文件不必改。SRC-11 隨之不做。工作負載改變（有了更快的鏈路）時再回到這一項。

---

## 不建議改的地方

| 不改的部分 | 理由 |
|---|---|
| Rust 加同步 thread，不換成 async | 工作都是阻塞的檔案系統呼叫，async 帶不來好處。io_uring 以後可以只用在寫入 stage，先量測再說 |
| 使用系統的 `ssh` | `~/.ssh/config`、ProxyJump、ssh-agent、OTP、硬體金鑰都直接能用。換成 SSH library，就得逐一重做這些 |
| client relay 兩個 server 之間的資料 | 量測顯示它不是瓶頸：4 GiB 那次，relay thread 只忙了 0.07 s。remote-to-remote 時資料經過 client 兩次，是設計文件寫明的取捨：兩台主機不必互通，也不必在任一台放憑證 |
| client 與 server 必須是同一個 build | 協定因此可以自由修改，不必相容舊版。ARC-04 這種格式改動也因此便宜 |
| 一個 lister 加 8 個 stat | 這是在 NFS 上量測後的選擇。量測的檔案樹中，Source 端的 scan 與 stat 都不是瓶頸 |
| 安裝模型：上傳同一個 build、並存多個版本 | 合理。要補強的是驗證與順序（TRN-01、TRN-02、TRN-03），不是模型本身 |
| 拆成多 crate 的 workspace | 目前不值得。等 fuzz 的建置，或遠端 dev build 的編譯時間變成問題時再考慮 |

---

## 附錄 A：`prof.py`（彙整 `--profile-file` 的 JSON）

```python
import json, sys
d = json.load(open(sys.argv[1]))
print(f"elapsed {d['elapsed_nanos'] / 1e9:.2f}s")
for side in d['sides']:
    print('==', side['side'])
    for r in side['roles']:
        total = sum(s['nanos'] for s in r['states'])
        busy = [(s['state'], s['nanos']) for s in r['states'] if not s['wait']]
        waits = sorted([(s['state'], s['nanos']) for s in r['states'] if s['wait']],
                       key=lambda x: -x[1])[:2]
        b = sum(ns for _, ns in busy)
        print(f"  {r['role']:<28} x{r['threads']:<5} busy {b / 1e9:7.2f}s ({b / max(total, 1) * 100:4.0f}%)  "
              + ", ".join(f"{s}:{ns / 1e9:.2f}s" for s, ns in busy if ns > 5e7)
              + " | waits " + ", ".join(f"{s}:{ns / 1e9:.2f}s" for s, ns in waits))
```

用法：`python3 -I prof.py profile.json`。

## 附錄 B：`dirbench.py`（目錄鎖的微基準，Linux）

```python
import os, sys, time, threading, resource, shutil
root = sys.argv[1]; N = 40000; data = os.urandom(4096)

def run(threads, spread, tmpfile):
    shutil.rmtree(root, ignore_errors=True); os.makedirs(root)
    dirs = [os.path.join(root, 'd%02d' % i) for i in range(threads if spread else 1)]
    for d in dirs: os.makedirs(d)
    fds = [os.open(d, os.O_DIRECTORY) for d in dirs]
    per = N // threads
    def work(t):
        dfd = fds[t % len(fds)]
        for i in range(per):
            name = f'f{t}_{i}'
            if tmpfile:
                fd = os.open('.', os.O_TMPFILE | os.O_WRONLY, 0o600, dir_fd=dfd)
                os.write(fd, data)
                os.link(f'/proc/self/fd/{fd}', name, dst_dir_fd=dfd, follow_symlinks=True)
                os.close(fd)
            else:
                tmp = '.' + name + '.ferry-x'
                fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600, dir_fd=dfd)
                os.write(fd, data); os.close(fd)
                os.rename(tmp, name, src_dir_fd=dfd, dst_dir_fd=dfd)
    r0 = resource.getrusage(resource.RUSAGE_SELF); t0 = time.time()
    ts = [threading.Thread(target=work, args=(t,)) for t in range(threads)]
    [t.start() for t in ts]; [t.join() for t in ts]
    t1 = time.time(); r1 = resource.getrusage(resource.RUSAGE_SELF)
    print(f"threads={threads:2} {'spread' if spread else 'one dir'} "
          f"{'O_TMPFILE+linkat' if tmpfile else 'temp+rename'}: {t1 - t0:5.2f}s wall, "
          f"{r1.ru_stime - r0.ru_stime:6.2f}s sys, {N / (t1 - t0):7.0f} files/s")
    for fd in fds: os.close(fd)

for tmpfile in (False, True):
    for threads, spread in ((1, False), (16, False), (16, True)):
        run(threads, spread, tmpfile)
shutil.rmtree(root)
```

用法：`python3 -I dirbench.py <要測的檔案系統上的一個新目錄>`。

## 附錄 C：產生 20 萬個 4 KiB 檔案

```sh
python3 -I -c "
import os
for d in range(200):
    p = os.path.join('small', 'd%03d' % d); os.makedirs(p, exist_ok=True)
    for i in range(1000):
        with open(os.path.join(p, 'f%04d' % i), 'wb') as f: f.write(os.urandom(4096))
"
find small -type f -exec cat {} + > /dev/null   # 先讀進 page cache
```

這次量測的 profile JSON（`big.json`、`small.json`、`small-fsync.json`）放在 [`materials/arch/profiles/`](materials/arch/profiles/)，兩個腳本放在 [`materials/arch/`](materials/arch/)。
