---
title: Platform skeleton
status: in-progress
targets:
  - crates/aneural-engine/src/lib.rs
  - crates/aneural-gui/src/main.rs
  - packages/mcp/src/server.ts
---

# Aneural — Platform Skeleton Plan

## Context

This repository starts as an empty directory. It becomes the monorepo for **Aneural**: a fungi-branded ("aneural organisms") tool for keeping visibility and context over everything in a directory while vibe-coding with Claude Code or Codex. The pieces:

- **GUI** (Rust + Bevy, downloadable): opens any directory (may span many repos) and renders it as a 2D mycelium-styled node graph. Nodes = directories, files, repos, manifests, packages, and spore-derived nodes; edges are typed and named (Neo4j-style). The graph grows live as files change.
- **CLI** (`aneural`, TypeScript on npm): `aneural init` creates an Obsidian-style `.aneural/` directory (config, custom node types, notes/plans that can live above repo level), plus `index`, `spores`, `mcp`, `doctor`.
- **Spores**: lightweight, inferred alternatives to JIRA/Obsidian (harvest `TODO`/`claude:` comments, Claude plan docs, a markdown icebox, wiki-links). "Open Spores Marketplace" = a registry of downloadable declarative spores. Philosophy: index and infer, never generate slop.
- **Focusable context loader**: the GUI's active filters + selection define the context that an **MCP server** serves to Claude Code / Codex. (Decision: MCP. claude.ai "connectors" are remote MCP, so a later HTTP transport covers them.)
- **Node icons** come from the `icondata` crate family.

Decisions already made with the user: full platform skeleton first; TypeScript CLI + MCP, Rust GUI; **one indexer in Rust, exposed to TS via a napi-rs addon (`@aneural/core`)**; analyzers for TS/JS, Python, Rust, Go, Java, PHP, Ruby (TS/JS fully resolved, others extraction + best-effort resolution); 2D graph with dimension-agnostic layout math.

## Environment facts

- macOS, Node 24.18.1, pnpm 12.3.4 present. **Rust toolchain is not installed** — step 1 installs it via rustup (stable 1.98.x; Bevy 0.19 MSRV 1.95).
- Versions below were verified against crates.io / npm on 2026-09-15.

## Repo layout

```
Aneural/
├── Cargo.toml                 workspace (resolver=3), members crates/*, [workspace.dependencies] pins, dev profile
├── rust-toolchain.toml        channel = "stable"
├── package.json               private root, packageManager pnpm@12.3.4, turbo scripts
├── pnpm-workspace.yaml        packages: [packages/*] + catalog (pnpm 12 rejects unknown keys)
├── turbo.json  biome.json  tsconfig.base.json  .gitignore  .github/workflows/{ci,napi-release}.yml
├── crates/
│   ├── aneural-core/     pure types: kind registry, NodeId scheme, Node/Edge/GraphDelta, Config/NodeTypeDef/SporeManifest/Focus serde, .aneural path conventions (no IO)
│   ├── aneural-store/    SQLite index (rusqlite bundled): schema, upsert, delete-by-origin, queries, BFS neighborhood
│   ├── aneural-lang/     tree-sitter import extraction (queries/<lang>/imports.scm) + resolvers (oxc_resolver for TS/JS; heuristics for the rest)
│   ├── aneural-engine/   orchestrator: ignore walker, repo/manifest detection, notify watcher, spore harvesters, incremental reindex → GraphDelta channel
│   ├── aneural-icons/    icondata registry (curated name→Icon table + kind/extension defaults) + `raster` feature (resvg → RGBA)
│   ├── aneural-napi/     cdylib napi-rs bindings over engine/store → @aneural/core
│   └── aneural-gui/      Bevy binary (only crate depending on Bevy)
├── packages/
│   ├── core/             @aneural/core: napi output (index.js, index.d.ts, npm/<platform> dirs)
│   ├── cli/              aneural: commander CLI
│   └── mcp/              @aneural/mcp: MCP v2 server library + serveStdio entry used by `aneural mcp`
├── spores/               first-party: comments/, plans/, icebox/, wiki-links/ (spore.json + README each)
├── fixtures/sample-workspace/   two git repos; TS app with tsconfig paths, py, rs, go, java, php, rb, TODOs, a plan .md
├── .aneural/             dogfood workspace
└── docs/                 architecture.md, graph-schema.md, spores.md, napi-api.md, gui.md
```

