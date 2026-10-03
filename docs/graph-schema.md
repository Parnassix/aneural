# Graph schema

## Node kinds

| Kind | Id | Producer | Notable props |
|---|---|---|---|
| `Directory` | `dir:<path>` (`dir:.` is the root) | walker | |
| `Repo` | `dir:<path>` | walker (dir containing `.git`) | `repo: true` |
| `File` | `file:<path>` | walker | `lang`, `ext`, `size`; `fingerprint` column |
| `Manifest` | `file:<path>` | walker (package.json, Cargo.toml, …) | as File |
| `Package` | `pkg:<ecosystem>/<name>` | imports of external packages | `ecosystem` (npm, cargo, pypi, go, maven, packagist, rubygems) |
| `Session` | `session:<uuid>` | `claude` producer | `title`, `branch`, `bridge`, `startedAt`, `endedAt`, `prompts`, `files`, `plans`, `tokens`†, `models`, `byModel`† |
| `Commit` | `commit:<repo>@<sha>` | `git` producer | `sha`, `short`, `author`, `at`, `session`, `files` |
| `Comment` | `comment:<file>#<hash(text)>` | spore `comments` | `tag`, `line`, `file` |
| `Plan` | `plan:<file>` · `plan:~/.claude/plans/<name>.md` | spore `plans` · `claude` producer | `file`, `status`; for a Claude plan `state`, `approvedAt`, `names`, `namesInGraph`, `steps`, `tokens`†, `models`, `byModel`† |
| `Usage` | `usage:workspace` · `usage:repo/<path>` | `claude` producer | `scope`, `sessions`, `tokens`†, `models`, `byModel`†; and where they differ `sessionsElsewhere`, `sessionsSwept`, `tokensSwept` |
| `Script` | `script:<path>` · `script:<path>#<entry>` | spore `scripts` | `file`, `found: shebang\|directory`, `interpreter` |
| `Schedule` | `schedule:<file>#<key>` · `schedule:~/installed/<label>` · `schedule:~/managed/verdict/<slug>` | spore `scripts` · a workspace spore · `runs` producer | `declaredBy` says which claim this is — see below. A declaration adds whichever of `cron`, `cadence`, `lastRefreshed`, `command`, `workflow`, `driftSignal` it gave; `declaredBy: installed` adds `mechanism`, `when`, `loaded`, `everyDays`; `declaredBy: verdict` adds `due`, `nextDue` and `drift` |
| `Run` | `run:<path>[#<entry>]` | `runs` producer | one per script, the latest run: `runId`, `status`, `trigger`, `fidelity`, `machine`, `startedAt`; and whichever of `endedAt`, `exitCode`, `durationMs`, `outBytes`, `errBytes`, `tail`, `logPath` is known |
| `Idea` | `idea:<file>#<slug(heading)>` | spore `icebox` | `file`, `line`, `body` |
| `Note` | `note:<file>` | spore `wiki-links` | `file` |
| `MissingNote` | `missing:<slug(target)>` | spore `wiki-links` | `target`, `unresolved: true` |
| `Tag` | `tag:<slug(tag)>` | spore `wiki-links` | `tag` (as written) |
| `Symbol` | `sym:…` | reserved | |

† The token props are always the same six, and they are kept apart on purpose: `tokens` is the
sum, and `tokensIn`, `tokensOut`, `cacheWrite`, `cacheRead` and `messages` are the parts, with
`tokensThinking` when there was any. A cached input token and a fresh one differ by an order of
magnitude in price, so the sum is a size, never a bill. `tokensThinking` is *inside* `tokensOut`
and is never added to it. Nothing carries a price: what a token costs depends on a plan Aneural
cannot see.

