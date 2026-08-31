import sys, statistics as st, bisect
def load(path, prefix):
    ev=[]
    for ln in open(path, errors="replace"):
        if ln.startswith(prefix+" t_ns="):
            f=ln.split(); ev.append((int(f[1].split('=')[1]), f[2]))
    return sorted(ev)
def rep(n,v):
    if not v: print(f"  {n:48s} none"); return
    s=sorted(v); q=lambda p:s[min(len(s)-1,int(p*len(s)))]
    print(f"  {n:48s} n={len(v):5d} med={st.median(v)/1e6:8.3f}ms p90={q(.9)/1e6:8.3f} TOT={sum(v)/1e9:6.2f}s")
segs={}
for clog,slog in zip(sys.argv[1::2], sys.argv[2::2]):
    C=load(clog,"RELAXSTAT"); S=load(slog,"SSTAGE")
    if not C or not S: continue
    cev=[(t,k) for t,k in C if k in ('RingShipped','SyncPrepared','SyncSent')]
    creply=[t for t,k in C if k=='ReplyApplied']
    sread=[t for t,k in S if k=='DeltaRead']
    sapp=[t for t,k in S if k=='DeltaApplied']
    sprog=[t for t,k in S if k=='RingProgress']
    sship=[t for t,k in S if k=='ReplyShipped']
    for i,(t,k) in enumerate(cev):
        if k!='RingShipped': continue
        trio=cev[i+1:i+3]
        if len(trio)<2 or trio[0][1]!='SyncPrepared' or trio[1][1]!='SyncSent': continue
        prep,sent = trio[0][0], trio[1][0]
        def nxt(arr,after):
            j=bisect.bisect_left(arr,after); return arr[j] if j<len(arr) else None
        r=nxt(sread,sent); 
        if r is None: continue
        ap=nxt(sapp,r); pg=nxt(sprog,ap or r); sh=nxt(sship,pg or r)
        if None in (ap,pg,sh): continue
        ca=nxt(creply,sh)
        if ca is None or ca-t>5_000_000_000: continue
        segs.setdefault('1a C: diff every blob + serialize',[]).append(prep-t)
        segs.setdefault('1b C: write + flush the batch',[]).append(sent-prep)
        segs.setdefault('1c transit + S works through the batch',[]).append(r-sent)
        segs.setdefault('2a S: read -> delta applied to ring memory',[]).append(ap-r)
        segs.setdefault('2b S: applied -> head moved (VIRGLRENDERER)',[]).append(pg-ap)
        segs.setdefault('2c S: head moved -> reply on the link',[]).append(sh-pg)
        segs.setdefault('3  transit back + C applies',[]).append(ca-sh)
        segs.setdefault('TOTAL round trip',[]).append(ca-t)
order=['1a C: diff every blob + serialize','1b C: write + flush the batch','1c transit + S works through the batch',
       '2a S: read -> delta applied to ring memory','2b S: applied -> head moved (VIRGLRENDERER)',
       '2c S: head moved -> reply on the link','3  transit back + C applies','TOTAL round trip']
print("FULL ROUND TRIP, seven stages, joined on the shared clock")
for k in order:
    if k in segs: rep(k, segs[k])
tot=sum(segs.get('TOTAL round trip',[]))
if tot:
    print("\n  share of the round trip:")
    for k in order[:-1]:
        if k in segs: print(f"    {k:48s} {100*sum(segs[k])/tot:5.1f}%")
