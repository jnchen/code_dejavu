<script lang="ts">
  import type { SessionRecord, TodoWriteInput, TodoItem } from "$lib/types";
  import { t } from "$lib/i18n.svelte";

  let {
    record,
  }: { record: SessionRecord; result?: SessionRecord | null; highlight?: string } = $props();

  let input = $derived((record.tool_input ?? {}) as TodoWriteInput);
  let todos = $derived<TodoItem[]>(Array.isArray(input.todos) ? input.todos : []);

  function mark(status: string): { icon: string; cls: string } {
    if (status === "completed") return { icon: "✓", cls: "text-success" };
    if (status === "in_progress") return { icon: "▸", cls: "text-accent" };
    return { icon: "○", cls: "text-text-muted" };
  }
</script>

<div class="space-y-1">
  {#if todos.length > 0}
    {#each todos as todo}
      {@const m = mark(todo.status)}
      <div class="flex items-start gap-2 px-1">
        <span class="text-[11px] font-bold shrink-0 mt-px {m.cls}">{m.icon}</span>
        <span class="text-[11px] leading-snug
          {todo.status === 'completed' ? 'text-text-muted line-through' :
           todo.status === 'in_progress' ? 'text-text font-medium' : 'text-text-secondary'}">
          {todo.status === 'in_progress' && todo.activeForm ? todo.activeForm : todo.content}
        </span>
      </div>
    {/each}
  {:else}
    <div class="text-[10px] text-text-muted italic px-1">{t("tool.emptyList")}</div>
  {/if}
</div>
