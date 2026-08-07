<script lang="ts">
  import type { SessionRecord, TerminalInput } from "$lib/types";
  import { t } from "$lib/i18n.svelte";

  let {
    record,
    result = null,
  }: { record: SessionRecord; result?: SessionRecord | null; highlight?: string } = $props();

  let input = $derived((record.tool_input ?? {}) as TerminalInput);
  let command = $derived(input.command ?? "");
  let description = $derived(input.description ?? "");
  let meta = $derived(result?.result_meta?.terminal ?? null);
  let stdout = $derived(result?.content_preview ?? "");
  let stderr = $derived(meta?.stderr ?? "");
  let interrupted = $derived(meta?.interrupted === true);
  let bgTaskId = $derived(meta?.background_task_id ?? null);
  let exitNote = $derived(meta?.exit_note ?? "");
  let exitCode = $derived(meta?.exit_code ?? null);
  let durationMs = $derived(meta?.duration_ms ?? null);
  const fmtMs = (ms: number) => (ms >= 1000 ? (ms / 1000).toFixed(1) + " s" : ms + " ms");
  // stderr text may already be folded into stdout; only show it separately if distinct.
  let showStderr = $derived(!!stderr && !stdout.includes(stderr));
</script>

<div class="space-y-1.5">
  <!-- Command line -->
  {#if command}
    <div class="rounded-md bg-[#1e1e2e] border border-border-subtle px-3 py-1.5 overflow-x-auto">
      <pre class="text-[11px] font-mono text-green-300 whitespace-pre-wrap break-all leading-snug"><span class="text-text-muted select-none">$ </span>{command}</pre>
    </div>
  {/if}
  {#if description}
    <div class="text-[10px] text-text-muted px-1">{description}</div>
  {/if}

  <!-- Badges -->
  {#if interrupted || bgTaskId || exitNote || exitCode != null || durationMs != null}
    <div class="flex items-center gap-1.5 flex-wrap px-1">
      {#if exitCode != null}<span class="text-[9px] px-1.5 py-0.5 rounded {exitCode === 0 ? 'bg-success-dim text-success' : 'bg-danger-dim text-danger'}">exit {exitCode}</span>{/if}
      {#if durationMs != null}<span class="text-[9px] px-1.5 py-0.5 rounded bg-bg-tertiary text-text-muted">⏱ {fmtMs(durationMs)}</span>{/if}
      {#if interrupted}<span class="text-[9px] px-1.5 py-0.5 rounded bg-warning-dim text-warning">{t("tool.interrupted")}</span>{/if}
      {#if bgTaskId}<span class="text-[9px] px-1.5 py-0.5 rounded bg-accent-dim text-accent">{t("tool.bgTask")}</span>{/if}
      {#if exitNote}<span class="text-[9px] px-1.5 py-0.5 rounded bg-bg-tertiary text-text-muted">{exitNote}</span>{/if}
    </div>
  {/if}

  <!-- Output -->
  {#if stdout}
    <div class="rounded-md bg-[#1e1e2e] border border-border-subtle px-3 py-2 max-h-96 overflow-auto">
      <pre class="text-[11px] font-mono text-gray-200 whitespace-pre-wrap break-words leading-snug">{stdout}</pre>
    </div>
  {/if}
  {#if showStderr}
    <div class="rounded-md bg-danger-dim border border-danger/30 px-3 py-2 max-h-60 overflow-auto">
      <pre class="text-[11px] font-mono text-danger whitespace-pre-wrap break-words leading-snug">{stderr}</pre>
    </div>
  {/if}
  {#if !stdout && !showStderr && result}
    <div class="text-[10px] text-text-muted italic px-1">{t("tool.noOutput")}</div>
  {/if}
</div>
