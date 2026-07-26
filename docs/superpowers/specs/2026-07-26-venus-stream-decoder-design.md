# The Venus command-stream decoder — walking a stream, not naming its first command

*Design spec. 2026-07-26. Branch context: `wp0-wayland-proxy`. Applies to `rayland-vtest` and a new
`rayland-venus-proto` crate.*

## Why this exists

Rayland can relay Venus command streams perfectly and cannot read them. Both halves of that sentence are
now measured facts, and the gap between them is what this spec closes.

The relay is sound. Over a real application's load, C and S independently fingerprinted every ring delta and
agreed on all 253 of them — same length, same digest. Every blob's contents agree too: the application's
memory, the reply arena, and (once shipped experimentally) even Venus's staging pool. Nothing is lost,
corrupted, reordered or truncated in transit.

And yet `rayland-s` cannot execute vkcube's `vkQueueSubmit`. virglrenderer reports
`vkr: vkQueueSubmit resulted in CS error`, which its generated dispatcher defines precisely:

```c
vn_decode_VkCommandTypeEXT(ctx->decoder, &cmd_type);
vn_decode_VkFlags(ctx->decoder, &cmd_flags);
vn_dispatch_table[cmd_type](ctx, cmd_flags);
if (vn_cs_decoder_get_fatal(ctx->decoder))
   vn_dispatch_debug_log(ctx, "%s resulted in CS error", vn_dispatch_command_name(cmd_type));
```

The decoder's fatal flag has exactly one trigger: **a read past the end of the available bytes.** At the
moment of failure S's own ring words read `s_head=27356 s_tail=27536` — the decoder was handed 180 bytes for
the command at 27356 and needed more. So the failure is about the **extent** of a command, not its content.

Two explanations remain, and they call for opposite fixes:

1. **A published `tail` can fall mid-command.** Mesa stores `tail` after writing a command, so it should
   always be a boundary — but "should" has been wrong repeatedly in this investigation.
2. **Part of the submit lives outside the ring**, reached through a `VkCommandStreamDescriptionMESA`
   (`vkExecuteCommandStreamsMESA`, command type 180) whose `{resourceId, offset, size}` S resolves
   differently than C intends.

**Distinguishing them requires walking the stream, and Rayland cannot.** `venus_ring::decode::encoded_size`
returns a size only for commands with a *fixed* encoding — three of them. Its own documentation names the
limit and the reason:

> `VK_COMMAND_TYPE_VK_CREATE_INSTANCE`, `VK_COMMAND_TYPE_VK_CREATE_RING_MESA` and
> `VK_COMMAND_TYPE_VK_EXECUTE_COMMAND_STREAMS_MESA` are all variable-length. Recognising a command is not the
> same as being able to skip it, and conflating the two is how a decoder desynchronizes.

Most application commands are variable-length — arrays, `pNext` chains, optional pointers — so **no table of
sizes can ever walk a real stream.** The decoder halts at the application's first real command, every time.
That behaviour has been genuinely useful (it is how every wall this week was *named*), and it is now the
ceiling. The question "where does this command end, and what else is in this stream" cannot be asked at all.

## What this is not

**This decoder must never make a correctness decision.** (c)1 spec §7 chose to relay the ring as opaque bytes
precisely so that *a decoding bug cannot become a corruption bug*, and that reasoning is untouched by this
work. Nothing here may be used to decide what to ship, when to ship it, which blobs a delta reads, or whether
a relay is safe. It may report, log, measure, assert in tests, and answer questions. **The invariant is
binding, not advisory**, and the implementation plan must carry a test that the relay path does not call it.

The distinction is not fussiness. A decoder that is merely *observing* can be wrong and produce a wrong
diagnosis, which a human notices. A decoder that is *deciding* can be wrong and produce a silently corrupted
frame, which nobody notices until it is a week of someone's life — which is the exact shape of every wall this
investigation has hit.

## The idea, in one sentence

**Mesa already contains a complete, correct, generated decoder for its own protocol; compile it and ask it
where each command ends, rather than reimplementing 73,442 lines of generated C in Rust.**

## Why borrow rather than build

This mirrors `CLAUDE.md`'s locked decision about the rendering engine, for the same reasons and with the same
shape. Rayland reuses Venus/virglrenderer rather than writing a Vulkan capture/replay engine, because the
engine "already exists and is hardened against our exact threat model", and the borrowed artifact sits behind
a clean Rust trait boundary. The venus-protocol headers are the same category of artifact: generated, exact,
maintained by the people who define the format, and updated in lockstep with the Mesa that produces the
streams we are reading.

