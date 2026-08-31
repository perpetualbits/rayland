import sys, statistics as st
def load(path):
    blocks=[]; cur=[]
    for ln in open(path, errors="replace"):
        if ln.startswith("RELAXSTAT t_ns="):
            f=ln.split(); cur.append((int(f[1].split('=')[1]), f[2]))
        elif ln.startswith("RELAXSTAT-CHUNK"): blocks.append(cur); cur=[]
    return sorted(e for b in blocks for e in b)
# Attribute EVERY interval between consecutive events by WHO ENDED IT.
#   ends with RingShipped   -> the application acted (thinking, drawing, or sleeping in vn_relax)
#   ends with ReplyApplied  -> Rayland delivered (network + S execute + return + C apply)
#   ends with FrameCallback -> the compositor released the app to draw again
buckets={'APP  (interval ends: app wrote the ring)':[],
         'RAYLAND (ends: we delivered a reply)':[],
         'COMPOSITOR (ends: frame callback)':[]}
key={'RingShipped':'APP  (interval ends: app wrote the ring)',
     'ReplyApplied':'RAYLAND (ends: we delivered a reply)',
     'FrameCallback':'COMPOSITOR (ends: frame callback)'}
for p in sys.argv[1:]:
    ev=load(p)
    for (t0,_),(t1,k1) in zip(ev,ev[1:]):
        d=t1-t0
        if d < 5_000_000_000:      # drop >5s: startup/teardown, not a frame interval
            buckets[key[k1]].append(d)
tot=sum(sum(v) for v in buckets.values())
print(f"WHO OWNS THE WALL CLOCK  (total {tot/1e9:.1f}s attributed, every interval charged exactly once)\n")
for k,v in buckets.items():
    s=sorted(v); q=lambda p:s[min(len(s)-1,int(p*len(s)))]
    print(f"  {k:44s} {100*sum(v)/tot:5.1f}%   n={len(v):5d} med={st.median(v)/1e3:8.1f}us p90={q(.9)/1e6:7.2f}ms p99={q(.99)/1e6:8.2f}ms")
print()
# The vn_relax signature: long app-side intervals. If the app slept in a growing back-off, the
# APP intervals would carry most of the app's share in a few long sleeps.
app=sorted(buckets['APP  (interval ends: app wrote the ring)'])
big=[d for d in app if d>1_000_000]
print(f"  app-side intervals > 1ms: {len(big)}/{len(app)} ({100*len(big)/len(app):.0f}%), carrying {100*sum(big)/sum(app):.0f}% of the app's own time")
print(f"  app-side total {sum(app)/1e9:.2f}s of {tot/1e9:.1f}s wall = {100*sum(app)/tot:.1f}%")
