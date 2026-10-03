# scripts spore

Finds the things in a workspace you can run, and the schedules declared for
them. Reads files; it never runs anything. Off by default — a repository built
around scripts grows a node per script, which is hundreds of them.

## What it emits

| Node | From |
|---|---|
| `Script` | a file with a shebang, or a file sitting directly in `scripts/` or `bin/` |
| `Schedule` | a crontab line, or a `cron:` entry in a GitHub Actions workflow |

Each hangs off the file it was found in with `REFERENCES` — `via: source` for a
script, `via: declared` for a schedule — so both grow beside that file rather
than floating.

## Two harvesters, one node

`shebang` and `script-directory` both emit `script:{file}`. That is deliberate:
a file can be found either way, and when it is found both ways the second
harvester fills in `interpreter` rather than being discarded. `found` records
which harvester saw it first.

`script-directory`'s pattern is `^`, which matches the first line of any file.
With `once` that is "emit one node per file this harvester's globs select" —
the globs are the whole rule, and the pattern is only there because a `regex`
harvester must have one.

## What it deliberately does not do

- **No command.** Discovering that `scripts/download_pubmed.py` exists does not
  say how to run it: `uv run python`, `python3` and `poetry run` are all
  plausible and only the project knows. The command lives in workspace config,
  not in a guess.
- **No cron promoted from prose.** A cron expression written in a sentence —
  "cron `0 4 * * *` picks them up the next morning" — is a note about intent,
  not a schedule. See below.
- **No launch agents.** The plists that matter are the *installed* ones, which
  live outside the workspace and so are out of reach of any spore.

## Reading a project's own conventions

Script naming, join keys and cadence tables are per-project, so they belong in a
workspace spore at `.aneural/spores/<name>/spore.json` rather than here. The
`table-row` markdown granularity exists for exactly this: a document that keeps
a table of sources and cadences becomes `Schedule` nodes with no code at all.

```json
{
  "publisher": "acme", "name": "cadences", "version": "0.1.0",
  "harvesters": [
    {
      "id": "cadence-index",
      "kind": "markdown",
      "include": ["**/Refresh Cadences.md"],
      "granularity": "table-row",
      "table": { "requires": ["source_id", "cadence"] },
      "emit": {
        "node": {
          "kind": "Schedule",
          "id": "acme.cadences.schedule:{file}#{slug(col.source_id)}",
          "label": "{col.source_id}",
          "props": {
            "cadence": "{col.cadence}",
            "lastRefreshed": "{col.last_refreshed}",
            "driftSignal": "{col.drift_signal}",
            "declaredBy": "cadence-index"
          }
        },
        "edges": [
          {
            "kind": "ANNOTATES",
            "src": "$node",
            "dst": "script:scripts/download_{col.source_id}.py",
            "props": { "via": "schedule" }
          }
        ]
      }
    }
  ]
}
```

The `kind` is the builtin `Schedule`, so the node is styled, filtered and
rendered like any other schedule — but the **id** lives in the spore's own
namespace. Id prefixes are owned: only a first-party spore may mint a bare
`schedule:` id. Pointing an *edge* at another producer's id, as `dst` does here,
is always allowed.

Column keys are the header text slugged with `_`: `last_refreshed`, not
`last-refreshed`. A template variable is matched by `[A-Za-z_][A-Za-z0-9_.]*`,
which admits `.` but not `-`, so a hyphenated key renders as the empty string
and says nothing about why.

A second harvester over a second table in the same file, emitting the same
id, adds its columns to the same node — so a cadence table and a
refresh-shape table describe one schedule between them.

## A cron written in prose is a hint

Harvest it into `cronHint`, never `cron`. In the vault this was written against,
one `notes` column holds around two hundred cron expressions, several cells hold
more than one, and some are explicitly proposals — *"Default cron suggestion if
scheduling independently: `0 5 * * 1`"*. A `cron` prop is a thing a scheduler may
act on; a `cronHint` is a thing a person promotes.
