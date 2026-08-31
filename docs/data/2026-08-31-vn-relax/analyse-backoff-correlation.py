import sys, statistics as st
def load(path):
    blocks=[]; cur=[]
    for ln in open(path, errors="replace"):
        if ln.startswith("RELAXSTAT t_ns="):
            f=ln.split(); cur.append((int(f[1].split('=')[1]), f[2]))
        elif ln.startswith("RELAXSTAT-CHUNK"): blocks.append(cur); cur=[]
    return sorted(e for b in blocks for e in b)
pairs=[]
for p in sys.argv[1:]:
    ev=load(p)
    for i,(t,k) in enumerate(ev):
        if k!='RingShipped': continue
        # how long had the ring been quiet before this delta? (time since the previous event)
        if i==0: continue
        idle = t-ev[i-1][0]
        # how long until the first reply came back?
        nxt=next(((t2,k2) for t2,k2 in ev[i+1:] if k2=='ReplyApplied'), None)
        if not nxt: continue
        wait = nxt[0]-t
        if idle<5e9 and wait<5e9: pairs.append((idle,wait))
print(f"n={len(pairs)} ring deltas with a measurable preceding idle and following wait\n")
print("HYPOTHESIS: if virglrenderer's ring thread is in a grown back-off, a LONGER preceding idle")
print("should predict a LONGER wait for the first reply.\n")
bins=[(0,0.2),(0.2,1),(1,5),(5,20),(20,1e9)]
print(f"  {'preceding idle (ms)':24s} {'n':>5s} {'median wait (ms)':>18s} {'p90':>8s}")
for lo,hi in bins:
    sel=[w for idl,w in pairs if lo*1e6<=idl<hi*1e6]
    if not sel: continue
    s=sorted(sel)
    lbl=f"{lo:g} - {hi:g}" if hi<1e9 else f">{lo:g}"
    print(f"  {lbl:24s} {len(sel):5d} {st.median(sel)/1e6:18.2f} {s[int(.9*len(s))]/1e6:8.2f}")
# Spearman rank correlation
def rank(v):
    order=sorted(range(len(v)), key=lambda i:v[i]); r=[0]*len(v)
    for pos,i in enumerate(order): r[i]=pos
    return r
x=[a for a,_ in pairs]; y=[b for _,b in pairs]
rx,ry=rank(x),rank(y); n=len(x)
mx,my=sum(rx)/n,sum(ry)/n
num=sum((a-mx)*(b-my) for a,b in zip(rx,ry))
den=(sum((a-mx)**2 for a in rx)*sum((b-my)**2 for b in ry))**.5
rho=num/den
import math
z=rho*math.sqrt(n-1)
print(f"\n  Spearman rho = {rho:+.3f}  (z={z:+.1f}, n={n})")
print("  rho >> 0 supports a host-side back-off; rho ~ 0 refutes it.")
