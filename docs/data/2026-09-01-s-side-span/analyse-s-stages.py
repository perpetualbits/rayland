import sys, statistics as st
def load(path, prefix):
    ev=[]
    for ln in open(path, errors="replace"):
        if ln.startswith(prefix+" t_ns="):
            f=ln.split(); ev.append((int(f[1].split('=')[1]), f[2]))
    return sorted(ev)
def rep(n,v):
    if not v: print(f"  {n:44s} none"); return
    s=sorted(v); q=lambda p:s[min(len(s)-1,int(p*len(s)))]
    print(f"  {n:44s} n={len(v):5d} med={st.median(v)/1e6:8.3f}ms p90={q(.9)/1e6:8.3f} p99={q(.99)/1e6:9.3f} TOTAL={sum(v)/1e9:6.2f}s")
segs={}
for p in sys.argv[1:]:
    ev=load(p,"SSTAGE")
    if not ev: continue
    span=(ev[-1][0]-ev[0][0])/1e9
    print(f"{p}: {len(ev)} stage events over {span:.1f}s")
    # For each DeltaRead, walk forward to the next occurrence of each downstream stage.
    idx=[i for i,(_,k) in enumerate(ev) if k=='DeltaRead']
    for i in idx:
        t0=ev[i][0]
        nxt={}
        for t,k in ev[i+1:]:
            if k=='DeltaRead': break          # next round trip begins; stop
            if k not in nxt: nxt[k]=t
        if 'DeltaApplied' in nxt:
            segs.setdefault('A read -> applied (S message thread)',[]).append(nxt['DeltaApplied']-t0)
            if 'RingProgress' in nxt:
                segs.setdefault('B applied -> head moved (VIRGLRENDERER)',[]).append(nxt['RingProgress']-nxt['DeltaApplied'])
                if 'VenusReply' in nxt:
                    segs.setdefault('C head moved -> reply bytes in hand',[]).append(nxt['VenusReply']-nxt['RingProgress'])
                if 'ReplyShipped' in nxt:
                    segs.setdefault('D head moved -> reply on the link',[]).append(nxt['ReplyShipped']-nxt['RingProgress'])
                    segs.setdefault('TOTAL read -> reply shipped',[]).append(nxt['ReplyShipped']-t0)
print("\nS-SIDE SPAN, per relayed ring delta")
for k in ['A read -> applied (S message thread)','B applied -> head moved (VIRGLRENDERER)',
          'C head moved -> reply bytes in hand','D head moved -> reply on the link',
          'TOTAL read -> reply shipped']:
    if k in segs: rep(k, segs[k])
tot=sum(segs.get('TOTAL read -> reply shipped',[]))
if tot:
    print("\n  share of the S-side span:")
    for k in ['A read -> applied (S message thread)','B applied -> head moved (VIRGLRENDERER)','D head moved -> reply on the link']:
        if k in segs: print(f"    {k:44s} {100*sum(segs[k])/tot:5.1f}%")
