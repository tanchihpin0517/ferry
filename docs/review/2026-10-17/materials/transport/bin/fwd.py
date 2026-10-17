import os, socket, sys, threading
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
def up():
    while True:
        d = os.read(0, 65536)
        if not d:
            s.shutdown(socket.SHUT_WR)
            return
        s.sendall(d)
threading.Thread(target=up, daemon=True).start()
while True:
    d = s.recv(65536)
    if not d:
        break
    os.write(1, d)
