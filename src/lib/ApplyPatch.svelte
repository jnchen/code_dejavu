<script lang="ts">
  import type { SessionRecord } from "$lib/types";

  let {
    record,
    result = null,
  }: { record: SessionRecord; result?: SessionRecord | null; highlight?: string } = $props();

  // Codex apply_patch carries the raw patch text in tool_input.input.
  let patch = $derived(String((record.tool_input as Record<string, unknown> | null)?.input ?? ""));
  let lines = $derived(patch.split(/\r?\n/));
  let resultText = $derived(result?.content_preview ?? "");

  type Kind = "file" | "hunk" | "add" | "del" | "ctx" | "marker";
  function kind(line: string): Kind {
    // Codex *** Begin/End Patch markers + file headers.
    if (line.startsWith("*** ")) return /^\*\*\* (Add|Update|Delete|Move) /.test(line) ? "file" : "marker";
    // Unified diff (OpenCode edit): Index:/=== are noise, ---/+++ are file headers (NOT -/+ content).
    if (line.startsWith("Index: ")) return "file";
    if (line.startsWith("====")) return "marker";
    if (line.startsWith("--- ") || line.startsWith("+++ ")) return "file";
    if (line.startsWith("@@")) return "hunk";
    if (line.startsWith("+")) return "add";
    if (line.startsWith("-")) return "del";
    return "ctx";
  }
</script>

<div class="space-y-2">
  <div class="rounded-md border border-border-subtle overflow-hidden max-h-96 overflow-y-auto bg-bg">
    {#each lines as line}
      {@const k = kind(line)}
      {#if k === "marker"}
        <!-- *** Begin Patch / *** End Patch — structural noise, hidden -->
      {:else if k === "file"}
        <div class="px-3 py-1 text-[10px] font-mono font-semibold text-accent bg-bg-tertiary border-y border-border-subtle">
          {line.replace(/^\*\*\* /, "")}
        </div>
      {:else if k === "hunk"}
        <div class="px-3 text-[10px] font-mono text-text-muted">{line}</div>
      {:else}
        <div class="px-3 font-mono text-[10px] leading-5
          {k === 'add' ? 'bg-success-dim text-success' : k === 'del' ? 'bg-danger-dim text-danger' : 'text-text-muted'}">
          <pre class="whitespace-pre-wrap break-words">{line || " "}</pre>
        </div>
      {/if}
    {/each}
  </div>
  {#if resultText}
    <div class="text-[10px] text-text-muted px-1 whitespace-pre-wrap break-words">{resultText}</div>
  {/if}
</div>
