<script lang="ts">
  import { onMount } from "svelte";
  import { goto } from "$app/navigation";
  import { t } from "$lib/i18n.svelte";

  onMount(() => {
    goto("/instructions", { replaceState: true });
  });
</script>

<div class="p-6 text-sm text-text-secondary">{t("inst.opening")}</div>
