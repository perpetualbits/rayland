import mmap, os, struct, sys, ctypes
PAGE, N = 4096, 8
libc = ctypes.CDLL("libc.so.6", use_errno=True)
libc.mmap.restype = ctypes.c_void_p
libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_long]
MAP_PRIVATE, MAP_ANONYMOUS, PROT_READ, PROT_WRITE = 0x02, 0x20, 1, 2
addr = libc.mmap(None, PAGE*N, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0)
if addr in (None, 0) or addr == 2**64-1:
    print("PROBE FAILED: mmap"); sys.exit(2)
buf = (ctypes.c_char*(PAGE*N)).from_address(addr)
for i in range(N): buf[i*PAGE] = b'\0'      # fault every page in
def entry(idx):
    with open('/proc/self/pagemap','rb') as f:
        f.seek((addr//PAGE + idx)*8)
        return struct.unpack('<Q', f.read(8))[0]
open('/proc/self/clear_refs','w').write("4")
before = [(entry(i)>>55)&1 for i in range(N)]
buf[3*PAGE] = b'X'                           # dirty exactly one page
after  = [(entry(i)>>55)&1 for i in range(N)]
present = [(entry(i)>>63)&1 for i in range(N)]
print(f"  present: {present}")
print(f"  soft-dirty before clear+write: {before}")
print(f"  soft-dirty after writing page 3: {after}")
ok = after[3]==1 and sum(after)==1 and sum(before)==0
print("VERDICT:", "SOFT-DIRTY WORKS AND DISCRIMINATES" if ok else "SOFT-DIRTY UNUSABLE ON THIS ARCH/KERNEL")
