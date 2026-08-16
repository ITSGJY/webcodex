# MCP Computer App image-result contract

This note records the durable conclusion from the August 2026 Computer App gray-card investigation. It is intentionally short; the temporary diagnostic tools used during the investigation were removed after the fix was verified.

## Observed failure

`computer_snapshot` could return a successful native MCP image while the bound Computer App card rendered as an empty gray surface. A successful `resources/read` response alone did not prove that the host had accepted the template or delivered the tool result to the App.

## Decisive controls

The investigation progressively held the App/resource/result shape constant while varying one image property at a time:

- tiny text results rendered, proving the basic MCP App resource, initialize, and tool-result path;
- a 68-byte native PNG rendered, proving native image ContentBlocks could cross the host/App bridge;
- synthetic native images rendered from 1 KiB through 512 KiB;
- fixed-size synthetic images decoded successfully through 3840x2160 intrinsic dimensions;
- synthetic 4K JPEGs decoded successfully at 64, 256, and 512 KiB;
- a real Runner JPEG (3840x1912, 419,991 bytes in the confirming run) also decoded normally once the descriptor contract was corrected.

These controls ruled out JPEG as a type, 4K dimensions, ordinary native-image framing, and payload sizes through 512 KiB as the cause of the observed gray card.

## Root cause and permanent invariant

The generic ToolRuntime `computer_snapshot` output contains `content_base64`. MCP native-image framing deliberately removes that field from `structuredContent.output`, adds `content_delivery = "mcp_image"`, and carries the binary bytes in an MCP image ContentBlock.

The gray-card build advertised the generic **pre-framing** output schema in MCP `tools/list`, even though MCP returned the **post-framing** structured result. After the MCP-facing descriptor was changed to omit `content_base64` and declare `content_delivery = "mcp_image"`, both the decode-aware real-snapshot control and the production `computer_snapshot` card rendered the same real browser screenshot normally without refresh or retry.

**Invariant:** a transport adapter that rewrites structured output must advertise the post-adapter schema on that transport. Do not reuse a pre-adapter ToolRuntime schema when the fields actually delivered to the MCP client differ. Keep the generic ToolRuntime/API schema unchanged when only the MCP representation changes.

The regression in `src/mcp_tests.rs` intentionally asserts both sides of this boundary: the runtime schema retains `content_base64`, while the MCP-facing `computer_snapshot` schema exposes `content_delivery = "mcp_image"` instead.

## Separate macOS lock-screen observation

During the same investigation, a locked Mac produced `image_too_large: cannot establish a bounded macOS capture scale`. After unlock, the same Edge window captured normally at the expected Retina 2x scale (1920x956 logical to 3840x1912 backing pixels). Treat this as a separate capture-scale/display-state issue, not as evidence of an MCP App gray-card failure. The current fail-closed behavior is safer than guessing a scale; error classification/message cleanup can be handled independently.