## Pinned versions

| Rust crate | Version | Notes |
|---|---|---|
| bevy | 0.19.1 | `default-features=false`, features `2d, bevy_winit, bevy_window, bevy_text, default_font, bevy_ui, multi_threaded, bevy_log, png, bevy_gizmos, bevy_gizmos_render`; no audio. Dev: `dynamic_linking` behind a `dev` feature, `[profile.dev] opt-level=1`, `[profile.dev.package."*"] opt-level=3`. Default macOS linker (do not add lld/mold). |
| bevy_egui | 0.42.0 | UI systems in `EguiPrimaryContextPass`; `ctx_mut()` returns Result |
| bevy_pancam | 0.21.0 | **do not enable its egui feature** (pins bevy_egui 0.40); gate panning via `wants_pointer_input()` |
| bevy_prototype_lyon | 0.17.0 | later upgrade path for retained edge meshes; skeleton uses gizmos |
| tree-sitter | =0.27.0 | ABI 13–15 accepted; iterate `QueryMatches` with `streaming-iterator` |
| grammars | typescript =0.23.2, javascript =0.25.0, python =0.25.0, rust =0.24.2, go =0.25.0, java =0.23.5, php =0.24.2, ruby =0.23.1 | exact pins; startup ABI test |
| oxc_resolver | 11.24.3 | TS/JS resolution incl. tsconfig paths/extends/references, exports maps, symlinks |
| napi / napi-derive / napi-build | 3.12.4 / 3.6.5 / 2.4.2 | features `napi9`, `serde-json` |
| rusqlite | 0.40.2 (`bundled`) | |
| notify / notify-debouncer-full | 8.2.0 / 0.7.0 | skip 9.x RCs |
| ignore / globset / petgraph / rfd | 0.4.33 / 0.4.20 / 0.8.3 / 0.17.2 | rfd folder picker called in `main()` before `App::new()` |
| icondata_core / icondata_lu / icondata_si / icondata_vs | 0.1.0 each | not the umbrella `icondata` crate (19 sets by default) |
| resvg / usvg / tiny-skia | 0.48.1 / 0.48.1 / 0.12.0 | `default-features=false` |
| misc | serde 1, serde_json 1, thiserror 2, blake3 1.8, crossbeam-channel 0.5, regex 1, tracing-subscriber 0.3 | |

| npm | Version | Notes |
|---|---|---|
| @modelcontextprotocol/server, /client | 2.0.0 | v2 split packages; `McpServer`, `registerTool`, `registerResource`, `serveStdio`; log to stderr only. Do not use legacy `@modelcontextprotocol/sdk`. |
| zod | 4.6.5 | |
| @napi-rs/cli | 3.9.1 | `napi build --platform --release --esm`, `create-npm-dirs`, `artifacts`, `prepublish` |
| commander | 15.0.0 | ESM-only |
| tsdown | 0.23.0 | dts via oxc backend → set `isolatedDeclarations: true` |
| typescript | 7.0.2 | `tsc --noEmit` only; no `baseUrl`, `moduleResolution: NodeNext` |
| vitest / turbo / @biomejs/biome / @types/node | 5.0.0 / 2.10.13 / 2.5.13 / ^24 | |

## Graph schema (`aneural-core`)

Node kinds are strings backed by a registry (builtin + `.aneural/nodes/*.json` + spore manifests).

| Kind | Producer |
|---|---|
| `Directory`, `File`, `Repo` (has `.git`), `Manifest` (package.json, Cargo.toml, pyproject.toml, go.mod, pom.xml/build.gradle, composer.json, Gemfile) | engine walker |
| `Package` (external dep) | manifest parsers + unresolved imports |
| `Comment`, `Plan`, `Idea`, `Note` | spores |
| `Symbol` | reserved for later |

