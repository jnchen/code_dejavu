<script lang="ts">
  import { t } from "$lib/i18n.svelte";
  // Reusable confirmation modal for destructive / irreversible actions (snapshot restore,
  // snapshot delete, memory delete). It exists to make the consequence explicit before the
  // backend touches real files under ~/.claude — the inline "是/否" was too easy to miss.
  interface Props {
    open: boolean;
    title: string;
    /** Body text; "\n" renders as line breaks. */
    message: string;
    /** Optional emphasised target (file path / snapshot name) shown in a monospace box. */
    detail?: string | null;
    confirmLabel?: string;
    cancelLabel?: string;
    /** "danger" → destructive red, "warning" → amber. */
    tone?: "danger" | "warning";
    busy?: boolean;
    onconfirm: () => void;
    oncancel: () => void;
  }

  let {
    open,
    title,
    message,
    detail = null,
    confirmLabel,
    cancelLabel,
    tone = "danger",
    busy = false,
    onconfirm,
    oncancel,
  }: Props = $props();

  function onKeydown(event: KeyboardEvent) {
    if (!open || busy) return;
    if (event.key === "Escape") {
      event.preventDefault();
      oncancel();
    } else if (event.key === "Enter") {
      event.preventDefault();
      onconfirm();
    }
  }
</script>

<svelte:window onkeydown={onKeydown} />

{#if open}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
    onclick={(event) => { if (event.target === event.currentTarget && !busy) oncancel(); }}
  >
    <div
      class="w-full max-w-md rounded-xl border border-border bg-bg-secondary p-5 shadow-2xl"
      role="dialog"
      aria-modal="true"
      aria-label={title}
    >
      <h3 class="text-sm font-semibold {tone === 'danger' ? 'text-danger' : 'text-warning'}">{title}</h3>
      <p class="mt-2 whitespace-pre-line text-xs leading-relaxed text-text-secondary">{message}</p>
      {#if detail}
        <div class="mt-3 break-all rounded-lg border border-border bg-bg px-3 py-2 font-mono text-[11px] text-text-muted">
          {detail}
        </div>
      {/if}
      <div class="mt-5 flex justify-end gap-2">
        <button
          onclick={oncancel}
          disabled={busy}
          class="rounded-lg border border-border px-4 py-1.5 text-xs hover:bg-bg-hover transition-colors disabled:opacity-50"
        >
          {cancelLabel ?? t("common.cancel")}
        </button>
        <button
          onclick={onconfirm}
          disabled={busy}
          class="rounded-lg px-4 py-1.5 text-xs font-medium text-white transition-all hover:opacity-90 disabled:opacity-50
            {tone === 'danger' ? 'bg-danger' : 'bg-warning'}"
        >
          {busy ? t("common.processing") : (confirmLabel ?? t("common.confirm"))}
        </button>
      </div>
    </div>
  </div>
{/if}
