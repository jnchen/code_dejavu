<script lang="ts">
  import type { SessionRecord } from "$lib/types";

  let {
    record,
    result = null,
    expanded = false,
  }: {
    record: SessionRecord;
    result?: SessionRecord | null;
    expanded?: boolean;
    highlight?: string;
  } = $props();

  let diff = $derived(result?.diff ?? null);
  let resultText = $derived(result?.content_preview ?? "");
  let showParams = $derived(!!record.tool_input && expanded);
  let hasAny = $derived(showParams || !!diff || !!resultText);

  function formatToolInput(input: Record<string, unknown>): Array<{ key: string; val: string; long: boolean }> {
    return Object.entries(input).map(([key, val]) => {
      const s = typeof val === "string" ? val : JSON.stringify(val, null, 2);
      return { key, val: s, long: s.length > 80 };
    });
  }
</script>

{#if hasAny}
    <!-- Params -->
    {#if showParams && record.tool_input}
      <div class="px-3 py-2 space-y-1.5 bg-bg-tertiary/30 {(diff || resultText) ? 'border-b border-border-subtle' : ''}">
        {#each formatToolInput(record.tool_input) as param}
          <div class="flex gap-2 {param.long ? 'flex-col' : 'items-start'}">
            <span class="text-[10px] font-semibold text-accent shrink-0 min-w-[60px]">{param.key}</span>
            {#if param.long}
              <pre class="text-[10px] font-mono text-text-secondary whitespace-pre-wrap break-words bg-bg-secondary rounded px-2 py-1.5 max-h-60 overflow-y-auto border border-border/30">{param.val}</pre>
            {:else}
              <span class="text-[10px] font-mono text-text-secondary break-all">{param.val}</span>
            {/if}
          </div>
        {/each}
      </div>
    {/if}

    <!-- Diff result -->
    {#if diff}
      <div class="max-h-72 overflow-y-auto bg-bg">
        {#each diff.hunks as hunk}
          <div class="px-2 py-0.5 text-[10px] text-text-muted bg-bg-tertiary border-b border-border-subtle font-mono">
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
    {/if}

    <!-- Text result -->
    {#if resultText && !diff}
      <div class="px-3 py-1.5 text-[10px] font-mono text-text-secondary bg-bg-secondary max-h-72 overflow-y-auto">
        <pre class="whitespace-pre-wrap break-words">{resultText}</pre>
      </div>
    {/if}
{/if}