| Edge | src → dst | Props |
|---|---|---|
| `CONTAINS` | Directory→Directory/File; Repo→root dir | |
| `BELONGS_TO` | File/Directory/Manifest→Repo | |
| `IMPORTS` | File→File | specifier, symbols[], line, kind: static/dynamic/type-only |
| `RE_EXPORTS` | File→File | symbols[] |
| `REFERENCES` | File→File (weak: require(), `mod`, asset urls) | specifier, line |
| `DEPENDS_ON` | Manifest→Package; File→Package (unresolved import) | range, specifier |
| `ANNOTATES` | Comment/Plan→File/Directory | line |
| `RELATES_TO` | Note/Idea/Plan→any | via: wikilink/frontmatter |

**Stable IDs** = `kind:` + workspace-relative path (content-independent): `dir:apps/web/src`, `file:apps/web/src/index.ts`, `repo:apps/web`, `manifest:apps/web/package.json`, `pkg:npm/react`, `pkg:cargo/serde`, `comment:<file>#<blake3(text)[..12]>`, `plan:.aneural/plans/x.md`, `idea:.aneural/icebox/ideas.md#<slug>`, `note:.aneural/notes/arch.md`. Workspace root = directory containing `.aneural/`.

**SQLite** `.aneural/cache/index.db` (WAL, `user_version=1`): `meta(key,value)`; `nodes(id PK, kind, label, path, repo_id, props JSON, fingerprint, source, origin, created_at, updated_at)` with indexes on kind/path/repo/origin; `edges(id, kind, src FK cascade, dst FK cascade, props, source, origin, UNIQUE(kind,src,dst,source))` with indexes on src/dst/kind/origin; `files(path PK, mtime, size, fingerprint, lang, indexed_at)`; `unresolved(origin, specifier, line, reason)`. Reindex of a file = one transaction deleting nodes/edges by `origin` then inserting; orphan `Package` nodes GC'd at end of run.

## `.aneural/` layout and file shapes

```
.aneural/
  config.json   nodes/*.json   spores/<name>/spore.json   notes/*.md   plans/*.md   icebox/*.md
  state/focus.json   state/layout.json   cache/index.db   .gitignore  (cache/, state/layout.json)
```

`config.json`: `version`, `name`, `roots`, `respectGitignore`, `ignore[]`, `languages[]`, `typescript{tsconfig:"auto", conditionNames}`, `spores{enabled[], registry}`, `nodeTypes{<kind>:{icon,color}}`, `gui{theme, labelZoomThreshold}`.

`nodes/<kind>.json`: `{ kind, label, icon: "<icondata static name>", color, shape, description }` — unknown icon names fall back to `LuCircleDot` with a `doctor` warning.

`state/focus.json` (GUI → MCP handoff, atomic temp+rename, 300 ms debounce): `version, updatedAt, workspace, filters{kinds[], edgeKinds[], repos[], query}, selection{primary, pinned[]}, neighborhood{depth, direction}, visibleNodeIds[], notes`.

## `@aneural/core` napi surface

JSON-in/JSON-out (`serde_json::Value` for props) to keep the type-mapping surface small; long calls via `AsyncTask`:

`version`, `findWorkspace(startDir)`, `initWorkspace(root, opts)`, `loadConfig(root)`, `indexWorkspace(root, {full, onProgress})`, `queryNodes(root, {kinds, repo, pathPrefix, text, limit})`, `getNode`, `getEdges(root, {src, dst, kinds})`, `neighborhood(root, ids, {depth, direction, edgeKinds, limit})`, `readFocus`, `writeFocus`, `listNodeTypes`, `listSpores`, `validateSpore(manifestPath)`, `harvestSpore(root, name)`, `doctor(root)`, `listIcons()`, `watch(root, cb)` (optional in skeleton).

## MCP server (`@aneural/mcp`, served by `aneural mcp`, stdio)

