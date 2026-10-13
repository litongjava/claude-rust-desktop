# Conversation work details and Trajectory

The implementation was informed by the locally installed DeepSeek Harness bundles
`@deepseek-ai/dsh-client-ui-conversation/lib/client.js` and
`@deepseek-ai/dsh-client-ui-trajectory/lib/client.js` inside its `resources/app.asar`.
Relevant pieces are `ConversationLocationIndex`, `TrajectorySnapshotBuilder`,
`trajectoryViewDefinition.toolCallFocus`, and `deriveTrajectoryLayout`. Harness
indexes events by turn/step and call ID, builds a snapshot, and shares selection
between the timeline, ledger, and inspector. Duration switches between actual
time and equal-width events; Turns and Calls control expansion.

This application's implementation uses its existing stream callbacks instead of
Harness's plugin/event infrastructure:

1. `createStreamCallbacks.ts` captures ordered text, thinking, and tool blocks.
   A tool's start/input/result updates share its original call ID. Buffered text
   is flushed before tool boundaries so commentary stays before its call.
2. `features/trajectory/model.ts` creates the common process representation.
   `buildTrace` adds turn/step coordinates. Inspect uses `message:index/step-id`,
   including the tool call ID, rather than matching visible text.
3. `WorkDetails.tsx` renders that representation at four detail levels. Standard,
   Detailed, and Verbose copy commands and results from full underlying values;
   scrolling and folding never truncate copied data. Coding view controls Inspect
   and the Trajectory view independently of work-detail verbosity.
4. `TrajectoryView.tsx` renders Input/Model/Tools lanes, an event list, and a
   selected-record inspector. Selecting a bar or Inspect opens the same record.
   Search includes input and output. Duration uses recorded times, while the
   default equal-width view also includes older events without timings.
5. Before each model request, the native engine captures the system prompt argument,
   additional system messages (including retrieved MemEx context), model and tool
   definitions after project/deep-mode injection. The desktop SSE route stores
   these in SQLite `message_trajectory`, linked to the assistant message, before
   forwarding `request_snapshot` to the frontend. The initial snapshot appears
   first as SYSTEM / Initial System Prompt. Clearing WebView storage does not
   remove these snapshots. They cascade with message/conversation deletion.
6. Ordered frontend process metadata is saved with the existing local message cache. Server
   messages remain authoritative: cached metadata is restored only when role,
   full content, and tool sequence match. This accommodates the server assigning
   different message/tool IDs without attaching stale events to edited messages.

The tool inspector has Summary / Payload / Result / Schema / Timing. Assistant
records have Summary / Preview / Raw, with Markdown in Preview. System records
have System Prompt / Tools. Each turn folds independently; the Turns button
expands/collapses all turns. Turn and Step share a compact single-line row.

Step timestamps currently measure frontend receipt of events, not provider TTFT or
server CPU time. Historical messages without metadata use existing text/tool
fields, with unavailable timings marked as unrecorded. Tool Schema is shown only
when captured on a tool event or in its request snapshot; it is never inferred
from argument examples. Request timestamps are measured in the native engine.
The UI does not invent or reconstruct unrecorded historical system prompts.
Clearing the local message
cache removes frontend-only event order/timings, leaving server history and
request snapshots available. Snapshots record provider adapter inputs, not HTTP
headers or secrets. Provider adapters may normalize these inputs for their wire
format (for example the JS runtime merges system messages into systemPrompt).

The conversation tab strip is removed. `useTabStore` retains a single active
conversation for compatibility with existing navigation and streaming code;
history remains accessible through the sidebar. Chat and Trajectory are views of
that same conversation.
