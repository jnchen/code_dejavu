<script lang="ts">
  import type { SessionRecord, WebFetchInput } from "$lib/types";
  import Markdown from "$lib/Markdown.svelte";
  import { api } from "$lib/api";
  import { t } from "$lib/i18n.svelte";
  import { pushToast } from "$lib/toast.svelte";

  let {
    record,
    result = null,
    highlight = "",
  }: {
    record: SessionRecord;
    result?: SessionRecord | null;
    highlight?: string;
  } = $props();

  // Extract every nullable field into a plain guarded local — never chain into a
  // possibly-null object inside the template (avoids HMR null-read tearing).
  let input = $derived((record.tool_input ?? {}) as WebFetchInput);
  let meta = $derived(result?.result_meta?.webfetch ?? null);
  let url = $derived(input.url ?? meta?.url ?? "");
  let prompt = $derived(input.prompt ?? "");
  let code = $derived(meta?.code ?? null);
  let codeText = $derived(meta?.code_text ?? "");
  let bytes = $derived(meta?.bytes ?? null);
  let durationMs = $derived(meta?.duration_ms ?? null);
  let resultText = $derived(result?.content_preview ?? "");
  let ok = $derived(code == null || (code >= 200 && code < 400));

  function fmtBytes(b: number): string {
    if (b >= 1048576) return (b / 1048576).toFixed(1) + " MB";
    if (b >= 1024) return (b / 1024).toFixed(1) + " KB";
    return b + " B";
  }
  function fmtDuration(ms: number): string {
    return ms >= 1000 ? (ms / 1000).toFixed(1) + " s" : ms + " ms";
  }
</script>

<div class="space-y-2">
  <!-- URL + HTTP status -->
  <div class="flex items-start gap-2">
    {#if code != null}
      <span class="mt-0.5 text-[9px] font-semibold px-1.5 py-0.5 rounded shrink-0
        {ok ? 'bg-success-dim text-success' : 'bg-danger-dim text-danger'}">
        {code}{codeText ? ' ' + codeText : ''}
      </span>
    {/if}
    {#if url}
      <button class="text-[11px] font-mono text-accent break-all leading-snug text-left hover:underline cursor-pointer"
        title={t("tool.openInBrowser") + ": " + url}
        onclick={() => /^https?:\/\//i.test(url) && api.shell.openExternal(url).catch(() => pushToast(t("toast.openLinkFailed")))}>{url}</button>
    {/if}
  </div>

  <!-- The prompt sent to the fetcher -->
  {#if prompt}
    <div class="rounded-md bg-bg-tertiary/40 border border-border-subtle px-2.5 py-1.5">
      <div class="text-[9px] font-semibold uppercase tracking-wide text-text-muted mb-0.5">{t("tool.extractPrompt")}</div>
      <div class="text-[11px] text-text-secondary whitespace-pre-wrap break-words leading-snug">{prompt}</div>
    </div>
  {/if}

  <!-- The fetched / processed result -->
  {#if resultText}
    <div class="rounded-md bg-bg-secondary border border-border-subtle px-3 py-2 max-h-96 overflow-y-auto">
      <Markdown content={resultText} {highlight} />
    </div>
  {:else if result}
    <div class="text-[10px] text-text-muted italic px-1">{t("tool.noTextResult")}</div>
  {/if}

  <!-- Size / timing footer -->
  {#if bytes != null || durationMs != null}
    <div class="flex items-center gap-3 text-[10px] text-text-muted px-1">
      {#if bytes != null}<span>📦 {fmtBytes(bytes)}</span>{/if}
      {#if durationMs != null}<span>⏱ {fmtDuration(durationMs)}</span>{/if}
    </div>
  {/if}
</div>
