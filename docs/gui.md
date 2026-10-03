# GUI (`aneural-gui`)

```sh
cargo run -p aneural-gui --features dev -- <directory>     # omit the path to get a folder picker
```

- **Engine thread** runs `aneural_engine::run` (full index, then watch) and streams `EngineEvent`s over a
  channel. A `PreUpdate` system applies deltas with a per-frame node budget (`gui.growthBudgetPerFrame`)
  so the graph visibly grows.
- **Nodes** are `Mesh2d` circles coloured by kind with a rasterised icondata icon sprite and a label
  shown when zoomed in past `gui.labelZoomThreshold`. New nodes sprout from their parent and scale in.
  Each carries a halo sprite behind it, unlit by day (see **Circadian** below).
- **Edges** are gizmo cubic Béziers with per-edge deterministic curvature (hyphae). Retained lyon meshes
  and a WGSL pulse material are the upgrade path.
- **Circadian** (`circadian.rs`): one number, `Vibe::night`, read off the local clock — 0 through
  working hours, rising through the evening, 1 around 2:30am — drives the whole look. At 0 the app is
  pixel-for-pixel the tool it has always been: nothing glows, nothing moves. As it rises the palette
  crossfades to a cooler, bioluminescent one (`theme::Palette`), node halos light and breathe, nodes
  wander a couple of pixels off their layout positions (`Drift`, never fed back into the forces or
  picking), pulses of light travel the hyphae, and spore motes drift across the canvas. `gui.circadian`
  (`auto` | `day` | `night`) pins it, and the dial in the top bar cycles the same three for the session.
- **Live** (`live/`): what is happening on this machine right now, as a glow on the nodes involved and
  a **portal** onto them — a small aperture with a camera of its own rendering that corner of the
  graph into a texture, so it carries the real icons, hyphae and night palette rather than a drawing
  of them. The camera never moves on its own; clicking a portal is how you go there. Signals come from
  two places and nothing is stored: the **live delta stream** the app already drains (a new `Plan`, a
  `Session` whose props moved, a new `Commit`, a `Run` changing status, a batch of files changing at
  once), and a **probe** on its own thread for the three things no file says — which Claude sessions
  are alive (`~/.claude/sessions/<pid>.json`, whose entries go stale, so the pid is checked against the
  process table), which processes are running one of this workspace's `Script`s (matched on argv), what
  **work a live session has in flight** whether or not the graph names it, and whether a repository is
  mid-merge. A signal that names no node on the canvas is never raised, which is both the design rule
  and the privacy answer.

  A **task** is the second of those: an agent runs things out of a scratchpad under `/private/tmp`
  that no indexer will ever walk, so the work Aneural most wants to report is work it cannot name in
  advance. A process belongs to a session when the session's id appears where the *system* put it --
  in a scratchpad path or a `claude --resume` argument, never merely somewhere in the command line --
  and the chain that does the work is collapsed into one task, summed and named for the script at the
  top of it rather than the interpreter at the bottom. Shells, `sleep`s and Aneural itself are not
  work. The subject is the place the work is happening: the repository the chain names, else the
  session, else the workspace root. The caption gives the program, the elapsed time, the pids and the
  memory -- never the command line, which is where tokens live. One decaying number per signal drives the glow, the rim and the portal's
  life. The probe's interval is adaptive — 700 ms when something is running in front of you, 5 s
  normally, 30 s unfocused, and stretching all the way to 30 s as load approaches saturation, because
  the work this is here to watch must not be slowed by the watching. `gui.live`, `gui.portals` (0 keeps
  the glow and the top-bar count) and `gui.livePace` (`auto` | `eager` | `calm` | `off`) tune it;
  `false` is the app as it was, pixel for pixel.
- **Layout** (`layout.rs`): computed, not simulated. Each folder is a disc holding its own node, a
  head of its files and a disc per subfolder, packed outward from the root so branches never cross;
  the simulation only eases nodes to their places. Once a workspace has grown in, the canvas is
  rearranged only when asked (`Space`, or opening another workspace): what was placed keeps its
  place and only new growth is seated. While it is *first* growing in, every batch is laid out
  from nothing instead, so the finished shape does not depend on the order things arrived in and
  the root ends up in the middle. Dragging a node moves its branch.
- **Window placement** (`placement.rs`): which monitor the window opens on and whether it fills it,
  read from `~/.config/aneural/window.json` — a per-user file, never the workspace's. See the
  README.
- **Panels** (egui): filters (kinds, edge kinds, repos, search, focus mode + depth), inspector (props,
  edges, pins, notes for the assistant, open in editor), status bar. The
  **marketplace** is an `egui::Modal` opened from the top bar, not a docked panel: `ee00a1c` removed
  the bottom spores panel because a permanent strip obstructed the graph, and a modal costs nothing
  when closed. Its detail pane also carries a spore's declared `settings` as editable fields and, for
  anything above the declarative tier, a **Refresh now** button. Saving a setting goes through the
  registry worker like every other config write, then reloads spores and re-fetches, so the graph
  reflects the new value without the user hunting for a refresh.
- **Focus writer**: `Filters + Selection + neighbourhood + visible ids + notes` → debounced atomic write
  of `.aneural/state/focus.json` (also on exit). This is what `aneural mcp` serves.
- **Camera**: the view follows the graph's bounds (centred on the canvas, i.e. the window minus the
  egui panels) while it first grows, and stops following on the first manual pan/zoom or once the
  index is complete and the layout has settled. Left-drag on empty canvas pans; left-drag on a node
  moves it; right/middle-drag pans anywhere; wheel/pinch zooms to the cursor.
- Keys: `F` frame all · `Space` lay the graph out afresh · `R` reindex · arrows/WASD pan.
