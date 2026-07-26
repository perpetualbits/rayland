/*
 * The shim: drives Mesa's generated per-command decoders to learn a command's length.
 *
 * Task 1 establishes only that the vendored protocol compiles against our `vkr_cs.h`. The decoding
 * entry point arrives in Task 2.
 */
#include "vkr_cs.h"

#include "vn_protocol_renderer_defines.h"
#include "vn_protocol_renderer_types.h"
#include "vn_protocol_renderer_handles.h"

/* Proves the translation unit links and the vendored headers are reachable. Replaced in Task 2. */
int
rayland_venus_proto_selftest(void)
{
   return (int)VK_COMMAND_TYPE_vkGetFenceStatus_EXT;
}
