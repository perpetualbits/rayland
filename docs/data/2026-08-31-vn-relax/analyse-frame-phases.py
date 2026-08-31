import sys, statistics as st
def load(path):
    blocks=[]; cur=[]
    for ln in open(path, errors="replace"):
        if ln.startswith("RELAXSTAT t_ns="):
            f=ln.split(); cur.append((int(f[1].split('=')[1]), f[2]))
        elif ln.startswith("RELAXSTAT-CHUNK"): blocks.append(cur); cur=[]
    return sorted([e for b in blocks for e in b])
allsegs={}
for path in sys.argv[1:]:
    ev=load(path)
    if not ev: continue
    n={k:sum(1 for _,x in ev if x==k) for k in ('RingShipped','ReplyApplied','FrameCallback')}
    span=(ev[-1][0]-ev[0][0])/1e9
    print(f"{path}: {len(ev)} events over {span:.1f}s  ring={n['RingShipped']} reply={n['ReplyApplied']} frameCB={n['FrameCallback']}")
    # A frame is bounded by successive FrameCallbacks. Inside one frame, split the wall clock by
    # which phase each interval belongs to, attributing every nanosecond exactly once.
    cbs=[t for t,k in ev if k=='FrameCallback']
    if len(cbs)<3: continue
    for i in range(len(cbs)-1):
        a,b=cbs[i],cbs[i+1]
        inner=[(t,k) for t,k in ev if a<t<=b]
        if not inner: 
            allsegs.setdefault('cb->cb (nothing between)',[]).append(b-a); continue
        # phase 1: frame callback -> app's first ring write (app reacting + our park)
        first_ring=next((t for t,k in inner if k=='RingShipped'), None)
        # phase 3: last reply -> next frame callback (waiting on the compositor)
        last_reply=None
        for t,k in inner:
            if k=='ReplyApplied': last_reply=t
        if first_ring is None or last_reply is None or last_reply<first_ring: continue
        allsegs.setdefault('1 frameCB -> app writes ring',[]).append(first_ring-a)
        allsegs.setdefault('2 ring out -> last reply in (S+relay)',[]).append(last_reply-first_ring)
        allsegs.setdefault('3 last reply -> next frameCB',[]).append(b-last_reply)
        allsegs.setdefault('TOTAL frame (cb->cb)',[]).append(b-a)
print()
def rep(n,v):
    s=sorted(v); q=lambda p:s[min(len(s)-1,int(p*len(s)))]
    print(f"  {n:38s} n={len(v):4d} med={st.median(v)/1e6:8.2f}ms p90={q(.9)/1e6:8.2f} TOTAL={sum(v)/1e9:7.2f}s")
order=['1 frameCB -> app writes ring','2 ring out -> last reply in (S+relay)','3 last reply -> next frameCB','TOTAL frame (cb->cb)']
print("FRAME DECOMPOSITION (every ns attributed exactly once)")
for k in order:
    if k in allsegs: rep(k, allsegs[k])
tot=sum(allsegs.get('TOTAL frame (cb->cb)',[]))
if tot:
    print("\n  share of frame time:")
    for k in order[:3]:
        if k in allsegs: print(f"    {k:38s} {100*sum(allsegs[k])/tot:5.1f}%")
