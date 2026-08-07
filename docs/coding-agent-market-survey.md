# Coding Agent Market Survey

Date: 2026-06-08

This survey is for Code Dejavu product and provider design. It is not a model-quality benchmark.
The useful question is: which local or cloud artifacts can this app safely read, search, resume,
archive, or edit?

## Sources Used

- Anthropic Claude Code docs: https://code.claude.com/docs/llms.txt
- OpenAI Codex CLI docs: https://developers.openai.com/codex/cli
- OpenCode docs: https://opencode.ai/docs/
- Windsurf docs index: https://docs.windsurf.com/llms.txt
- Cursor docs: https://cursor.com/docs
- Gemini CLI repository/docs: https://github.com/google-gemini/gemini-cli
- Cline docs index: https://docs.cline.bot/llms.txt
- Aider docs: https://aider.chat/docs/usage/conventions.html
- GitHub Copilot product/docs entry points: https://github.com/features/copilot

Some vendors expose rich static docs indexes; others render docs dynamically or keep agent data
cloud-side. Treat any provider without stable local artifacts as lower priority for direct indexing.

## Evaluation Dimensions

- Local session artifact: JSONL, SQLite, markdown transcript, editor storage, or cloud only.
- Instruction surface: repo file, global file, UI setting, prompt profile, or none.
- Rule surface: explicit rules/policies distinct from instructions.
- Memory surface: durable user/project memories distinct from rules and instructions.
- Tool surface: MCP, hooks, extensions, custom commands, or native plugins.
- Resume surface: native session resume, task reopen, PR/issue reopen, or none.
- Parseability: whether Code Dejavu can read it safely without reverse-engineering volatile state.
- Write risk: how dangerous it is to edit artifacts from this app.

## Priority Summary

### Tier 1: Good first-class providers

These have useful local artifacts or are already implemented here.

| Agent | Current fit | Why |
| --- | --- | --- |
| Claude Code | Very high | Rich local/project surfaces, session history, CLAUDE.md, settings, hooks, skills, rules, memory, subagents, archive-like profiles. |
| Codex CLI | High | Local sessions are already implemented in this repo; config and instruction surfaces should be added carefully. |
| OpenCode | High | Local SQLite session store is already implemented; child sessions/subagents are readable. |
| Gemini CLI | Medium-high | Has GEMINI.md, settings, built-in tools, shell/file tools, and MCP. Need inspect local session durability before full provider. |
| Aider | Medium | Mature CLI with convention files and repo-map behavior. Session artifacts may be less rich than Claude/Codex/OpenCode. |
| Cline / Roo Code | Medium | Strong local/editor extension model, explicit rules and memory concepts, MCP. Need inspect VS Code storage and extension files. |

### Tier 2: Useful but more editor/proprietary

| Agent | Current fit | Why |
| --- | --- | --- |
| Cursor | Medium | Official docs cover Agent, Rules, MCP, Skills, and CLI. Local storage and memories may be more product-managed than file-native. |
| Windsurf Cascade | Medium | Docs explicitly cover memories/rules, AGENTS.md, MCP, hooks, workflows, analytics, and checkpoints. Need confirm local storage stability. |
| Continue | Medium-low | Strong context/provider framework, but product artifacts are more assistant/config oriented than session archive oriented. |
| Zed Agent/Assistant | Low-medium | Useful editor assistant, but less clear stable local session/rule/memory surface for external indexing. |

### Tier 3: Cloud-task providers and import targets

| Agent | Current fit | Why |
| --- | --- | --- |
| GitHub Copilot coding agent | Low for local provider, high as connector | Cloud/task/PR oriented. Best treated via GitHub APIs, issues, PRs, logs, and comments rather than local files. |
| Devin | Low for local provider | Cloud workspace and task oriented. Treat as connector/import if API/export exists. |
| Replit Agent | Low for local provider | Cloud workspace and checkpoints. Useful as import/export target, not local index first. |
| Bolt/Lovable/v0-style builders | Low | Product artifacts are app/project/cloud oriented, not coding-agent transcript stores first. |

## Agent Notes

### Claude Code

Claude Code is the strongest fit for Code Dejavu. Anthropic documents it as an agentic coding tool
available across terminal, IDE, desktop app, and browser. Its docs index includes CLAUDE.md,
settings, hooks, skills, commands, subagents, workflows, rules, auto memory, MCP, sessions, and
external session storage.

Product implication:

- Keep it as the richest provider.
- Do not let its rich model define the minimum contract for other sources.
- Split `instructions`, `rules`, `memory`, `tools`, and `sessions` cleanly so weaker agents can
  implement only a subset.

### Codex CLI

Codex CLI is implemented in this repo as a session provider. The current backend
reads `~/.codex/sessions/**/rollout-*.jsonl`, extracts session metadata, messages, tool calls, tool
results, model context, usage, search documents, instruction discovery, and resume support.

