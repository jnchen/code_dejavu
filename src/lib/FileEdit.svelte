<script lang="ts">
  import type { SessionRecord } from "$lib/types";

  let {
    record,
    result = null,
  }: { record: SessionRecord; result?: SessionRecord | null; highlight?: string } = $props();

  // The diff lives on the result record (structuredPatch parsed by the backend).
  let diff = $derived(result?.diff ?? record.diff ?? null);
  let resultText = $derived(result?.content_preview ?? "");
  let filePath = $derived(
    (record.tool_input?.file_path as string | undefined) ?? diff?.file_path ?? ""
  );
  let shortPath = $derived(filePath ? filePath.split(/[/\\]/).slice(-3).join("/") : "");
</script>

<div class="space-y-1.5">
  {#if shortPath}
    <div class="text-[10px] font-mono text-text-muted px-1 break-all">{shortPath}</div>
  {/if}

  {#if diff}
    <div class="rounded-md border border-border-subtle overflow-hidden max-h-96 overflow-y-auto bg-bg">
      {#each diff.hunks as hunk}
        <div class="px-2 py-0.5 text-[10px] text-text-muted bg-bg-tertiary border-b border-border-subtle font-mono sticky top-0">
          @@ -{hunk.old_start},{hunk.old_lines} +{hunk.new_start},{hunk.new_lines} @@
        </div>
        {#each hunk.lines as line}
          <div class="px-3 font-mono text-[10px] leading-5
            {line.startsWith('+') ? 'bg-success-dim text-success' :
             line.startsWith('-') ? 'bg-danger-dim text-danger' : 'text-text-muted'}">
            <pre class="whitespace-pre-wrap break-words">{line}</pre>
          </div>
        {/each}
      {/each}
    </div>
  {:else if resultText}
    <div class="rounded-md bg-bg-secondary border border-border-subtle px-3 py-2 max-h-96 overflow-y-auto">
      <pre class="text-[11px] font-mono text-text-secondary whitespace-pre-wrap break-words leading-snug">{resultText}</pre>
    </div>
  {/if}
</div>
