# Agent Capability Model

Date: 2026-06-09

Code Dejavu should treat every coding assistant as a partial data source. Do not assume that a
provider has instructions, rules, memory, session resume, subagents, usage, archive support, or
stable local storage.

## Core Product Objects

- `AgentSource`: a registered assistant backend, such as Claude Code, Codex CLI, OpenCode, Gemini
  CLI, Cursor, Windsurf, Cline, Roo Code, Aider, Continue, or a transcript importer.
- `Capability`: an independently declared behavior that this source supports now.
- `Artifact`: a concrete object read from or written to a source: session, transcript, instruction
  file, rule file, memory file, config entry, workflow, checkpoint, or archive snapshot.
- `Scope`: where the artifact applies: global user, organization, workspace, project, repository
  subtree, session, task, pull request, or imported transcript.

The UI should expose actions from capabilities, not from hard-coded source names.

## Capability Vocabulary

Capabilities should be independent. Do not make one imply another.

- `sources.detect`: detect whether the source exists on this machine.
- `sessions.read`: list and render conversation history.
- `sessions.search`: index and search conversation, tool, and reasoning text.
- `sessions.resume`: reopen a native session.
- `sessions.share`: create or inspect a native share artifact.
- `sessions.subagents`: render child sessions, task sessions, or delegated agent runs.
- `usage.read`: expose model, token, credit, or cost context when present.
- `instructions.read`: inspect durable instruction surfaces.
- `instructions.write`: safely edit durable instruction surfaces.
- `rules.read`: inspect rule or policy surfaces.
- `rules.write`: toggle or edit rule or policy surfaces.
- `memory.read`: inspect long-term or project memory.
- `memory.write`: edit long-term or project memory.
- `workflows.read`: inspect reusable commands, workflows, or skills.
- `workflows.write`: edit reusable commands, workflows, or skills.
- `archive.read`: browse saved snapshots.
- `archive.write`: create, restore, or delete snapshots.
- `tools.read`: list configured external tools, MCP servers, hooks, or connectors.
- `tools.write`: edit tool, MCP, hook, or connector configuration.

The backend `Capabilities` struct uses only verb-based flags (`sessions_read`, `sessions_search`,
`sessions_resume`, `sessions_subagents`, `rules_read`, `rules_write`, `memory_read`, `memory_write`,
`instructions_read`, `instructions_write`, `archive_read`, `archive_write`). Do not add broad
category booleans such as `rules` or `can_resume`; those make weak providers ambiguous.

## Capability Rules

- A weak agent is still useful if it contributes one reliable artifact type, for example read-only
  sessions.
- No provider should fake support by mapping unsupported concepts to empty files.
- If an agent has no rules, report no rule capability.
- If an agent has instructions but no toggleable rules, keep those concepts separate.
- If a source stores data in a proprietary local database, start with read-only browsing.
- If a source is cloud-only, model it as connector/import support unless it exposes a stable API.
- Write support must be native, scoped, reversible where practical, and covered by tests.

## Provider Contract

- `AgentProvider` methods default to "empty read" or "unsupported action" where possible. A new
  provider should only override methods for capabilities it actually declares.
- Session paging and JSONL caching are shared through `LineParser`; a JSONL provider should supply
  parser hooks instead of building a second pager.
- Database-backed providers can build `SessionRecord`s directly, but should still expose the same
  `SessionSummary`, `IndexDoc`, and resume-command contracts.
- Commands resolve providers by the requested capability. A default source for `sessions.read`
  should be the first available provider that supports `sessions_read`, not the first registered
  provider.
- Search indexing should consume `provider.index_documents()` only from providers that declare
  `sessions_search`.

## UI Contract

- Aggregate read-first surfaces: sessions, search, recent activity, source health.
- Gate write actions: editing rules, memory, instructions, tool config, archives, and resume.
- Empty states should distinguish "no data" from "source does not support this capability".
- Source cards should summarize supported capabilities without implying future work.
- Implementation plans belong in docs, not in the product interface.

## Product Layout Implication

- `Dashboard`: recent activity only.
- `Sessions`: aggregate across all sources with source filters.
- `Memory`: show only sources with `memory.read`; write only with `memory.write`.
  Memory project discovery and file writes should route through `AgentProvider`; source-specific
  project IDs are opaque to the UI.
- `Rules`: show only sources with `rules.read`; write only with `rules.write`.
  The command layer should route rule reads and writes through `AgentProvider`; provider-specific
  file layouts belong in provider modules.
- `Instructions`: generalize the current `CLAUDE.md` page into source-scoped instruction surfaces.
- Source detection and availability belong inside the surfaces that need them, such as session
  filters and instruction source groups. Do not create a standalone source page unless it provides
  real configuration or repair actions.
- `Archives`: operate on snapshot-capable sources and scopes only.
  Snapshot commands should route through `AgentProvider`; snapshot content and restore semantics are
  source-owned and must not be generalized by copying arbitrary directories in the command layer.

Read operations can be unified. Write operations must remain source-scoped and capability-gated.
