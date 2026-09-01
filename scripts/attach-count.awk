# Count (or timestamp) the frames an application actually put on screen, as seen by rayland-c's
# Wayland proxy log.
# =============================================================================================
#
# WHAT A FRAME IS, HERE
#   A Wayland application presents by attaching a buffer to a surface and committing it. The proxy
#   on C logs every request it forwards as `forward obj <app_obj> opcode <n>`, and logs the object
#   table as `objects+ app_obj=<n> <interface>`. `wl_surface.attach` is opcode 1, so the number of
#   frames the application presented is the number of forwarded opcode-1 requests **on an object
#   that is a wl_surface**.
#
# WHY THIS IS A FILE AND NOT A ONE-LINE grep
#   Both `wp0-soak.sh` and `milkv-demo.sh` used to count `forward obj 3 opcode 1` — with the object
#   id **3 hardcoded**. Object 3 is `vkcube`'s surface. It is not a constant: it is whatever id the
#   application's own Wayland client happened to allocate, and `vkgears` allocates 6.
#
#   The consequence was not a cosmetic one. Every `vkgears` run either harness ever scored reported
#   **zero frames** — identically for a run rendering at 33 FPS and for a run that had genuinely
#   stopped. On 2026-09-01 that produced a recorded finding, an evidence directory and a commit
#   asserting that `vkgears` "hangs" on the riscv64 board and "never attaches a buffer". Re-scoring
#   that session's own archived log with this program shows **634 attaches, 634 frame callbacks and
#   632 `wl_buffer.release` events in 35 s** — a healthy 18 FPS render. There was no hang. The
#   number was an artefact of the hardcoded id, and `milkv-demo.sh` printed it to the screen every
#   ten seconds while a human watched and drew the obvious conclusion.
#
#   So the id is now READ FROM THE LOG, every surface the application creates is counted, and there
#   is exactly ONE copy of this logic for both harnesses to share. Two copies would drift, and the
#   drift would again be invisible: a wrong frame count does not look wrong, it looks like a result.
#
# OPCODE 1 IS ONLY `attach` ON wl_surface
#   Opcode numbers are per-interface. Opcode 1 on some other interface means something else
#   entirely, which is why the object id is checked against the surface table rather than assumed —
#   counting every opcode-1 request would silently inflate the frame count instead of zeroing it.
#
# USAGE
#   awk -f attach-count.awk              c.log   -> a single integer: frames presented
#   awk -v mode=timeline -f attach-count.awk c.log -> the t_ns of each frame, one per line
#
# The count is printed even when it is zero, and a zero here is a real zero — which is the whole
# point of the change.

/objects\+ app_obj=[0-9]+ wl_surface$/ {
  # The `$` anchor matters: without it this would also match a longer interface name that merely
  # begins with `wl_surface`, and quietly count another object's requests as frames.
  if (match($0, /app_obj=[0-9]+/)) surf[substr($0, RSTART + 8, RLENGTH - 8)] = 1
}

/forward obj [0-9]+ opcode 1 / {
  # The trailing space is deliberate — it stops `opcode 1` from also matching `opcode 10`.
  if (match($0, /obj [0-9]+/)) {
    id = substr($0, RSTART + 4, RLENGTH - 4)
    if (id in surf) {
      n++
      if (mode == "timeline" && match($0, /t_ns=[0-9]+/)) print substr($0, RSTART + 5, RLENGTH - 5)
    }
  }
}

END { if (mode != "timeline") print n + 0 }