Tools: `aneural_focus` (focus.json + resolved selected/neighbor nodes + edges + bounded file excerpts), `aneural_search_nodes`, `aneural_get_node`, `aneural_neighborhood`, `aneural_get_edges`, `aneural_read_file` (bounded, line range), `aneural_list_node_types`, `aneural_list_spores`, `aneural_index` (idempotent, not read-only), `aneural_add_idea` (only write: append to icebox; optional).
Resources: `aneural://focus` (json), `aneural://focus/context.md` (rendered bundle; `sendResourceUpdated` on focus.json change via fs.watch), `aneural://node/{id}` template, `aneural://file/{path}`, `aneural://node-types`, `aneural://spores`. Prompt: `aneural_context`.
`aneural init --claude` writes a `.mcp.json` entry (`npx aneural mcp`); docs cover Codex `~/.codex/config.toml`.

## Bevy app (`aneural-gui`)

Startup: dir from argv or `rfd` folder picker, before `App::new()`. Run: `cargo run -p aneural-gui --features dev -- <dir>`.

| Plugin | State | Behavior |
|---|---|---|
| Workspace | `Workspace{root, config, node_types}` | load config + registry |
| Engine | `EngineRx(crossbeam Receiver<GraphDelta>)` | `std::thread` runs `aneural_engine::run(root, tx)` (full index streamed in batches, then watcher); `PreUpdate` drain with a per-frame budget (~200 nodes) so growth is visible |
| Graph | `GraphState{by_id, petgraph StableGraph}`; components `GraphNode`, `GraphEdge`, `Pos(Vec3)`, `Vel(Vec3)`, `Pinned`, `GrowIn`, `Hidden` | new nodes spawn at parent position + jitter; `GrowIn` tween scale 0→1 (ease-out-back), edges grow 0→1 |
| Layout | `LayoutParams` | force-directed on Vec3 with z locked to 0, in `FixedUpdate`, damping, freeze below energy ε; Barnes–Hut later |
| Render | `IconAtlas`, `KindStyle` | node = `Mesh2d(Circle)` + `ColorMaterial` + child `Sprite` icon (white raster, tinted) + `Text2d` label above zoom threshold. Edges = `Gizmos` cubic Béziers (control points offset by per-edge seeded noise for organic curvature), `linestrip_2d`, drawn twice (wide low-alpha glow + thin core); growth param truncates the strip. Upgrade path: lyon retained meshes → custom `Material2d` WGSL pulse. |
| Camera | `Camera2d` + `PanCam` | `pancam.enabled = !egui.wants_pointer_input()` |
| Picking | `Selection{primary, pinned}` | `On<Pointer<Click>>` observers, hover highlight |
| Ui (egui) | `Filters{kinds, edge_kinds, repos, query}` | left: filters/search; right: inspector (props, edges, open in editor); bottom: spores panel; top: index status |
| Focus | `Focus` derived from Filters + Selection + neighborhood | change-detected, debounced atomic write of `state/focus.json`; also on `AppExit` |
| Icon | uses `aneural-icons` `raster` | rasterize curated icons at 32 px and 128 px at startup; pick by zoom |

### Icons (`aneural-icons`)
- Depend on `icondata_core` + `icondata_lu` (Lucide), `icondata_si` (Simple Icons: language logos), `icondata_vs` (VS Code codicons). Never `use icondata_*::*`.
- Render by rasterizing at startup: wrap `IconData` fields into an `<svg viewBox=… fill/stroke/stroke-width/linecap/linejoin>{data}</svg>`, `currentColor` → `#ffffff`, `usvg::Tree::from_str` → `resvg::render` → RGBA → Bevy `Image` (`Rgba8UnormSrgb`), tint via `Sprite.color`. Keep an `IconSource` trait so `bevy_vello` can replace it later.
- Registry: a `icon_table!` macro over a curated subset generates `ICONS: &[(&str, Icon)]` so a typo is a compile error; `lookup(name)`, `names()`, `default_icon(kind, path)`. Icon statics are `#[doc(hidden)]`, so the first implementation step greps `~/.cargo/registry/src/*/icondata_*-0.1.0/src/lib.rs` to confirm each name.
- Defaults: Directory `LuFolder`/`LuFolderOpen`, Repo `VsRepo`, Manifest `VsPackage`, Package `LuPackage`, File `LuFile`, `.ts/.tsx` `SiTypescript`, `.js*` `SiJavascript`, `.py` `SiPython`, `.rs` `SiRust`, `.go` `SiGo`, `.java` `SiOpenjdk`, `.php` `SiPhp`, `.rb` `SiRuby`, `.md` `SiMarkdown`, `.json` `VsJson`, `.yaml/.toml` `LuFileCode`, Comment `LuMessageSquare`, Plan `LuMap`, Idea `LuLightbulb`, Note `LuStickyNote`, Spore `LuPuzzle`, app mark `LuSprout`.

