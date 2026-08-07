/// <reference lib="webworker" />

type ExportRecord = {
  record_type: string;
  content_preview: string;
  timestamp?: string | null;
  tool_name?: string | null;
  tool_input?: unknown;
};

type ExportSession = {
  source?: string | null;
  session_id: string;
  project_path: string;
  agent_title?: string | null;
  created_at?: string | null;
  updated_at?: string | null;
  timestamp?: string | null;
  first_prompt?: string | null;
};

type ExportMessage = {
  format: "md" | "json" | "html";
  session: ExportSession;
  records: ExportRecord[];
  sourceLabel: string;
  labels: {
    source: string;
    project: string;
    time: string;
    agentTitle: string;
    created: string;
    updated: string;
  };
};

function escapeHtml(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function toMarkdown(message: ExportMessage): string {
  const { session: s, records, labels } = message;
  const out: string[] = [];
  out.push(`# ${s.agent_title || s.first_prompt || s.project_path || s.session_id}`, "");
  out.push(`- ${labels.source}: ${message.sourceLabel}`);
  out.push(`- ${labels.project}: ${s.project_path}`);
  if (s.agent_title) out.push(`- ${labels.agentTitle}: ${s.agent_title}`);
  if (s.created_at) out.push(`- ${labels.created}: ${s.created_at}`);
  if (s.updated_at ?? s.timestamp) out.push(`- ${labels.updated}: ${s.updated_at ?? s.timestamp}`);
  out.push("");
  for (const r of records) {
    if (r.record_type === "user") {
      out.push("## 🧑 User", "", r.content_preview, "");
    } else if (r.record_type === "assistant") {
      if (r.tool_name) {
        out.push(`### 🔧 ${r.tool_name}`, "", "```json", JSON.stringify(r.tool_input ?? {}, null, 2), "```", "");
      } else if (r.content_preview) {
        out.push("## 🤖 Assistant", "", r.content_preview, "");
      }
    } else if (r.record_type === "thinking") {
      out.push("> 💭 " + r.content_preview.replace(/\n/g, "\n> "), "");
    } else if (r.record_type === "tool_result") {
      out.push("```", r.content_preview, "```", "");
    } else if (r.content_preview) {
      out.push(`_${r.record_type}_: ${r.content_preview}`, "");
    }
  }
  return out.join("\n");
}

function toJson(message: ExportMessage): string {
  const s = message.session;
  return JSON.stringify({
    source: s.source ?? null,
    session_id: s.session_id,
    project_path: s.project_path,
    agent_title: s.agent_title ?? null,
    created_at: s.created_at ?? null,
    updated_at: s.updated_at ?? s.timestamp ?? null,
    timestamp: s.timestamp ?? null,
    first_prompt: s.first_prompt ?? null,
    records: message.records,
  }, null, 2);
}

function toHtml(message: ExportMessage): string {
  const s = message.session;
  const title = s.agent_title || s.first_prompt || s.project_path || s.session_id;
  const rows = message.records.map((r) => {
    const ts = r.timestamp ? `<div class="ts">${escapeHtml(r.timestamp)}</div>` : "";
    if (r.record_type === "user")
      return `<div class="msg user"><div class="role">🧑 User</div>${ts}<pre>${escapeHtml(r.content_preview)}</pre></div>`;
    if (r.record_type === "assistant" && r.tool_name)
      return `<div class="msg tool"><div class="role">🔧 ${escapeHtml(r.tool_name)}</div><pre>${escapeHtml(JSON.stringify(r.tool_input ?? {}, null, 2))}</pre></div>`;
    if (r.record_type === "assistant")
      return `<div class="msg assistant"><div class="role">🤖 Assistant</div>${ts}<pre>${escapeHtml(r.content_preview)}</pre></div>`;
    if (r.record_type === "thinking")
      return `<div class="msg think"><div class="role">💭 Thinking</div><pre>${escapeHtml(r.content_preview)}</pre></div>`;
    if (r.record_type === "tool_result")
      return `<div class="msg result"><div class="role">↳ Result</div><pre>${escapeHtml(r.content_preview)}</pre></div>`;
    if (r.content_preview)
      return `<div class="msg other"><div class="role">${escapeHtml(r.record_type)}</div><pre>${escapeHtml(r.content_preview)}</pre></div>`;
    return "";
  }).join("\n");
  return `<!doctype html><html lang="zh"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>${escapeHtml(title)}</title>
<style>
body{font-family:system-ui,-apple-system,"Microsoft YaHei",sans-serif;max-width:860px;margin:24px auto;padding:0 16px;color:#212529;background:#fff;line-height:1.6}
h1{font-size:18px}.meta{color:#868e96;font-size:12px;margin-bottom:20px}
.msg{margin:14px 0;padding:10px 14px;border-radius:10px;border:1px solid #dee2e6}
.user{background:#eef0fb}.assistant{background:#f8f9fa}.tool,.result{background:#f1f3f5}.think{background:#fff8e1;color:#495057}
.role{font-weight:600;font-size:12px;margin-bottom:4px}.ts{color:#adb5bd;font-size:11px;margin-bottom:4px}
pre{white-space:pre-wrap;word-break:break-word;margin:0;font-family:ui-monospace,monospace;font-size:12px}
</style></head><body>
<h1>${escapeHtml(title)}</h1>
<div class="meta">${escapeHtml(message.sourceLabel)} · ${escapeHtml(s.project_path)}${s.updated_at ?? s.timestamp ? " · " + escapeHtml(message.labels.updated) + " " + escapeHtml((s.updated_at ?? s.timestamp)!) : ""}</div>
${rows}
</body></html>`;
}

const worker = self as unknown as { onmessage: ((event: MessageEvent<ExportMessage>) => void) | null; postMessage: (value: unknown) => void };
worker.onmessage = (event) => {
  try {
    const message = event.data;
    const content = message.format === "md"
      ? toMarkdown(message)
      : message.format === "json"
        ? toJson(message)
        : toHtml(message);
    worker.postMessage({ ok: true, content });
  } catch (error) {
    worker.postMessage({ ok: false, error: String(error) });
  }
};

export {};
