/**
 * Host badges for data that does not live on this machine.
 *
 * The backend keeps every path it hands over readable, which on Windows means a WSL project comes
 * through as `\\wsl.localhost\<distro>\home\me\app`. That makes the host recoverable from the path
 * itself, so no extra field has to be threaded through session summaries, rules or instructions
 * just to render a badge.
 *
 * Opaque keys (project slugs, snapshot names) instead carry an explicit `@wsl:<distro>/` prefix.
 * They are passed back to the backend verbatim — only display strips the tag.
 */

const UNC_HOST = /^\\\\(?:wsl\.localhost|wsl\$)\\([^\\]+)\\?/i;
const KEY_TAG = /^@wsl:([^/]+)\//;

/** `WSL:Ubuntu` for a path inside a distro, `null` for anything on this machine. */
export function hostOfPath(path: string | null | undefined): string | null {
  const match = UNC_HOST.exec((path ?? "").replace(/\//g, "\\"));
  return match ? `WSL:${match[1]}` : null;
}

/**
 * `WSL:Ubuntu` for a host-tagged key, `null` for a native one. A distro shared by two accounts
 * encodes the account after `~`; it reads better as `WSL:Ubuntu/me`.
 */
export function hostOfKey(key: string | null | undefined): string | null {
  const match = KEY_TAG.exec(key ?? "");
  return match ? `WSL:${match[1].replace("~", "/")}` : null;
}

/** The host badge for anything carrying either form. */
export function hostLabel(value: { project?: string; project_path?: string; name?: string } | string | null): string | null {
  if (typeof value === "string") return hostOfPath(value) ?? hostOfKey(value);
  if (!value) return null;
  return hostOfPath(value.project_path) ?? hostOfKey(value.project) ?? hostOfKey(value.name);
}

/** Drop the `@wsl:<distro>/` prefix for display. Never send the result back to the backend. */
export function withoutHostTag(key: string): string {
  return key.replace(KEY_TAG, "");
}

/** Prefix a key so the backend routes it to `host` (e.g. `WSL:Ubuntu`). */
export function withHostTag(key: string, host: string | null): string {
  if (!host) return key;
  const distro = host.startsWith("WSL:") ? host.slice(4) : host;
  return `@wsl:${distro}/${key}`;
}

/** A WSL UNC path shown the way the distro itself would: `Ubuntu:/home/me/app`. */
export function displayPath(path: string): string {
  const normalized = path.replace(/\//g, "\\");
  const match = UNC_HOST.exec(normalized);
  if (!match) return path;
  const rest = normalized.slice(match[0].length).replace(/\\/g, "/");
  return `${match[1]}:/${rest}`;
}