## Spores

`spore.json`: `name, version, displayName, description, license, aneural (semver range), nodeTypes[{kind,label,icon,color,shape}], edgeTypes[{kind,label,style}], harvesters[], panel{title, columns}`.
Harvester kinds, all run by built-in Rust runners in `aneural-engine::spores` (no third-party code execution; `wasm` kind reserved):
- `regex`: globset include + line regex with named captures → templated node/edge emit (`{file} {line} {text} {hash(x)} {slug(x)}`).
- `tree-sitter`: query over supported languages, captures as template vars.
- `markdown`: frontmatter → props/targets, headings → sections, `[[Wiki Links]]` → `RELATES_TO` resolved by basename.

First-party: **comments** (`TODO|FIXME|HACK|claude:` → `Comment` + `ANNOTATES`), **plans** (`.aneural/plans/**/*.md` and `**/.claude/plans/*.md` → `Plan`, `ANNOTATES` via frontmatter `targets`), **icebox** (each `## heading` in `.aneural/icebox/*.md` → `Idea`), **wiki-links** (`RELATES_TO` across all `.md`). Harvested nodes are always derived, never written back.

Marketplace: `registry.json` in a GitHub repo (`aneural/spores`) listing `{name, description, version, repo, path, sha256}`. `aneural spores add <name>` fetches, verifies sha256, runs `validateSpore`, installs to `.aneural/spores/<name>/`; also `list|remove|update`.

## Implementation steps (each independently verifiable)

