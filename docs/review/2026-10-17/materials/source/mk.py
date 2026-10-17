import os, sys
root = sys.argv[1]; dirs = int(sys.argv[2]); per = int(sys.argv[3])
os.makedirs(root, exist_ok=True)
for d in range(dirs):
    p = os.path.join(root, "d%05d" % d)
    os.mkdir(p)
    for f in range(per):
        fd = os.open(os.path.join(p, "f%04d" % f), os.O_CREAT | os.O_WRONLY, 0o644)
        os.close(fd)
