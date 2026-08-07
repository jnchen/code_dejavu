<script lang="ts">
  import type { SessionRecord, AskQuestion } from "$lib/types";
  import { t } from "$lib/i18n.svelte";

  let {
    record,
    result = null,
  }: { record: SessionRecord; result?: SessionRecord | null; highlight?: string } = $props();

  let questions = $derived<AskQuestion[]>(
    Array.isArray(record.tool_input?.questions) ? (record.tool_input!.questions as AskQuestion[]) : []
  );
  let answers = $derived(result?.result_meta?.answers ?? null);

  function answerFor(q: AskQuestion): string[] {
    const a = answers?.[q.question];
    if (a == null) return [];
    return Array.isArray(a) ? a : [a];
  }

  function isSelected(q: AskQuestion, label: string): boolean {
    return answerFor(q).some((a) => a === label || a.split(/,\s*/).includes(label));
  }

  // Free-text answers ("Other") that don't correspond to any predefined option.
  function customAnswers(q: AskQuestion): string[] {
    const labels = new Set(q.options.map((o) => o.label));
    const out: string[] = [];
    for (const a of answerFor(q)) {
      if (labels.has(a)) continue;
      const toks = a.split(/,\s*/);
      if (toks.length > 1 && toks.every((tok) => labels.has(tok))) continue;
      out.push(a);
    }
    return out;
  }

  let answered = $derived(answers != null && Object.keys(answers).length > 0);
</script>

<div class="space-y-2.5">
  {#each questions as q}
    {@const custom = customAnswers(q)}
    <div class="rounded-lg border border-border-subtle bg-bg-secondary overflow-hidden">
      <!-- Question header -->
      <div class="px-3 py-2 border-b border-border-subtle bg-bg-tertiary/40">
        <div class="flex items-center gap-2 flex-wrap">
          {#if q.header}
            <span class="text-[9px] font-semibold uppercase tracking-wide px-1.5 py-0.5 rounded bg-accent-dim text-accent shrink-0">{q.header}</span>
          {/if}
          {#if q.multiSelect}
            <span class="text-[9px] px-1.5 py-0.5 rounded bg-bg-tertiary text-text-muted shrink-0">{t("tool.multiSelect")}</span>
          {/if}
          {#if !answered}
            <span class="text-[9px] px-1.5 py-0.5 rounded bg-warning-dim text-warning shrink-0">{t("tool.unanswered")}</span>
          {/if}
        </div>
        <div class="text-[12px] text-text mt-1 leading-snug">{q.question}</div>
      </div>

      <!-- Options -->
      <div class="p-1.5 space-y-1">
        {#each q.options as opt}
          {@const sel = isSelected(q, opt.label)}
          <div class="flex items-start gap-2 px-2 py-1.5 rounded-md transition-colors
            {sel ? 'bg-success-dim border border-success/40' : 'border border-transparent opacity-60'}">
            <span class="mt-0.5 w-3.5 h-3.5 rounded-full shrink-0 flex items-center justify-center text-[9px] font-bold
              {sel ? 'bg-success text-white' : 'border border-border'}">
              {sel ? '✓' : ''}
            </span>
            <div class="min-w-0">
              <div class="text-[11px] font-medium {sel ? 'text-success' : 'text-text-secondary'}">{opt.label}</div>
              {#if opt.description}
                <div class="text-[10px] text-text-muted leading-snug">{opt.description}</div>
              {/if}
            </div>
          </div>
        {/each}

        <!-- Free-text / "Other" answers -->
        {#each custom as c}
          <div class="flex items-start gap-2 px-2 py-1.5 rounded-md bg-success-dim border border-success/40">
            <span class="mt-0.5 w-3.5 h-3.5 rounded-full shrink-0 flex items-center justify-center text-[9px] font-bold bg-success text-white">✓</span>
            <div class="min-w-0">
              <div class="text-[9px] text-text-muted">{t("tool.customAnswer")}</div>
              <div class="text-[11px] font-medium text-success whitespace-pre-wrap break-words">{c}</div>
            </div>
          </div>
        {/each}
      </div>
    </div>
  {/each}
</div>