`byModel` is the same six counts again, split by the model that ran them and ordered busiest
first — `[{model, tokens, in, out, thinking, cacheWrite, cacheRead, messages}]` — with `models` as
the names alone for a one-line summary. The split is the substance rather than a detail: a token's
worth depends entirely on which model produced it, so a total spanning several is a size and
nothing more. The rows always add up to the headline. See [Token usage](#token-usage).

Kinds are open strings. Custom kinds come from `.aneural/nodes/<kind>.json`
(`{ kind, label, icon, color, shape, description }`) or spore manifests. Every node has `repoId` (nearest
enclosing repo) when inside one.

## Edge kinds

| Kind | src → dst | Props |
|---|---|---|
| `CONTAINS` | Directory/Repo → Directory/File | |
| `IMPORTS` | File → File / Directory (Go packages, Java wildcards) / Package | `specifier`, `line`, `lines[]`, `importKind`, `importKinds[]`, `symbols[]` |
| `REFERENCES` | File → File (require(), `mod`, include) · spore nodes (e.g. foreign keys) | same |
| `ANNOTATES` | Comment/Plan → File (`line` / `via: frontmatter`) | a Claude plan adds `step`, `section` |
| `RELATES_TO` | Note/Idea/Plan/File → File, MissingNote or Tag (`via: wikilink\|unresolved\|tag\|source`, `line`) · Commit → Session (`via: trailer`) | |
| `MODIFIES` | Commit/Session/PullRequest → File | `status`, `at`; a session's also `via: tool\|backup`, `firstAt`, `lastAt` |
| `REALIZES` | Session/Commit/PullRequest → Plan | `via: plan-mode\|session` |
| `ANNOTATES` | Usage → Repo or the root Directory | `via: usage` |
| `REFERENCES` | Script → File (`via: source`) · Schedule → File (`via: declared`) | |
| `ANNOTATES` | Schedule → Script | `via: schedule` |
| `ANNOTATES` | Schedule (verdict) → Schedule (declaration) | `via: verdict` |
| `ANNOTATES` | Run → Script | `via: run` |
| `REALIZES` | Run → Schedule | which claim actually fired |

A Claude plan's `steps` is its own outline — `[{heading, level, line}]` for every `##` and `###`
in the document, in order — and each `ANNOTATES` carries the `step` that named the file. That is
what the GUI walks: one step at a time, with the files it promised and what became of them. A file
named twice belongs to the first step that named it, because edge identity is
`(kind, src, dst, source)` and a second edge for the same pair cannot exist.

A `[[Wiki Link]]` resolves by **name**, not by the path the link happens to carry — Obsidian writes
`[[../../Notes|Notes]]` when a link crosses folders, and that path goes stale the moment a note
moves, while the name is what the author typed. A trailing backslash in the target is markdown
escaping rather than part of the name — inside a table the alias separator has to be written `\|`
or the row breaks, which is how most cross-folder links in a real vault are written — so it is
stripped. A note also answers to any `aliases:` its frontmatter declares, and a real file of a
given name always beats another note's alias.

A link to a note nobody has written yet becomes a **`MissingNote`**, the way Obsidian keeps an
unresolved link in its own graph: in a real vault a good fraction of links are unresolved, and which
notes are being asked for is worth more than a dropped edge. One placeholder is shared by every file
that asks for it, so — like a `Package` — it carries **no origin** and belongs to no single file;
`Store::gc_orphan_by_kind` collects it once the last link to it goes. The engine stamps
`unresolved: true` on it so a reader can tell a placeholder from a real node without knowing which
spore made it. A directory holding an Obsidian vault gets `vault: true`, and `hideUnresolvedLinks`
from the vault's own `.obsidian/graph.json` when it states one; the GUI seeds that kind's filter row
from it once, so the vault's own answer is honoured and one click overrides it. A directory can be a
vault *and* a repo, which is why both are props rather than kinds.

A frontmatter `tags:` list becomes one **`Tag`** node per distinct tag, joined to every file
carrying it (`via: tag`) — written `[a, b]`, as a `- item` block or bare `a, b`, all three read. It
is the same shared-node arrangement as a `MissingNote` and for the same reason: a tag is a name many
files have in common, and one node is what makes "everything tagged `fda`" something you can see
rather than a search. The id is slugged, so `FDA` and `fda` are one tag while the label keeps what
the author typed. In a vault this is usually the densest real structure in the graph — far denser
than the folder tree — because the tags were applied deliberately and the folders mostly were not.

`importKind` is how the file pulled the target in (`static`, `dynamic`, `type-only`, `re-export`, …).
One file importing and re-exporting the same module is a single edge; `importKinds` then lists both.

In the GUI, `CONTAINS` is the folder tree and is always drawn. `RELATES_TO`, `MODIFIES` and
`REALIZES` are never drawn as strands, and **nor is any edge whose end floats** — a node with no
place in the tree that relates to others (a note, idea, plan, session or commit) floats near what it
relates to, and the thread only appears faintly while one end is selected or hovered. That rule is
not cosmetic: one commit that changed twenty files would otherwise draw twenty lines across the
whole tree, and thirty of them turn the graph into a starburst. Only strand kinds can be switched
off, so the drawer lists `IMPORTS`, `REFERENCES`, `ANNOTATES` and spore-declared kinds.

Edges are unique per `(kind, src, dst, source)`. Standard-library imports produce nothing; other
unresolved imports land in the `unresolved` table and surface via `aneural doctor`.

## SQLite (`.aneural/cache/index.db`, `user_version = 4`)

`nodes(id, kind, label, path, repo_id, props, fingerprint, source, origin, created_at, updated_at)`,
`edges(id, kind, src, dst, props, source, origin)`, `files(path, mtime, size, fingerprint, lang, indexed_at)`,
`unresolved(origin, specifier, line, reason)`, `meta(key, value)`. Foreign keys are not enforced;
dangling edges are filtered at query time and cascaded on delete.

## `.aneural/`

```
config.json          workspace config (see aneural-core::config::Config)
nodes/*.json         custom node types
spores/<name>/       installed spores (spore.json)
notes/ plans/ icebox/   markdown harvested by first-party spores
state/focus.json     GUI → MCP handoff (local; state/ is gitignored)
runs/<machine>/<YYYY-MM>.jsonl   the run journal: committed, append-only
.gitattributes       `runs/**/*.jsonl merge=union`
cache/index.db       derived, gitignored
cache/runs.db        derived from the journal, gitignored, own user_version
```

`state/focus.json`:

```json
{ "version": 1, "updatedAt": "…", "workspace": "/abs/root",
  "filters": { "kinds": [], "edgeKinds": [], "repos": [], "query": "" },
  "selection": { "primary": "file:apps/web/src/index.ts", "pinned": [] },
  "neighborhood": { "depth": 1, "direction": "both" },
  "visibleNodeIds": ["…"], "notes": "" }
```

## Scripts and schedules

A `Script` is something in the workspace you can run. The `scripts` spore finds them two ways —
a file with a shebang, and a file sitting directly in `scripts/` or `bin/` — and both emit the same
`script:<path>` id, so a file found both ways is described once, with the shebang harvester
contributing `interpreter` and the other contributing nothing it already has. `found` records which
saw it first. The spore is off by default: a repository built around scripts grows a node per
script, which is two hundred of them.

What a `Script` deliberately does **not** carry is a command. That `scripts/download_pubmed.py`
exists does not say whether it is run with `uv run python`, `python3` or `poetry run`; only the
project knows, so the command belongs in workspace config rather than in a guess.

A `Schedule` is one *declaration* that something should run, and there is deliberately one node per
declaration rather than one per script. A cadence written in a document, a launch agent actually
installed, and what Aneural itself manages are three separate claims about the same script, and they
disagree in practice — that disagreement is the thing worth seeing, so `declaredBy` says which claim
each node is and no node is overwritten to make them agree. A node has exactly one origin, so this
also happens to be the only arrangement that works: the producer that reads installed launch agents
cannot add props to a node the spore owns, because re-harvesting the declaring file would wipe them.

**A cron written in prose is a `cronHint`, never a `cron`.** In the vault this was built against,
one `notes` column holds around two hundred cron expressions; several cells hold more than one, and
some are explicitly proposals ("Default cron suggestion if scheduling independently: `0 5 * * 1`").
A `cron` prop is something a scheduler may act on. A note about intent is not.

Naming is per-project and so is the join. A source's declared cadence is keyed by whatever the
project keys it by, and the key rarely matches a filename: in the repository this was built for,
`download_pubmed.py` serves two sources through a flag, and a third of the declared cadences name a
source whose work is done by an `extract_*` or `build_db_*` script instead. So the mapping belongs in
a workspace spore at `.aneural/spores/<name>/spore.json`, reading the project's own table, with the
`table-row` markdown granularity. A schedule whose `ANNOTATES` target does not exist is not a bug —
it is a cadence kept for a source nobody has written a script for.

### The three claims

`declaredBy` is what tells them apart, and none of them is authoritative:

| `declaredBy` | Origin | Says |
|---|---|---|
| whatever the spore called it (`crontab`, `github-actions`, `cadence-index`, …) | the declaring file | what somebody wrote down |
| `installed` | `runs://installed` | what this machine's launchd actually has, and whether it is loaded |
| `verdict` | `runs://verdicts` | what Aneural makes of the pair: `due`, `nextDue`, and `drift` when they disagree |

The `runs` producer is what adds the second and third, because neither is within a spore's reach: the
launch agents live in `~/Library/LaunchAgents`, outside the workspace, and a due date needs a clock.
It is gated by `runs.enabled`, off by default — that default *is* the consent, the same argument as
`history.claude`. `runs.launchAgentsRoot` overrides the directory, and when it is set `launchctl` is
**not** consulted: agents in some other directory are files launchd has never seen, so reporting one
as loaded because a label of that name is running would simply be false.

**Drift is compared by interval, never by text.** A launch agent says `06:00 every day` and a table
says `daily`; those are one claim written two ways. `everyDays` is what makes them comparable, and
the coarsest calendar field wins — a `Weekday` entry is weekly however precisely it also pins the
hour. An agent with neither a calendar nor an interval (a `KeepAlive` daemon, a run-once job) reports
no `everyDays` at all rather than a number that would invent a disagreement.

Two verdicts are worth knowing the ordering of: **installed-but-not-loaded outranks
wrong-interval**, because it means nothing is running. And a declaration nothing can be said
about — no cadence anyone recognises, nothing installed to compare — grows no verdict node, because
one reading `due: unknown, drift: none` is worse than none at all.

A cadence or last-refresh cell carries commentary in practice (`monthly (POC data on disk; not
formally onboarded)`, `2026-05-12 (audit baseline …; PID 63039 → log …)`), so the leading token is
read as the claim and the rest as prose. `in-progress` in place of a date means a multi-week baseline
is still being built, and reports `building` rather than being overdue forever.

Ids: only a first-party spore may mint a bare `schedule:` id. A third-party or workspace spore emits
the builtin `Schedule` **kind** under its own id namespace (`acme.cadences.schedule:…`) and points
edges at other producers' ids freely — edge targets are not namespaced, only minted ids are.

## Runs

A `Run` node is the **latest** run of one script, and there is exactly one per
script for as long as the script exists. The durable history is not in the graph
at all:

```
.aneural/runs/<machine>/<YYYY-MM>.jsonl   committed, append-only
.aneural/cache/runs.db                    derived, gitignored, user_version = 1
```

The graph holds one node because a node per run would despawn and respawn a
canvas entity every time a schedule fired — 288 a day for a five-minute cron,
losing its position and its pin each time. `runId` and `startedAt` say which run
a node is currently describing, so that a pinned node quietly becoming a
different run is visible rather than silent.

### Why the journal is text and the database is derived

Run history has to survive a `git pull` from another machine, and a committed
SQLite file loses on every axis: every append rewrites pages, so git stores a
megabyte-scale binary blob per commit; two machines committing conflict on
*every* run; and a merge driver would have to open both databases and replay
rows — which is the JSONL ingest anyway, plus a binary format git cannot help
with. So the durable form is text and the fast form is a cache.

Three rules make a merge resolve itself with no merge driver:

| Rule | Why |
|---|---|
| one directory per machine | two machines never write the same file, so the common case has no conflict |
| append only, never edit | mutating a line breaks both `merge=union` and every reader's byte offset |
| the run id is derived, never random | `<machine>/<millis>-<hash(scriptKey)>` is the same string on every clone, so one run arriving from two directions is one row |

Given those, `merge=union` — built into git, nothing to install — is the
*correct* resolution rather than a workaround, because both sides' lines are
true. Whatever duplication it leaves is collapsed at ingest by last-`seq`-wins,
which is idempotent and order-independent; that matters, because a merged file
hands you lines in no useful order. A run writes **two** lines, `seq: 0` when it
starts and `seq: 1` when it finishes, rather than editing the first.

`runs.db` is its own file because `index.db`'s `user_version` means "drop the
cache and rebuild it", which is right for a graph re-derivable from the workspace
in seconds and wrong for run history. Its own version means adding a node kind
never costs anyone their history — and bumping *its* version is also safe,
because the journal is the copy that matters.

Segments are read on from a remembered offset only while the bytes before that
offset still hash the same. A merge, a rebase or a hand edit changes them, and
then the segment is read again from zero with its old rows deleted first, so a
line the rewrite *removed* goes away rather than lingering.

### What `fidelity` means

| | `exact` | `observed` |
|---|---|---|
| who ran it | Aneural | a launch agent |
| `startedAt` | when the process began | when output was last **seen** |
| `durationMs`, `endedAt` | measured | absent — unknown, never zero |
| `status` | measured | from `launchctl list`, or `unknown` |

launchd keeps no history: `launchctl list` gives a pid and the last exit status,
and nothing else survives. So the only evidence an agent ever fired is the log
files its own plist names — a modification time and a size. An agent that named
no logs, or whose logs do not exist, is recorded as **not having run**, because
standing `now` in for the missing instant would mint a new fake run on every tick
of the timer.

Two consequences taken from real data. A **zero-byte stdout beside a busy stderr
is the normal shape** for anything logging through Python, so "no output" is
never read as "never ran". And an unloaded agent reports `unknown` rather than
`ok`: launchd has forgotten how it went, and guessing would turn a job that died
into one that succeeded.

### What is committed, and what is not

The journal carries an exit code, byte counts, a path and a **capped tail**; the
full output stays wherever it was written. Consecutive identical lines are
collapsed to one plus `⋯ ×N` before the cap, because a long job's stderr is
mostly one library complaining in a loop — measured on a real campaign, 40
repetitions of a single CoreGraphics warning filled the whole tail and pushed
both interesting lines off the top. A cap alone honours "nothing unbounded is
committed" while committing almost nothing useful.

Only the end of a log is read — a fixed window, not the file — so the cost
depends on how much is kept rather than on how much was written. The real log
this was measured against is 21 MB and growing, and this runs on a timer.

Retention drops **whole segment files**, never rewrites one: rewriting would
break the union merge and invalidate every clone's offset at once, to save bytes
in a text file that compresses well.

### Nothing here executes anything

Everything above is observation, on the same `runs.enabled` gate as reading what
is installed. `runs.execute` is a separate gate and nothing reads it yet.

## Token usage

Claude Code writes a usage block on every assistant message in its transcripts and no running
total anywhere, so the totals are accumulated as the `claude` producer tails each transcript and
persisted with the rest of its state. They appear in three places, which is the three questions
people ask:

- **Each session** — props on the `Session` node. A session is routinely more than one model — a
  subagent on Haiku, the main thread on Opus — so it carries `byModel` as well as the total.
- **Each plan already carried out** — props on the `Plan` node, `byModel` included: a plan picked
  up again on a different model is the case the split exists for. A plan is charged for the stretch
  of transcript that ran *under* it: from the `ExitPlanMode` that approved it until the next plan
  was approved, across every session that picked it up. The message that proposes a plan is
  charged to the work that argued for it, never to the plan it is asking for. Work before any plan
  is charged to no plan at all, so every token lands in exactly one bucket.
- **Each repository, and the whole workspace** — a `Usage` node annotating the repository, or the
  root directory. A workspace that is a single repository gets one node rather than two saying the
  same thing.

A session's whole cost goes to the repository its working directory sat in, and nowhere else.
Splitting it across the repositories it edited would need a ratio between an edit and a token, and
there isn't one. `sessionsElsewhere` counts the sessions that reached into a repository from
outside it, so the coarseness is visible instead of implied.

`sessionsSwept` and `tokensSwept` are the reconciliation. A session that edited nothing that
survives and approved no plan is swept off the canvas, but it still spent what it spent; those two
props are what a total has to shed to equal the sum of the `Session` nodes you can actually see.

Nothing here is a spore. A spore that read `~/.claude` would need the `fs-read` capability, which
is deliberately vocabulary rather than a grant — see `docs/marketplace.md`. These are engine
producers, like the walker and `git`, and they follow `history.claude` in the workspace config.

`cargo run -p aneural-engine --example tally -- <dir>` prints the three tables from a terminal.