1. **Toolchain + shell.** Install rustup stable; `git init`; `rust-toolchain.toml`; workspace `Cargo.toml` with pins + dev profile; root `package.json`, `pnpm-workspace.yaml` (+catalog), `turbo.json`, `biome.json`, `tsconfig.base.json` (strict, isolatedDeclarations, NodeNext, no baseUrl); `.gitignore`. Verify: `cargo --version`, `pnpm install`, `pnpm turbo run build`.
2. **aneural-core.** Kinds, NodeId canonicalization, Node/Edge/GraphDelta, Config/NodeTypeDef/SporeManifest/Focus serde + JSON Schema export test. Verify: `cargo test -p aneural-core`.
3. **aneural-store.** Schema/migrations, upsert, delete-by-origin, queries, BFS neighborhood; in-memory DB tests.
4. **aneural-engine walk.** `ignore::WalkBuilder` (+ `.aneuralignore`) → Directory/File/Repo/Manifest + CONTAINS/BELONGS_TO; manifest parsers → Package + DEPENDS_ON. Build `fixtures/sample-workspace`. Verify: node/edge count assertions.
5. **aneural-lang.** Parsers + `imports.scm` per language; ABI test that `set_language` succeeds for all 8; TS/JS resolution via `oxc_resolver` (nearest tsconfig, `.js→[.ts,.tsx,.js]` extension alias, condition names); heuristic resolvers for py/rs/go/java/php/rb; unresolved → `pkg:` nodes + `unresolved` rows. Verify: fixture IMPORTS edges including an `@/…` alias.
6. **Engine orchestration + incremental.** blake3 fingerprints, `notify-debouncer-full` watcher, reindex changed origins, `GraphDelta` over crossbeam; `Engine::index_full`, `Engine::watch`. Verify: integration test touches a fixture file → delta arrives.
7. **Spores.** Manifest loading (`spores/*` builtin + `.aneural/spores/*`), regex/markdown/tree-sitter runners, four first-party manifests. Verify: TODO → Comment + ANNOTATES; plan md → Plan.
8. **aneural-icons.** Registry macro + curated table + defaults; `raster` feature. Verify: every curated name resolves; rasterizing `LuLeaf` at 64 px yields non-blank alpha.
9. **aneural-napi + packages/core.** `crate-type=["cdylib"]`, `build.rs`; `packages/core/package.json` napi config (`binaryName: aneural-core`, targets darwin-arm64, darwin-x64, linux-x64-gnu, linux-arm64-gnu, win32-x64-msvc); build script `napi build --platform --release --esm --manifest-path ../../crates/aneural-napi/Cargo.toml -o .`; vitest smoke test. Verify: `node -e "import('@aneural/core').then(m=>console.log(m.version()))"`.
10. **packages/cli.** commander 15, tsdown build with `bin`; commands `init [--claude]`, `index [--watch]`, `query`, `focus`, `spores list|add|remove|update`, `mcp`, `doctor`. Verify: build, then `init` + `index` on the fixture.
11. **packages/mcp.** `McpServer` v2 tools/resources above, focus.json watcher → `sendResourceUpdated`; `aneural mcp` calls `serveStdio`. Verify: vitest round-trip via `@modelcontextprotocol/client` over stdio; manual `claude mcp add aneural -- npx aneural mcp`.
12. **aneural-gui.** Plugins in order: window + camera → engine thread + node spawn with GrowIn → gizmo hyphae → layout → egui panels → picking/selection → icons → focus writer. Verify: run on the fixture and watch growth; append a TODO to a fixture file → a node sprouts live; click a node → `state/focus.json` updates → `aneural_focus` returns it.
13. **Dogfood + docs + CI.** `aneural init` on this repo; `docs/*.md`; GitHub Actions `ci.yml` (`cargo test --workspace --exclude aneural-gui`, `cargo check -p aneural-gui`, `pnpm test`) and `napi-release.yml` from the napi-rs package-template matrix on tags.

## Verification

- `cargo test --workspace --exclude aneural-gui` and `cargo clippy --workspace -- -D warnings` pass.
- `cargo test -p aneural-lang abi_` proves every grammar loads under tree-sitter 0.27.
- `pnpm -r build && pnpm -r test` (core smoke, cli tmp-dir init/index, mcp client round-trip).
- `pnpm --filter @aneural/core pack` contains `index.js` + `index.d.ts`, platform `.node` only in optional platform packages.
- GUI manual pass on `fixtures/sample-workspace`: filters hide/show kinds; selection writes focus.json within ~300 ms; `aneural mcp` reflects it in Claude Code.
- `aneural doctor` on the dogfood workspace lists unresolved imports and bad icon names.

## Risks and mitigations

1. **napi prebuild/publish pipeline.** Copy napi-rs package-template config verbatim; JSON-in/JSON-out API; local `--platform` builds for dev; env var to point the CLI at a local `.node` file.
2. **tree-sitter ABI / stale grammars** (typescript grammar is from 2024). Exact `=` pins; startup ABI test; extraction reads error-recovered trees; fallback of vendoring `parser.c` via `cc`.
3. **Bevy compile times / API churn.** Bevy isolated in one crate; `dynamic_linking` dev feature; dev-profile opt-levels; minimal features; pin 0.19.x.
4. **TS/JS resolution edge cases.** `oxc_resolver` covers paths/extends/references/exports/symlinks; failures land in `unresolved` and surface via `doctor`; fixture covers alias + exports map + symlink.
5. **Icon pipeline** (0.1.0 crates with hidden docs, raster blur at extreme zoom). Compile-checked curated table; two raster sizes; `IconSource` trait for a later vello swap; `listIcons()` for spore authors.
6. **Toolchain newness** (TypeScript 7.0 compiler API not stable until 7.1; MCP SDK 2.0 just shipped). `isolatedDeclarations` + oxc dts backend; exact pin on the MCP server package; thin MCP layer.