Three options were weighed:

| approach | cost | drift risk | verdict |
|---|---|---|---|
| **FFI shim over Mesa's `venus-protocol` headers** | small C shim + a generated switch | **none** — it *is* Mesa's decoder | **chosen** |
| Generate Rust by parsing those C headers | a C parser, re-validated per Mesa release | moderate; a parser bug is a silent decode bug | rejected |
| Generate Rust from `vk.xml`, reimplementing Venus's encoding rules | a second implementation of the protocol | high; any divergence is silent | rejected |

The decisive argument against both generators is that they create a **second source of truth for a format
Rayland does not own**. When it diverges from Mesa's — and it will, at some Mesa release — the symptom is a
decoder that confidently reports the wrong thing. Borrowing cannot diverge.

**A qualification on that "none," recorded rather than left as an unconditional claim:** the drift risk is
none only for as long as the vendored `venus-protocol` tree, the virglrenderer S actually links, and the
Venus ICD C actually runs are all the same protocol version. That holds today by construction — S and C both
run against the Mesa this workspace pins — but it is not a law of the approach, only a fact about the current
deployment. The one way this design *could* still return a plausible wrong length is a newer ICD speaking a
protocol revision this vendored copy predates (a field added to a command this crate's table or borrowed
decoder does not yet know to expect); see `vendor/MESA_VERSION`'s update checklist for what re-vendoring must
re-verify before that risk is closed again.

The cost of borrowing is honest and stated below: a second C dependency, and a crate whose "no dependencies
but `libc` and `thiserror`" property changes.

## Feasibility: why the borrowed decoders can run outside virglrenderer

The renderer-side decoders resolve object handles, which at first looks like it requires a live virglrenderer
context. It does not, and the reason is visible in the generated code:

```c
vn_decode_VkDevice_lookup(struct vn_cs_decoder *dec, VkDevice *val)
{
    uint64_t id;
    vn_decode_uint64_t(dec, &id);                                   /* consumes 8 bytes */
    *val = (VkDevice)vn_cs_decoder_lookup_object(dec, id, VK_OBJECT_TYPE_DEVICE);  /* consumes nothing */
}
```

**Byte consumption is independent of the lookup's result.** The decode reads a `u64` and then interprets it;
how far the cursor advanced does not depend on whether the object exists. A shim whose `lookup_object`
returns null therefore produces *identical framing* to a live renderer.

Validation of those looked-up handles happens in the **dispatch** functions, not the decoders —
`if (!args.device) { vn_cs_decoder_set_fatal(dec); }` sits in `vkr_dispatch_*`, after decoding. So the shim
calls `vn_decode_<command>_args_temp` directly and never enters dispatch, which means it never validates,
never executes, and never touches a GPU. That is exactly the property this decoder needs: **it reads the
shape of the stream and nothing else.**

Two consequences follow, both design requirements rather than accidents:

- The decoders branch on *decoded* values — array counts, `pNext` chain presence, optional-pointer markers.
  That is desirable and is precisely what makes variable-length framing correct.
- The `_args_temp` functions allocate for arrays, from the decoder's temporary pool. The shim must provide a
  working allocator; a bump allocator over a caller-owned buffer is sufficient, since nothing outlives one
  command.

## Architecture

A new crate, **`rayland-venus-proto`**, whose entire public surface is one question:

```rust
/// How many bytes does the command at the start of `stream` occupy?
pub fn command_len(stream: &[u8]) -> Result<usize, DecodeFault>;
```

Everything else — walking, reporting, the `RingCommand` list, the `DecodeStop` taxonomy — stays in
`rayland-vtest`'s `venus_ring::decode`, in Rust, where it already is and is already tested.

That boundary is the important design choice. The borrowed C is confined to answering a single, total,
side-effect-free question about a byte slice. It holds no state between calls, owns no resources, and has no
opinion about rings, relays or blobs. If Mesa's protocol is ever replaced, or the shim is ever rewritten in
Rust, `command_len` is the only thing that has to keep its meaning.

### The three pieces

**1. The C shim** (`rayland-venus-proto/csrc/`). Compiles Mesa's `venus-protocol` headers together with
virglrenderer's `vkr_cs.h`, and provides:
- a `vn_cs_decoder` positioned over the caller's buffer;
- a bump allocator for `_args_temp` allocations, reset per call;
- stub object lookups returning null, per the feasibility argument above;
- a `switch` on `VkCommandTypeEXT` calling the matching `vn_decode_<command>_args_temp`;
- a return of either the cursor's advance, or a fault code if the decoder went fatal.