Product implication:

- Keep sessions/read/search/resume as first-class.
- Keep config and instruction adapters conservative; current instruction surfaces are read-only.
- Do not claim memory or rule support until a real Codex artifact maps to those concepts.

### OpenCode

OpenCode is implemented in this repo as a session provider. The provider reads
`~/.local/share/opencode/opencode.db`, builds sessions from SQLite `session`, `message`, and `part`
tables, contributes search documents, discovers instruction/config files, and maps child sessions
to subagents.

OpenCode docs show terminal-first usage, share/undo flows, config, commands, and customization.

Product implication:

- Keep SQLite session reading as read-only.
- Treat child sessions as `sessions.subagents`.
- Keep OpenCode config/instruction surfaces read-only until write semantics are verified.

### Gemini CLI

The Gemini CLI repository and docs refer to `GEMINI.md` context files, settings, built-in tools
including file operations and shell, Google Search grounding, and MCP server integration.

Product implication:

- Candidate provider for instructions, tools, and possibly sessions.
- Start with source detection, `GEMINI.md` discovery, and settings inspection.
- Do not assume durable local sessions until verified.

### Cursor

Cursor is a full editor product. The official docs landing page describes Agent mode, Rules, MCP,
Skills, CLI, model settings, and enterprise setup.

Product implication:

- Good conceptual fit for rules and agent workflows.
- Risk is local storage stability and proprietary state.
- Start with repo-visible artifacts such as rules/skills if they are stored in files; treat memories
  cautiously if they are app-managed.

### Windsurf Cascade

Windsurf docs explicitly list Cascade memories and rules, AGENTS.md, MCP, hooks, workflows, usage
analytics, and checkpoints. This is a strong conceptual match.

Product implication:

- Good candidate for instructions/rules/memory/provider health.
- Verify whether local storage is stable and user-editable.
- Enterprise/system-level rules imply scope must include org/team policy, not only project files.

### Cline

Cline docs list Memory Bank, Rules, and MCP. It is a VS Code extension, so local state may be split
between workspace files, extension storage, and settings.

Product implication:

- Good candidate for rule and memory file discovery.
- Provider should not edit VS Code extension state until storage format is verified.
- Memory Bank can map to `memory.read` if it is file-backed in the workspace.

### Roo Code

Roo Code is closely related to the Cline ecosystem and focuses on rules, modes, memory, and MCP.
It should be investigated alongside Cline.

Product implication:

- Likely similar provider architecture to Cline.
- Treat custom modes as workflow/instruction artifacts, not memory.

### Aider

Aider is a mature CLI. Its docs explicitly support coding convention files such as
`CONVENTIONS.md`, plus repo-map and command-driven workflows.

Product implication:

- Good lightweight source for instructions/conventions.
- Session indexing depends on whether the user enables/keeps chat history files.
- Do not model conventions as rules unless they are enforceable or toggleable.

### Continue

Continue is an open-source assistant framework with context/provider configuration. Its fit is more
about assistant config and context sources than transcript archiving.

Product implication:

- Good for source/config/tool discovery.
- Lower priority for session archive unless stable local conversations are available.

### GitHub Copilot Coding Agent

GitHub Copilot's agentic workflow is strongly GitHub-centered: issues, PRs, task sessions, comments,
and GitHub-managed state. It is not primarily a local transcript database.

Product implication:

- Treat as a connector, not a local provider.
- Use GitHub APIs to import issues, PRs, comments, task logs, and agent outputs.
- Write operations should go through GitHub-native permissions and audit trails.

## Product Decisions

1. Use capability-driven UI, not source-name-driven UI.
2. Aggregate read paths first: sessions, search, recent activity, source health.
3. Keep write paths source-scoped: memory, rules, instructions, tools, archives, resume.
4. Add weak providers without shame: a provider can be "sessions only" or "instructions only".
5. Maintain separate object types for memory, rules, instructions, workflows, tools, and sessions.
6. Prefer file/native APIs. Avoid reverse-engineering volatile editor internals unless read-only and
   clearly marked experimental.
7. First implementation priorities after the current providers:
   - Add provider tests around session parsing, search documents, and capability-gated writes.
   - Add Gemini CLI source detection and `GEMINI.md` inspection.
   - Investigate Cline/Roo workspace files and VS Code storage.
   - Investigate Windsurf/Cursor file-backed rules before touching app-managed memory.

## Provider Implementation Checklist

For each new provider, answer these before coding write support:

- What is the official source of truth for each artifact type?
- Is the artifact file-backed, database-backed, API-backed, or cloud-only?
- Can the artifact be read without mutating the source app?
- Can writes be scoped to one project/workspace?
- Does the source app provide native undo, checkpoints, or restore?
- Can we test with fixture data without requiring a logged-in proprietary account?
- What happens when the source changes its storage format?
- What should the UI say when a capability is unsupported?
