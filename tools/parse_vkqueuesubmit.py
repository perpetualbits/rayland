#!/usr/bin/env python3
"""Decode a captured vkQueueSubmit command's fields from its raw bytes.

Field order transcribed by hand from the vendored `vn_decode_vkQueueSubmit_args_temp` /
`vn_decode_VkSubmitInfo_temp` / `_self_temp`, and validated against real captures: it consumes
exactly 36 bytes for a fence-only submit and exactly 120 for a one-VkSubmitInfo submit, with
`_exact` reporting whether the walk landed on the end.

Why it exists: the vkQueueSubmit that stalled (c)1 for three days was eventually root-caused to
VK_ERROR_DEVICE_LOST, but getting there needed the failing submit compared field-by-field against an
accepted one from the same run. That comparison is what showed the refused submit was the *second
swapchain image's* resource set -- identical in structure, different handles -- which killed several
theories at once.

Feed it the hex from rayland-c's `[ring-queue] submit @N bytes(M) <hex>` diagnostic
(RAYLAND_RING_DUMP=1):

    python3 tools/parse_vkqueuesubmit.py <hex> [<hex> ...]
"""
import struct, sys
# Field order transcribed from the vendored vn_decode_vkQueueSubmit_args_temp /
# vn_decode_VkSubmitInfo_temp / _self_temp. Arrays are [u64 size][elements].
class R:
    def __init__(s,b): s.b=b; s.o=0
    def u32(s):
        v=struct.unpack_from('<I',s.b,s.o)[0]; s.o+=4; return v
    def u64(s):
        v=struct.unpack_from('<Q',s.b,s.o)[0]; s.o+=8; return v
def parse(hexstr):
    b=bytes.fromhex(hexstr); r=R(b); out={}
    out['type']=r.u32(); out['flags']=r.u32(); out['queue']=hex(r.u64())
    out['submitCount']=r.u32()
    n=r.u64(); out['pSubmits_arraysize']=n
    subs=[]
    for _ in range(n):
        si={}
        si['sType']=r.u32()
        si['pNext_marker']=r.u64()          # 0 = no pNext chain
        si['waitSemaphoreCount']=r.u32()
        k=r.u64(); si['waitSem_n']=k; si['waitSemaphores']=[hex(r.u64()) for _ in range(k)]
        k2=r.u64(); si['waitDstStage_n']=k2; si['pWaitDstStageMask']=[hex(r.u32()) for _ in range(k2)]
        si['commandBufferCount']=r.u32()
        k3=r.u64(); si['cmdbuf_n']=k3; si['pCommandBuffers']=[hex(r.u64()) for _ in range(k3)]
        si['signalSemaphoreCount']=r.u32()
        k4=r.u64(); si['sigSem_n']=k4; si['pSignalSemaphores']=[hex(r.u64()) for _ in range(k4)]
        subs.append(si)
    out['pSubmits']=subs
    out['fence']=hex(r.u64())
    out['_consumed']=r.o; out['_total']=len(b); out['_exact']= (r.o==len(b))
    return out
if __name__ == "__main__":
    import json
    for h in sys.argv[1:]:
        print(json.dumps(parse(h),indent=1))
