# Spores

A spore is a `spore.json` manifest. It declares node types, edge types, and *harvesters* that infer
nodes from files. Harvested nodes are always derived — Aneural never writes them back.

```json
{
  "$schema": "https://aneural.dev/schema/spore-v1.json",
  "name": "comments", "version": "0.1.0", "displayName": "Code Comments",
  "description": "…", "license": "MIT", "aneural": ">=0.1",
  "nodeTypes": [{ "kind": "Comment", "label": "Comment", "icon": "LuMessageSquare", "color": "#e8c170", "shape": "pill" }],
  "edgeTypes":  [{ "kind": "ANNOTATES", "label": "annotates", "style": "dotted" }],
  "harvesters": [ … ],
  "panel": { "title": "Comments", "columns": ["tag", "file", "line"] }
}
```

## Harvester kinds

- **`regex`** — `include`/`exclude` globs, a Rust `pattern` applied per line with named captures.
  `once: true` stops at the first matching line, for a pattern that marks what a file *is* — a
  shebang, a module constant — rather than a thing to collect one of per occurrence.
- **`tree-sitter`** — `language` + `query` (tree-sitter S-expression); each match's captures become
  template variables.
- **`markdown`** — `granularity: document | heading | table-row`; optional `wikilinks` (`[[Name]]` → edges,
  resolved by name — file stem or an `aliases:` the target declares — preferring the same directory)
  and `annotate` (a frontmatter list of paths → edges).
  Variables: `{file} {title} {heading} {line} {body} {fm.<key>}`.
  `wikilinks.unresolved` takes a node to emit for a link whose target does not exist, with
  `{target}` and `{slug}`; `tags` takes a `frontmatterKey` (default `tags`) and a `node` with
  `{tag}` and `{slug}`. Both emit **shared** nodes — one per name, belonging to no file, so they
  carry no origin and are collected once nothing points at them.
  `granularity: table-row` emits one node per row of a pipe table, with `{col.<key>}` per column
  plus `{table}` (the nearest heading above it), `{headers}`, `{row}` and `{line}`; `table.requires`
  names columns a table must have, which is how a harvester picks the one it means in a document
  holding several. **Column keys are the header slugged with `_`** — `last_refreshed`, not
  `last-refreshed` — because a template variable matches `[A-Za-z_][A-Za-z0-9_.]*`, so a hyphen
  would render as the empty string and say nothing about why. A cell that is a single backtick span
  is unquoted (`` `aact` `` → `aact`), cells are capped at 2000 characters, and a pipe is not a
  separator when it is escaped, inside a backtick span, or inside `[[a|b]]`. Rows may be ragged: a
  row is read for the columns it does have.
- **`sqlite`** — a local database's schema; the one kind given a *path* instead of bytes.
- **`http`** — a JSON web API; the one kind with no file behind it at all. Requires an `http`
  capability, so it is tier 1 and consent-gated. See below.
- **`wasm`** — reserved.

## `emit`

```json
"emit": {
  "node": { "kind": "Comment", "id": "comment:{file}#{hash(text)}", "label": "{text}", "props": { "tag": "{upper(tag)}", "line": "{line}" } },
  "edges": [ { "kind": "ANNOTATES", "src": "$node", "dst": "file:{file}", "props": { "line": "{line}" } } ]
}
```

Template functions: `hash`, `slug`, `upper`, `lower`, `trim`, `basename`, `stem`. `$node` refers to the
emitted node. Numeric-looking props become numbers.

## Icons

`icon` must be a name from the curated icondata registry (`aneural spores validate` / `aneural doctor`
flag unknown names; the GUI falls back to `LuCircleDot`). List them with `@aneural/core`'s `listIcons()`.

## Marketplace

See `docs/marketplace.md` for identity, tiers, registries, consent and the lockfile.

## Harvester: `sqlite`

The one harvester given a file's *path* rather than its bytes, so it can open a database larger than
`MAX_PARSE_BYTES` instead of reading it into memory. Opened **read-only**.

```json
{ "id": "schema", "kind": "sqlite", "include": ["**/*.db"],
  "references": { "edgeKind": "REFERENCES" },
  "emit": { "node": { "kind": "Table", "id": "acme.db.table:{file}#{table}", "label": "{table}" } } }
```

Variables: `{table} {columns} {columnCount} {rowCount} {sql} {mtime}`. `references` turns each foreign
key into an edge to the referenced table, resolved through the same id template.

`sampleRows` (default 0, capped at 10, values truncated) puts real rows into a `sample` prop. Think
before enabling it: node props are served to coding agents over MCP, so your data would be going to a
model. Schema-only is the default for that reason.

## Harvester: `http`

The only harvester that is not driven by the walker. It runs on a refresh interval, and its nodes hang
off a synthetic origin (`spore://<spore id>/<harvester id>`) rather than a path.

```json
{
  "kind": "http", "id": "open-pulls",
  "request": {
    "url": "https://api.github.com/repos/{setting.repo}/pulls?state=open",
    "headers": { "Authorization": "Bearer {secret.githubToken}" }
  },
  "select": "", "maxPages": 2, "refreshSeconds": 300,
  "emit": { "node": { "kind": "PullRequest", "id": "pull:{item.number}", "label": "{item.title}" } },
  "expand": {
    "request": { "url": "https://api.github.com/repos/{setting.repo}/pulls/{item.number}/files" },
    "edges": [{ "kind": "TOUCHES", "src": "$parent", "dst": "file:{item.filename}" }]
  }
}
```

- `select` is an RFC 6901 JSON pointer to the array of records; empty means the body is the array.
- `maxPages` follows `Link: rel="next"`, defaulting to 1 and capped at 10.
- `refreshSeconds` defaults to 300 and may not go below 60.
- `expand` makes one follow-up request per record and may emit **only edges**, capped at 50 records.

Variables: `{item.<dotted.path>}` flattened from each record — `{item.user.login}`,
`{item.labels.0.name}`, and `{item.labels.name}` for a whole column of an array of objects — plus
`{setting.<key>}`, `{readAt}`, and inside `expand`, `{parent.*}` for the outer record and `$parent`
for the node it emitted.

A manifest must declare every setting it uses and every secret it reads — and for each secret, the
hosts it may be sent to: `{ "kind": "secret", "names": ["githubToken"], "hosts": ["api.github.com"] }`.
`aneural spores set <id>
<key> <value>` records a setting; a secret comes from `ANEURAL_SECRET_<UPPER_SNAKE>` in the
environment and is never stored by Aneural. `docs/marketplace.md` has the full rule set and the
reasoning behind each one.
