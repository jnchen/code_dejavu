<script lang="ts">
  import "../app.css";
  import { onDestroy, onMount } from "svelte";
  import { goto } from "$app/navigation";
  import { page } from "$app/state";
  import { t, toggleLang, initLang } from "$lib/i18n.svelte";
  import { toasts, dismissToast } from "$lib/toast.svelte";
  import packageInfo from "../../package.json";

  let theme = $state<"light" | "dark">("light");
  function applyTheme(mode: "light" | "dark") {
    document.documentElement.classList.toggle("dark", mode === "dark");
  }
  function systemPrefersDark(): boolean {
    return typeof window !== "undefined" && !!window.matchMedia
      ? window.matchMedia("(prefers-color-scheme: dark)").matches
      : false;
  }
  function savedTheme(): string | null {
    try {
      return localStorage.getItem("dejavu-theme");
    } catch {
      return null;
    }
  }
  function toggleTheme() {
    theme = theme === "dark" ? "light" : "dark";
    try {
      localStorage.setItem("dejavu-theme", theme);
    } catch {}
    applyTheme(theme);
  }
  onMount(() => {
    initLang();
    // No saved choice → follow the OS. Mirrors the pre-paint script in app.html, so the reactive
    // state matches what was already painted (no flash, correct toggle icon).
    const saved = savedTheme();
    theme = saved ? (saved === "dark" ? "dark" : "light") : systemPrefersDark() ? "dark" : "light";
    applyTheme(theme);

    // Keep following the OS while the user hasn't made an explicit choice.
    const mq = window.matchMedia?.("(prefers-color-scheme: dark)");
    const onSystemChange = (e: MediaQueryListEvent) => {
      if (savedTheme()) return; // an explicit user choice always wins
      theme = e.matches ? "dark" : "light";
      applyTheme(theme);
    };
    mq?.addEventListener?.("change", onSystemChange);
    return () => mq?.removeEventListener?.("change", onSystemChange);
  });

  const nav = [
    { href: "/", key: "nav.dashboard", icon: "◎" },
    { href: "/sessions", key: "nav.sessions", icon: "▤" },
    { href: "/workflows", key: "nav.workflows", icon: "❖" },
    { href: "/memories", key: "nav.memories", icon: "◈" },
    { href: "/rules", key: "nav.rules", icon: "⚙" },
    { href: "/tools", key: "nav.tools", icon: "⚒" },
    { href: "/usage", key: "nav.usage", icon: "▦" },
    { href: "/profiles", key: "nav.profiles", icon: "⟲" },
    { href: "/instructions", key: "nav.instructions", icon: "✎" },
    { href: "/settings", key: "nav.settings", icon: "⊞" },
  ];

  let { children } = $props();

  function isActive(href: string): boolean {
    if (href === "/") return page.url.pathname === "/";
    return page.url.pathname.startsWith(href);
  }

  // Native anchors are ideal for ordinary navigation, but a burst of sidebar clicks can still
  // create and destroy many route components before any page becomes visible. Coalesce only plain
  // primary-button clicks; modifier/middle clicks retain normal browser behavior.
  let pendingNavigation: string | null = null;
  let navigationTimer: ReturnType<typeof setTimeout> | null = null;
  function navigateSidebar(event: MouseEvent, href: string) {
    if (
      event.button !== 0 ||
      event.metaKey ||
      event.ctrlKey ||
      event.shiftKey ||
      event.altKey
    ) {
      return;
    }
    event.preventDefault();
    pendingNavigation = href;
    if (navigationTimer) clearTimeout(navigationTimer);
    navigationTimer = setTimeout(() => {
      navigationTimer = null;
      const target = pendingNavigation;
      pendingNavigation = null;
      if (target && target !== page.url.pathname) void goto(target);
    }, 32);
  }
  onDestroy(() => {
    if (navigationTimer) clearTimeout(navigationTimer);
  });
</script>

<div class="flex h-dvh w-screen bg-bg">
  <!-- Sidebar -->
  <aside class="w-[200px] shrink-0 border-r border-border bg-bg-secondary flex flex-col">
    <!-- Nav -->
    <nav class="flex-1 px-2 pt-3 space-y-0.5">
      {#each nav as item}
        {@const active = isActive(item.href)}
        <a
          href={item.href}
          onclick={(event) => navigateSidebar(event, item.href)}
          class="flex items-center gap-2.5 px-3 py-[7px] rounded-lg text-[13px] transition-all duration-150
            {active
              ? 'bg-accent-dim text-accent font-medium'
              : 'text-text-secondary hover:bg-bg-hover hover:text-text'}"
        >
          <span class="text-[15px] w-5 text-center opacity-60">{item.icon}</span>
          {t(item.key)}
        </a>
      {/each}
    </nav>

    <!-- Footer -->
    <div class="px-4 py-4 border-t border-border">
      <button
        onclick={toggleTheme}
        title={t("footer.themeTitle")}
        class="flex w-full items-center gap-2 rounded-lg px-2 py-1.5 text-[11px] text-text-secondary transition-colors hover:bg-bg-hover"
      >
        <span class="text-[13px] w-5 text-center opacity-70">{theme === "dark" ? "☀" : "☾"}</span>
        {theme === "dark" ? t("footer.light") : t("footer.dark")}
      </button>
      <button
        onclick={toggleLang}
        title={t("footer.langTitle")}
        class="mb-2 flex w-full items-center gap-2 rounded-lg px-2 py-1.5 text-[11px] text-text-secondary transition-colors hover:bg-bg-hover"
      >
        <span class="text-[13px] w-5 text-center opacity-70">🌐</span>
        {t("footer.lang")}
      </button>
      <div class="text-[10px] text-text-muted">{t("footer.tagline")}</div>
      <div class="mt-1 text-[10px] text-text-muted font-mono">v{packageInfo.version}</div>
    </div>
  </aside>

  <!-- Main -->
  <main class="flex-1 overflow-y-auto bg-bg">
    {@render children()}
  </main>
</div>

<!-- Global non-fatal notifications -->
{#if toasts().length > 0}
  <div class="fixed bottom-4 right-4 z-50 flex max-w-sm flex-col gap-2">
    {#each toasts() as toast (toast.id)}
      <button
        onclick={() => dismissToast(toast.id)}
        title={t("common.dismiss")}
        class="rounded-lg border px-3 py-2 text-left text-xs shadow-lg transition-colors
          {toast.kind === 'error'
            ? 'border-danger/30 bg-danger-dim text-danger hover:bg-danger/20'
            : 'border-accent/30 bg-accent-dim text-accent hover:bg-accent/20'}"
      >
        {toast.message}
      </button>
    {/each}
  </div>
{/if}
