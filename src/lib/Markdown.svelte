<script lang="ts">
  import { marked } from "marked";
  import DOMPurify from "dompurify";
  import { api } from "$lib/api";
  import { t } from "$lib/i18n.svelte";
  import { pushToast } from "$lib/toast.svelte";

  let {
    content = "",
    highlight = "",
    onLocalLink,
  }: {
    content: string;
    highlight?: string;
    /** Called for relative/local links (e.g. another .md file). If absent, they do nothing. */
    onLocalLink?: (href: string) => void;
  } = $props();

  // Links inside rendered markdown must NEVER navigate the webview (that unloads the SPA).
  // http(s) → system browser; relative/local (e.g. a sibling .md) → parent-supplied handler.
  // Also handles the injected per-code-block copy button.
  function handleClick(e: MouseEvent) {
    const target = e.target as HTMLElement | null;
    const copyBtn = target?.closest(".copy-code-btn");
    if (copyBtn) {
      e.preventDefault();
      const code = copyBtn.parentElement?.querySelector("pre")?.textContent ?? "";
      navigator.clipboard
        .writeText(code)
        .then(() => {
          copyBtn.textContent = t("tool.copied");
          setTimeout(() => {
            copyBtn.textContent = t("tool.copyCode");
          }, 1500);
        })
        .catch(() => pushToast(t("toast.copyFailed")));
      return;
    }
    const anchor = target?.closest("a");
    if (!anchor) return;
    const href = anchor.getAttribute("href");
    e.preventDefault();
    if (!href) return;
    if (/^https?:\/\//i.test(href)) {
      api.shell.openExternal(href).catch(() => pushToast(t("toast.openLinkFailed")));
    } else if (!href.startsWith("#")) {
      onLocalLink?.(href);
    }
  }

  marked.setOptions({
    breaks: true,
    gfm: true,
  });

  let html = $derived.by(() => {
    // Transcript content can include attacker-controlled web pages / tool output, so it MUST be
    // treated as hostile. Render markdown, then hard-sanitize with DOMPurify's allowlist BEFORE it
    // ever reaches {@html}. This strips <script>/<iframe>/<object>/<embed>, inline on* handlers and
    // javascript:/data: URLs that the previous hand-rolled normalizer could not reliably block.
    // The copy-button / highlight rewrites below only inject fixed, trusted markup on top of the
    // already-sanitized string, so they can't reintroduce script.
    let h = DOMPurify.sanitize(marked.parse(content) as string, {
      USE_PROFILES: { html: true },
      FORBID_TAGS: ["style", "form", "input", "textarea", "iframe", "object", "embed"],
      FORBID_ATTR: ["style"],
    });
    if (highlight && highlight.length >= 2) {
      const escaped = highlight.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
      const re = new RegExp(`(${escaped})`, 'gi');
      h = h.replace(/>([^<]+)</g, (_m: string, t: string) =>
        '>' + t.replace(re, '<mark class="bg-warning/40 text-inherit rounded px-0.5">$1</mark>') + '<'
      );
    }
    // Wrap each code block so it can carry a hover copy button (handled in handleClick).
    h = h
      .replace(
        /<pre(\b[^>]*)?>/g,
        `<div class="code-block"><button class="copy-code-btn" type="button">${t("tool.copyCode")}</button><pre$1>`
      )
      .replace(/<\/pre>/g, "</pre></div>");
    return h;
  });
</script>

<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="markdown-body" onclick={handleClick}>
  {@html html}
</div>

<style>
  .markdown-body {
    font-size: 12px;
    line-height: 1.6;
    word-wrap: break-word;
    overflow-wrap: break-word;
  }
  .markdown-body :global(h1),
  .markdown-body :global(h2),
  .markdown-body :global(h3) {
    font-weight: 600;
    margin: 0.6em 0 0.3em;
    line-height: 1.3;
  }
  .markdown-body :global(h1) { font-size: 1.2em; }
  .markdown-body :global(h2) { font-size: 1.1em; }
  .markdown-body :global(h3) { font-size: 1em; }
  .markdown-body :global(p) { margin: 0.3em 0; }
  .markdown-body :global(ul),
  .markdown-body :global(ol) {
    padding-left: 1.5em;
    margin: 0.3em 0;
  }
  .markdown-body :global(li) { margin: 0.1em 0; }
  .markdown-body :global(code) {
    font-family: var(--font-mono, monospace);
    font-size: 0.9em;
    background: var(--color-bg-tertiary);
    padding: 0.15em 0.4em;
    border-radius: 4px;
  }
  .markdown-body :global(pre) {
    background: var(--color-bg-tertiary);
    border: 1px solid var(--color-border);
    border-radius: 8px;
    padding: 0.6em 0.8em;
    overflow-x: auto;
    margin: 0.4em 0;
    font-size: 0.9em;
  }
  .markdown-body :global(.code-block) {
    position: relative;
  }
  .markdown-body :global(.copy-code-btn) {
    position: absolute;
    top: 8px;
    right: 8px;
    font-size: 10px;
    padding: 2px 7px;
    border-radius: 5px;
    background: var(--color-bg);
    border: 1px solid var(--color-border);
    color: var(--color-text-muted);
    cursor: pointer;
    opacity: 0;
    transition: opacity 0.15s, color 0.15s, background 0.15s;
  }
  .markdown-body :global(.code-block:hover .copy-code-btn) {
    opacity: 1;
  }
  .markdown-body :global(.copy-code-btn:hover) {
    background: var(--color-bg-hover);
    color: var(--color-text);
  }
  .markdown-body :global(pre code) {
    background: none;
    padding: 0;
    border-radius: 0;
  }
  .markdown-body :global(blockquote) {
    border-left: 3px solid var(--color-border);
    padding-left: 0.8em;
    margin: 0.4em 0;
    color: var(--color-text-secondary);
  }
  .markdown-body :global(table) {
    border-collapse: collapse;
    margin: 0.4em 0;
    font-size: 0.9em;
    width: 100%;
  }
  .markdown-body :global(th),
  .markdown-body :global(td) {
    border: 1px solid var(--color-border);
    padding: 0.3em 0.6em;
    text-align: left;
  }
  .markdown-body :global(th) {
    background: var(--color-bg-tertiary);
    font-weight: 600;
  }
  .markdown-body :global(strong) { font-weight: 600; }
  .markdown-body :global(a) {
    color: var(--color-accent);
    text-decoration: none;
  }
  .markdown-body :global(a:hover) { text-decoration: underline; }
  .markdown-body :global(hr) {
    border: none;
    border-top: 1px solid var(--color-border);
    margin: 0.6em 0;
  }
  .markdown-body :global(img) {
    max-width: 100%;
    border-radius: 6px;
  }
</style>
