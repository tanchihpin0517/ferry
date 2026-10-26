"""A FUSE filesystem, spoken raw on /dev/fuse, whose file `stuck` never
answers a read; run under `unshare -rm`. After mounting, it makes the kernel
wait on it without letting signals interrupt (a read interrupted, the
interrupt answered ENOSYS), so that a read of `stuck` waits in D state.
Then it runs the command after `--`, with MNT set to the mount point, and
ends once that does.

usage: unshare -rm python3 -I stuckfs.py MOUNTPOINT -- COMMAND...

For tests/copy.rs.
"""

import ctypes
import errno
import os
import signal
import struct
import subprocess
import sys
import threading
import time

mnt = sys.argv[1]
command = sys.argv[sys.argv.index("--") + 1:]

fd = os.open("/dev/fuse", os.O_RDWR)
libc = ctypes.CDLL(None, use_errno=True)
options = f"fd={fd},rootmode=40000,user_id=0,group_id=0".encode()
if libc.mount(b"stuckfs", mnt.encode(), b"fuse", 2 | 4, options) != 0:
    code = ctypes.get_errno()
    raise OSError(code, os.strerror(code))

NOW = int(time.time()) - 3600
FILES = {2: (b"a", b"hello\n"), 3: (b"stuck", 1 << 20), 4: (b"probe", 4096)}


def attr(ino):
    if ino == 1:
        mode, size, nlink = 0o40755, 0, 2
    else:
        content = FILES[ino][1]
        size = len(content) if isinstance(content, bytes) else content
        mode, nlink = 0o100644, 1
    return struct.pack(
        "<QQQQQQIIIIIIIIII",
        ino, size, (size + 511) // 512, NOW, NOW, NOW,
        0, 0, 0, mode, nlink, 0, 0, 0, 4096, 0,
    )


def reply(unique, error=0, data=b""):
    os.write(fd, struct.pack("<IiQ", 16 + len(data), error, unique) + data)


def dirent(ino, off, name, kind):
    entry = struct.pack("<QQII", ino, off, len(name), kind) + name
    return entry + b"\0" * ((8 - len(entry) % 8) % 8)


probe_read = threading.Event()
interrupted = threading.Event()
held = {}


def serve():
    while True:
        try:
            buffer = os.read(fd, (1 << 17) + 4096)
        except OSError as error:
            if error.errno == errno.ENODEV:
                return
            if error.errno in (errno.EINTR, errno.ENOENT, errno.EAGAIN):
                continue
            raise
        length, opcode, unique, node = struct.unpack_from("<IIQQ", buffer)
        body = buffer[40:length]
        if opcode == 26:  # INIT
            major, minor, readahead, flags = struct.unpack_from("<IIII", body)
            reply(unique, 0, struct.pack(
                "<IIIIHHIIHHI28x", 7, 31, readahead, 0, 16, 12, 65536, 1, 32, 0, 0))
        elif opcode == 1:  # LOOKUP
            name = body.split(b"\0", 1)[0]
            found = [ino for ino, (n, _) in FILES.items() if n == name and node == 1]
            if found:
                reply(unique, 0, struct.pack("<QQQQII", found[0], 0, 1, 1, 0, 0) + attr(found[0]))
            else:
                reply(unique, -errno.ENOENT)
        elif opcode == 3:  # GETATTR
            reply(unique, 0, struct.pack("<QII", 1, 0, 0) + attr(node))
        elif opcode in (14, 27):  # OPEN, OPENDIR
            reply(unique, 0, struct.pack("<QII", 0, 0, 0))
        elif opcode == 28:  # READDIR
            _, offset, size = struct.unpack_from("<QQI", body)
            entries = [(1, b".", 4), (1, b"..", 4)] + [
                (ino, name, 8) for ino, (name, _) in FILES.items() if name != b"probe"
            ]
            out = b""
            for index in range(offset, len(entries)):
                ino, name, kind = entries[index]
                entry = dirent(ino, index + 1, name, kind)
                if len(out) + len(entry) > size:
                    break
                out += entry
            reply(unique, 0, out)
        elif opcode in (18, 25, 29):  # RELEASE, FLUSH, RELEASEDIR
            reply(unique)
        elif opcode == 15:  # READ
            _, offset, size = struct.unpack_from("<QQI", body)
            if node == 2:
                reply(unique, 0, FILES[2][1][offset:offset + size])
            else:
                held[unique] = node
                if node == 4:
                    probe_read.set()
        elif opcode == 36:  # INTERRUPT
            (target,) = struct.unpack_from("<Q", body)
            # No interrupts: from now on, the kernel waits for answers in a
            # wait only a fatal signal ends.
            reply(unique, -errno.ENOSYS)
            if held.pop(target, None) == 4:
                reply(target, -errno.EIO)
                interrupted.set()
        elif opcode == 17:  # STATFS
            reply(unique, 0, struct.pack("<QQQQQIIII24x", 1000, 500, 500, 100, 50, 4096, 255, 4096, 0))
        elif opcode in (2, 42):  # FORGET, BATCH_FORGET
            pass
        elif opcode == 38:  # DESTROY
            reply(unique)
            return
        else:
            reply(unique, -errno.ENOSYS)


threading.Thread(target=serve, daemon=True).start()

probe = subprocess.Popen(["cat", f"{mnt}/probe"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
assert probe_read.wait(10), "the probe never read"
probe.send_signal(signal.SIGTERM)
assert interrupted.wait(10), "no interrupt came"
probe.wait()
print("stuckfs: mounted; reads of stuck now wait in D state", file=sys.stderr, flush=True)

code = subprocess.call(command, env={**os.environ, "MNT": mnt})
sys.exit(code)
