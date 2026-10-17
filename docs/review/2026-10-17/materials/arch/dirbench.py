import os, sys, time, threading, resource, shutil
root=sys.argv[1]; N=40000; data=os.urandom(4096)
def run(threads, spread, tmpfile):
    shutil.rmtree(root, ignore_errors=True); os.makedirs(root)
    dirs=[os.path.join(root,'d%02d'%i) for i in range(threads if spread else 1)]
    for d in dirs: os.makedirs(d)
    fds=[os.open(d, os.O_DIRECTORY) for d in dirs]
    per=N//threads
    def work(t):
        dfd=fds[t % len(fds)]
        for i in range(per):
            name=f'f{t}_{i}'
            if tmpfile:
                fd=os.open('.', os.O_TMPFILE|os.O_WRONLY, 0o600, dir_fd=dfd)
                os.write(fd,data)
                os.link(f'/proc/self/fd/{fd}', name, dst_dir_fd=dfd, follow_symlinks=True)
                os.close(fd)
            else:
                tmp='.'+name+'.ferry-x'
                fd=os.open(tmp, os.O_WRONLY|os.O_CREAT|os.O_EXCL, 0o600, dir_fd=dfd)
                os.write(fd,data); os.close(fd)
                os.rename(tmp, name, src_dir_fd=dfd, dst_dir_fd=dfd)
    r0=resource.getrusage(resource.RUSAGE_SELF); t0=time.time()
    ts=[threading.Thread(target=work,args=(t,)) for t in range(threads)]
    [t.start() for t in ts]; [t.join() for t in ts]
    t1=time.time(); r1=resource.getrusage(resource.RUSAGE_SELF)
    print(f"threads={threads:2} {'spread over dirs' if spread else 'one dir        '} {'O_TMPFILE+linkat' if tmpfile else 'temp+rename     '}: {t1-t0:5.2f}s wall, {r1.ru_stime-r0.ru_stime:6.2f}s sys, {N/(t1-t0):7.0f} files/s")
    for fd in fds: os.close(fd)
for tmpfile in (False, True):
    for threads,spread in ((1,False),(16,False),(16,True)):
        run(threads,spread,tmpfile)
shutil.rmtree(root)