**2. The switch generator** (build-time). The switch is ~331 one-line cases and must track Mesa, so it is
generated rather than typed: a small script scans the headers for `vn_decode_*_args_temp` symbols and emits
the cases. This is a *code generator over symbol names*, not a C parser — it does not need to understand
types, only to enumerate them. That is what makes it robust where option 2 above was not.

**3. The Rust wrapper.** One `unsafe` module, converting a `&[u8]` into the shim's arguments and its fault
code into a typed error. No other crate sees `unsafe` or C.

### How it joins the existing decoder

`decode_commands`'s signature, return type and semantics are unchanged. Today it walks until `encoded_size`
returns `None`. It will walk until *both* `encoded_size` and `command_len` decline. `encoded_size` is kept as
the first question rather than deleted, for two reasons: it is a pure-Rust, dependency-free answer for the
three commands Rayland cares about most (including the doorbell), and it is an independent cross-check —
where both answer, they must agree, and a test will assert exactly that.

## What this unlocks beyond the current bug

Stated because the cost is real and should be justified by more than one investigation:

- **The immediate question**, directly: walk the stream up to the failing submit and see whether the relayed
  `tail` lands on a command boundary, and whether a `vkExecuteCommandStreamsMESA` refers outside the ring.
- **Honest diagnostics for every future wall.** Every stall this week was named by a decoder that could see
  one command; several were misdiagnosed for want of seeing the next one.
- **Precise measurement.** "Which commands does this application actually use, and how much of the stream is
  each" is currently unanswerable, and it is the input to any serious work on the round-trip count that
  ring-findings §7 identifies as the real cost.
- **A test oracle.** The captured-ring fixture can be asserted against in full rather than in prefix.

It does **not** unlock, and must not be used for: deciding what to relay, deciding when, or deciding which
blobs a delta touches. See "What this is not".

## Testing

- **The captured fixture, walked whole.** `venus_ring`'s existing captured ring currently decodes to
  `UnknownCommandSize`. It must now reach `DecodeStop::ReachedEnd`, and the sum of the decoded command sizes
  must equal the `head` byte counter the *real* host wrote into that capture. That equality is the anchor:
  the sizes come from Mesa's decoder and the total comes from virglrenderer's consumer, so agreement is two
  independent implementations meeting — not a tautology.
- **Cross-check against the existing table.** For the three commands `encoded_size` knows, `command_len` must
  return the same number. A disagreement means one of them is wrong and the build should say so.
- **Fault handling, not just success.** A truncated stream, a stream cut mid-command, and an unknown command
  type must each produce a typed fault rather than a panic, a hang, or a plausible-looking wrong answer.
- **The prohibition, enforced.** A test asserting that `rayland-c`'s relay path does not depend on the
  decoder — the mechanical counterpart to "this may never make a correctness decision".
- **`no_gpu_linkage` re-verified, not assumed.** `rayland-c` links this crate; the guard must be re-run and
  its meaning re-read, because the crate it guards has changed shape.

## Costs and consequences, stated plainly

- **`rayland-vtest` loses a property it advertises.** Its crate docs and `CLAUDE.md` both state it "has no GPU
  dependencies, by construction: only `libc` and `thiserror`". Depending on `rayland-venus-proto` adds a C
  dependency. It remains **GPU-free** — the venus-protocol headers are protocol only, with no driver, no
  device and no `libvirglrenderer` — but the sentence as written becomes false and must be updated in the same
  change, per this repository's own rule about stale documentation.
- **A vendored copy of Mesa's headers, with a recorded version.** The build must not depend on a Mesa checkout
  happening to exist in a scratch directory. The headers are vendored, their Mesa version recorded, and the
  fixture test is what detects drift when they are updated.
- **`cc` at build time.** A build dependency on a C compiler for `rayland-vtest`'s dependents, including
  `rayland-c` — the crate that runs on the weak machine. This is a build-host requirement, not a runtime one.
- **The shim is `unsafe` by nature.** It is confined to one crate, one module, and one function's worth of
  contract, and it is exercised by the fixture on every test run.

## Open questions for the implementation plan

- **Where the vendored headers live and how they are updated** — a directory in the crate, with the Mesa
  version in a file beside them, is the obvious answer, but the update procedure (and what the fixture test
  is expected to catch) should be written down before the first update rather than after.
- **Whether `command_len` should also report the command type.** It has it in hand, and the caller currently
  re-reads it. Returning it would remove a duplicated read, at the cost of a wider contract for the one
  function this design deliberately keeps narrow. Deferred to the plan.
