# The exact scenario rayland-c needs: process A (the "app") writes a MAP_SHARED memfd;
# process B (rayland-c) reads A's pagemap to learn which pages A dirtied.
import os, mmap, struct, sys, ctypes, time
PAGE, NPAGES = 4096, 64
libc = ctypes.CDLL("libc.so.6", use_errno=True)
libc.memfd_create.restype = ctypes.c_int
libc.memfd_create.argtypes = [ctypes.c_char_p, ctypes.c_uint]
fd = libc.memfd_create(b"probe", 0)
os.ftruncate(fd, PAGE*NPAGES)
pid = os.fork()
if pid == 0:                                   # child = the "application"
    m = mmap.mmap(fd, PAGE*NPAGES, mmap.MAP_SHARED, mmap.PROT_READ|mmap.PROT_WRITE)
    for i in range(NPAGES): m[i*PAGE] = 0      # fault every page in
    open('/tmp/sd_child_ready','w').write(str(os.getpid()))
    while not os.path.exists('/tmp/sd_go'): time.sleep(0.01)
    m[7*PAGE] = 65                             # dirty exactly page 7
    m[40*PAGE] = 66                            # and page 40
    open('/tmp/sd_done','w').write("1")
    time.sleep(5); os._exit(0)
# parent = "rayland-c"
for f in ('/tmp/sd_go','/tmp/sd_done'):
    if os.path.exists(f): os.unlink(f)
while not os.path.exists('/tmp/sd_child_ready'): time.sleep(0.01)
time.sleep(0.3)
child = int(open('/tmp/sd_child_ready').read())
def child_range():
    for ln in open(f'/proc/{child}/maps'):
        if 'probe' in ln and 'rw-s' in ln:
            lo,hi = (int(x,16) for x in ln.split()[0].split('-'))
            return lo,hi
    return None
rng = child_range()
if not rng: print("PROBE FAILED: no shared mapping found in the child"); os._exit(2)
lo,hi = rng
def dirty_pages():
    out=[]
    with open(f'/proc/{child}/pagemap','rb') as f:
        f.seek((lo//PAGE)*8)
        data=f.read(((hi-lo)//PAGE)*8)
    for i in range(0,len(data),8):
        e=struct.unpack('<Q', data[i:i+8])[0]
        if (e>>55)&1: out.append(i//8)
    return out
open(f'/proc/{child}/clear_refs','w').write("4")     # clear from the OTHER process
before = dirty_pages()
open('/tmp/sd_go','w').write("1")
while not os.path.exists('/tmp/sd_done'): time.sleep(0.01)
time.sleep(0.2)
after = dirty_pages()
print(f"  mapping {hi-lo} bytes = {(hi-lo)//PAGE} pages")
print(f"  dirty after clear (expect []):        {before}")
print(f"  dirty after child wrote pages 7,40:   {after}")
ok = set(after) == {7,40} and not before
print("VERDICT:", "SHARED-MEMFD SOFT-DIRTY WORKS CROSS-PROCESS" if ok else
      ("PARTIAL: " + str(after)) if after else "DOES NOT WORK for shared memfd")
os.kill(child, 9)
