import sys, statistics as st
def load(path):
    blocks=[]; cur=[]
    for ln in open(path, errors="replace"):
        if ln.startswith("RELAXSTAT t_ns="):
            f=ln.split(); cur.append((int(f[1].split('=')[1]), f[2]))
        elif ln.startswith("RELAXSTAT-CHUNK"): blocks.append(cur); cur=[]
    return sorted(e for b in blocks for e in b)
ray=[]; prev_kind={}
for p in sys.argv[1:]:
    ev=load(p)
    for (t0,k0),(t1,k1) in zip(ev,ev[1:]):
        d=t1-t0
        if d<5_000_000_000 and k1=='ReplyApplied':
            ray.append((d,k0))
tot=sum(d for d,_ in ray)
print(f"RAYLAND bucket: {len(ray)} intervals, {tot/1e9:.2f}s total\n")
print("  concentration — how much of our time sits in how few intervals:")
for thr_ms in (0.1,1,5,10,20):
    thr=thr_ms*1e6
    big=[d for d,_ in ray if d>thr]
    print(f"    intervals > {thr_ms:5.1f}ms: {len(big):5d} ({100*len(big)/len(ray):4.1f}% of count)  carry {100*sum(big)/tot:5.1f}% of our time")
print("\n  what preceded our long (>5ms) deliveries — i.e. what were we waiting on?")
from collections import Counter
c=Counter(k for d,k in ray if d>5_000_000)
tt=sum(d for d,k in ray if d>5_000_000)
for k,n in c.most_common():
    s=sum(d for d,kk in ray if d>5_000_000 and kk==k)
    print(f"    previous event {k:14s}: {n:5d} intervals, {s/1e9:6.2f}s ({100*s/tt:5.1f}% of our long time)")
