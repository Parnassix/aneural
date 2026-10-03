#!/usr/bin/env node
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import * as core from '@aneural/core';
import { Command } from 'commander';
import { c, fail, json, table } from './output.js';
import * as scaffold from './scaffold.js';

const VERSION = '0.1.0';

const program = new Command()
  .name('aneural')
  .description('Aneural: a living map of everything you are building.')
  .version(VERSION)
  .option('--root <dir>', 'workspace root (default: nearest .aneural above the cwd)');

interface Globals {
  root?: string;
}

function resolveRoot(cmd: Command, required = true): string {
  const g = cmd.optsWithGlobals<Globals>();
  if (g.root) {
    const found = core.findWorkspace(path.resolve(g.root));
    if (found) return found;
    if (required) fail(`no .aneural workspace at or above ${g.root} (run \`aneural init\`)`);
    return path.resolve(g.root);
  }
  const found = core.findWorkspace(process.cwd());
  if (found) return found;
  if (required)
    fail('no .aneural workspace found above the current directory (run `aneural init`)');
  return process.cwd();
}

// The CLI on PATH, never `npx -y aneural`: until the package is published under
// a name this project owns, npx would fetch and run whatever anyone uploads as
// "aneural", from a file that is usually committed.
const MCP_SERVER_ENTRY = { command: 'aneural', args: ['mcp'] };

function writeMcpJson(dir: string): string {
  const file = path.join(dir, '.mcp.json');
  let existing: Record<string, unknown> = {};
  if (fs.existsSync(file)) {
    try {
      existing = JSON.parse(fs.readFileSync(file, 'utf8')) as Record<string, unknown>;
    } catch {
      fail(`${file} exists but is not valid JSON; fix or remove it first`);
    }
  }
  const servers = (existing.mcpServers ?? {}) as Record<string, unknown>;
  servers.aneural = MCP_SERVER_ENTRY;
  existing.mcpServers = servers;
  fs.writeFileSync(file, `${JSON.stringify(existing, null, 2)}\n`);
  return file;
}

// ---- init ------------------------------------------------------------------

program
  .command('init [dir]')
  .description('create a .aneural/ workspace directory')
  .option('--name <name>', 'workspace display name')
  .option('--force', 'overwrite an existing config.json')
  .option('--claude', 'also register the MCP server in <dir>/.mcp.json for Claude Code')
  .option('--codex', 'print the Codex config.toml snippet for the MCP server')
  .action(
    (
      dir: string | undefined,
      opts: { name?: string; force?: boolean; claude?: boolean; codex?: boolean },
    ) => {
      const target = path.resolve(dir ?? program.opts<Globals>().root ?? process.cwd());
      try {
        const info = core.initWorkspace(target, { name: opts.name, force: opts.force ?? false });
        process.stdout.write(
          `${c.green('created')} ${info.aneuralDir}  ${c.dim(`(workspace "${info.name}")`)}\n`,
        );
      } catch (e) {
        fail((e as Error).message);
      }
      if (opts.claude) {
        const file = writeMcpJson(target);
        process.stdout.write(`${c.green('wrote')} ${file}  ${c.dim('→ mcpServers.aneural')}\n`);
      }
      if (opts.codex) {
        process.stdout.write(
          `\nAdd to ~/.codex/config.toml:\n\n[mcp_servers.aneural]\ncommand = "aneural"\nargs = ["mcp"]\n\n`,
        );
      }
      process.stdout.write(`${c.dim('next:')} aneural index\n`);
    },
  );

// ---- index -----------------------------------------------------------------

program
  .command('index')
  .description('index the workspace (incremental)')
  .option('--full', 're-analyse every file')
  .option('--watch', 'keep running and apply changes live')
  .option('--json', 'machine-readable output')
  .action(async (opts: { full?: boolean; watch?: boolean; json?: boolean }, cmd: Command) => {
    const root = resolveRoot(cmd);
    if (opts.watch) {
      let printedStats = false;
      const handle = core.watch(root, (ev) => {
        if (ev.type === 'delta' && printedStats) {
          const d = ev.delta as {
            nodes: unknown[];
            edges: unknown[];
            removedNodeIds: unknown[];
            removedEdges: unknown[];
          };
          const removed = d.removedNodeIds.length + d.removedEdges.length;
          if (d.nodes.length + d.edges.length + removed > 0) {
            process.stdout.write(
              `${c.dim(new Date().toISOString())} ${c.green(`+${d.nodes.length} nodes`)} ${c.accent(`+${d.edges.length} edges`)}${removed ? ` ${c.yellow(`-${removed} removed`)}` : ''}\n`,
            );
          }
        } else if (ev.type === 'indexComplete') {
          printedStats = true;
          process.stdout.write(
            opts.json
              ? `${JSON.stringify(ev.stats)}\n`
              : `${formatStats(ev.stats as core.IndexStats)}\n`,
          );
        } else if (ev.type === 'watching') {
          process.stdout.write(`${c.accent('watching')} ${root} ${c.dim('(ctrl-c to stop)')}\n`);
        } else if (ev.type === 'error') {
          process.stderr.write(`${c.red('engine')} ${ev.message}\n`);
        }
      });
      const stop = (): void => {
        handle.stop();
        process.exit(0);
      };
      process.on('SIGINT', stop);
      process.on('SIGTERM', stop);
      await new Promise(() => {});
      return;
    }
    const stats = await core.indexWorkspace(root, { full: opts.full ?? false });
    process.stdout.write(opts.json ? `${json(stats)}\n` : `${formatStats(stats)}\n`);
  });

function formatStats(s: core.IndexStats): string {
  return `${c.green('indexed')} ${s.filesIndexed} files ${c.dim(`(${s.filesSkipped} unchanged)`)} → ${c.bold(String(s.nodes))} nodes, ${c.bold(String(s.edges))} edges${s.unresolved ? c.yellow(`, ${s.unresolved} unresolved imports`) : ''} ${c.dim(`in ${s.durationMs} ms`)}`;
}

// ---- query -----------------------------------------------------------------

program
  .command('query <text>')
  .description('search nodes by label, path or id')
  .option('-k, --kind <kinds...>', 'restrict to node kinds')
  .option('-n, --limit <n>', 'max results', '50')
  .option('--json', 'machine-readable output')
  .action(
    (text: string, opts: { kind?: string[]; limit: string; json?: boolean }, cmd: Command) => {
      const root = resolveRoot(cmd);
      const nodes = core.queryNodes(root, { text, kinds: opts.kind, limit: Number(opts.limit) });
      if (opts.json) {
        process.stdout.write(`${json(nodes)}\n`);
        return;
      }
      if (nodes.length === 0) {
        process.stdout.write(`${c.dim('no nodes match')} ${text}\n`);
        return;
      }
      process.stdout.write(
        `${table(
          nodes.map((n) => [n.kind, n.id, n.label]),
          ['kind', 'id', 'label'],
        )}\n`,
      );
    },
  );

// ---- neighbors -------------------------------------------------------------

program
  .command('neighbors <id>')
  .description("show a node's neighbourhood")
  .option('-d, --depth <n>', 'hops', '1')
  .option('--direction <dir>', 'in | out | both', 'both')
  .option('--json', 'machine-readable output')
  .action(
    (id: string, opts: { depth: string; direction: string; json?: boolean }, cmd: Command) => {
      const root = resolveRoot(cmd);
      const g = core.neighborhood(root, [id], {
        depth: Number(opts.depth),
        direction: opts.direction,
        limit: 500,
      });
      if (opts.json) {
        process.stdout.write(`${json(g)}\n`);
        return;
      }
      const labels = new Map(g.nodes.map((n) => [n.id, n.label]));
      process.stdout.write(`${c.bold(id)}\n`);
      for (const e of g.edges) {
        const other = e.src === id ? e.dst : e.src;
        const arrow = e.src === id ? '→' : '←';
        process.stdout.write(
          `  ${arrow} ${c.accent(e.kind.padEnd(11))} ${other} ${c.dim(labels.get(other) ?? '')}\n`,
        );
      }
      if (g.truncated) process.stdout.write(`${c.dim('(truncated)')}\n`);
    },
  );

// ---- focus -----------------------------------------------------------------

const focus = program
  .command('focus')
  .description('inspect or set the focus handed to MCP clients');

focus
  .command('show', { isDefault: true })
  .option('--json', 'machine-readable output')
  .action((opts: { json?: boolean }, cmd: Command) => {
    const root = resolveRoot(cmd);
    const f = core.readFocus(root);
    if (!f) {
      process.stdout.write(
        `${c.dim('no focus yet')} — select a node in the GUI or run \`aneural focus set <id>\`\n`,
      );
      return;
    }
    if (opts.json) {
      process.stdout.write(`${json(f)}\n`);
      return;
    }
    const sel = f.selection as { primary?: string; pinned?: string[] };
    const ids = [sel.primary, ...(sel.pinned ?? [])].filter((x): x is string => Boolean(x));
    const nodes = core.getNodes(root, ids);
    process.stdout.write(`${c.dim('updated')} ${f.updatedAt}\n`);
    process.stdout.write(`${c.bold('primary')} ${sel.primary ?? c.dim('none')}\n`);
    process.stdout.write(
      `${c.bold('pinned')}  ${(sel.pinned ?? []).join(', ') || c.dim('none')}\n`,
    );
    if (nodes.length) process.stdout.write(`${table(nodes.map((n) => [n.kind, n.id, n.label]))}\n`);
    process.stdout.write(
      `${c.bold('visible')} ${f.visibleNodeIds.length} nodes${f.notes ? `\n${c.bold('notes')}   ${f.notes}` : ''}\n`,
    );
  });

focus
  .command('set <ids...>')
  .description('set the primary (first id) and pinned (rest) selection')
  .action((ids: string[], _opts: unknown, cmd: Command) => {
    const root = resolveRoot(cmd);
    const [primary, ...pinned] = ids;
    const missing = ids.filter((id) => !core.getNode(root, id));
    if (missing.length) fail(`unknown node id(s): ${missing.join(', ')}`);
    const previous = core.readFocus(root);
    core.writeFocus(root, {
      version: 1,
      updatedAt: new Date().toISOString(),
      workspace: root,
      filters: previous?.filters ?? {},
      selection: { primary, pinned },
      neighborhood: previous?.neighborhood ?? { depth: 1, direction: 'both' },
      visibleNodeIds: ids,
      notes: previous?.notes ?? '',
    });
    process.stdout.write(
      `${c.green('focus')} ${primary}${pinned.length ? c.dim(` +${pinned.length} pinned`) : ''}\n`,
    );
  });

focus.command('clear').action((_opts: unknown, cmd: Command) => {
  const root = resolveRoot(cmd);
  const file = path.join(root, '.aneural', 'state', 'focus.json');
  fs.rmSync(file, { force: true });
  process.stdout.write(`${c.green('cleared')} ${file}\n`);
});

// ---- spores ----------------------------------------------------------------

const spores = program
  .command('spores')
  .description('browse, install and validate spores from the marketplace');

/// Mirrors `EnvSecrets::var_name` in `aneural-core`, so the CLI can print the
/// exact variable the engine will look for.
function envName(secret: string): string {
  let out = '';
  let prevLower = false;
  for (const ch of secret) {
    if (ch >= 'A' && ch <= 'Z' && prevLower) out += '_';
    out += /[a-zA-Z0-9]/.test(ch) ? ch.toUpperCase() : '_';
    prevLower = /[a-z0-9]/.test(ch);
  }
  return out;
}

function tierBadge(tier: string): string {
  // Anything above `declarative` runs with capabilities the user must grant.
  return tier === 'declarative' ? c.dim(tier) : c.yellow(tier);
}

spores
  .command('list', { isDefault: true })
  .option('--json', 'machine-readable output')
  .action((opts: { json?: boolean }, cmd: Command) => {
    const root = resolveRoot(cmd);
    const list = core.listSpores(root);
    if (opts.json) {
      process.stdout.write(`${json(list)}\n`);
      return;
    }
    process.stdout.write(
      `${table(
        list.map((s) => [
          s.enabled ? c.green('on') : c.dim('off'),
          s.id,
          s.version,
          s.tier === 'declarative' ? c.dim('—') : tierBadge(s.tier),
          s.nodeKinds.join(','),
          s.missingSettings?.length
            ? c.yellow(`needs ${s.missingSettings.join(', ')}`)
            : s.description,
        ]),
        ['', 'id', 'version', 'tier', 'kinds', 'description'],
      )}\n`,
    );
  });

spores
  .command('search [query...]')
  .description('search the configured registries')
  .option('--json', 'machine-readable output')
  .action((query: string[], opts: { json?: boolean }, cmd: Command) => {
    const root = resolveRoot(cmd);
    for (const s of core.registryRefresh(root, false)) {
      if (!s.ok) process.stderr.write(`${c.red('registry')} ${s.name}: ${s.error}\n`);
    }
    const hits = core.searchSpores(root, (query ?? []).join(' '));
    if (opts.json) {
      process.stdout.write(`${json(hits)}\n`);
      return;
    }
    if (hits.length === 0) {
      process.stdout.write(`${c.dim('nothing found')}\n`);
      return;
    }
    process.stdout.write(
      `${table(
        hits.map((h) => [
          h.id,
          h.version,
          tierBadge(h.tier),
          h.registry,
          h.firstParty ? c.green('first-party') : c.dim('third-party'),
          h.description,
        ]),
        ['id', 'version', 'tier', 'from', '', 'description'],
      )}\n`,
    );
  });

spores
  .command('info <id>')
  .description('what a spore would add, and what it would be allowed to do')
  .option('--json', 'machine-readable output')
  .action((id: string, opts: { json?: boolean }, cmd: Command) => {
    const root = resolveRoot(cmd);
    let plan: core.InstallPlan;
    try {
      plan = core.planInstallSpore(root, id);
    } catch (e) {
      fail((e as Error).message);
    }
    if (opts.json) {
      process.stdout.write(`${json(plan)}\n`);
      return;
    }
    process.stdout.write(renderPlan(plan));
  });

/// The same summary the GUI consent sheet shows. Keeping one renderer is what
/// keeps the CLI and the GUI honest about what the user agreed to.
function renderPlan(plan: core.InstallPlan): string {
  const lines: string[] = [];
  lines.push(`${c.bold(plan.displayName || plan.id)} ${c.dim(`${plan.id} · v${plan.version}`)}`);
  if (plan.description) lines.push(plan.description);
  lines.push('');
  lines.push(
    `${c.dim('from')}     ${plan.registry} · ${plan.repo}${
      plan.firstParty ? ` ${c.green('first-party')}` : ''
    }`,
  );
  lines.push(`${c.dim('tier')}     ${tierBadge(plan.tier)}`);
  if (plan.nodeKinds.length) lines.push(`${c.dim('adds')}     ${plan.nodeKinds.join(', ')}`);
  if (plan.previousVersion) {
    lines.push(`${c.dim('update')}   ${plan.previousVersion} → ${plan.version}`);
  }
  lines.push('');
  lines.push(c.bold('It will be allowed to:'));
  if (plan.consentLines.length === 0) {
    lines.push(`  ${c.dim('Nothing. It only reads files you already index.')}`);
  }
  for (const line of plan.consentLines) lines.push(`  · ${line}`);
  if (plan.settings?.length) {
    lines.push('');
    lines.push(c.bold('You will need to set:'));
    for (const setting of plan.settings) {
      const have = setting.value ? c.green(setting.value) : c.yellow('not set');
      const hint = setting.example ? c.dim(` e.g. ${setting.example}`) : '';
      lines.push(`  · ${setting.key}  ${have}${hint}`);
      if (setting.description) lines.push(`      ${c.dim(setting.description)}`);
    }
    lines.push(c.dim(`  aneural spores set ${plan.id} <key> <value>`));
  }
  if (plan.missingSecrets?.length) {
    lines.push('');
    lines.push(c.bold('Credentials it needs, which are not set:'));
    for (const name of plan.missingSecrets) {
      lines.push(`  · ${name}  ${c.dim(`export ANEURAL_SECRET_${envName(name)}=…`)}`);
    }
  }
  if (!plan.firstParty) {
    lines.push('');
    lines.push(c.yellow(core.marketplaceDisclaimer()));
  }
  return `${lines.join('\n')}\n`;
}

/// Ask before installing third-party code. A non-interactive run must never
/// consent on the user's behalf, so it exits rather than guessing.
async function confirmInstall(plan: core.InstallPlan, yes: boolean): Promise<void> {
  process.stdout.write(renderPlan(plan));
  if (yes) return;
  if (!process.stdin.isTTY) {
    process.stderr.write(
      `${c.red('error')} refusing to install without consent; re-run with --yes\n`,
    );
    process.exit(2);
  }
  const readline = await import('node:readline/promises');
  const rl = readline.createInterface({ input: process.stdin, output: process.stdout });
  const answer = (await rl.question('\nInstall? [y/N] ')).trim().toLowerCase();
  rl.close();
  if (answer !== 'y' && answer !== 'yes') {
    process.stdout.write(`${c.dim('cancelled')}\n`);
    process.exit(1);
  }
}

spores
  .command('add <id>')
  .description('install a spore from the marketplace')
  .option('-y, --yes', 'skip the consent prompt')
  .option('--no-enable', 'install without enabling')
  .action(async (id: string, opts: { yes?: boolean; enable?: boolean }, cmd: Command) => {
    const root = resolveRoot(cmd);
    try {
      const plan = core.planInstallSpore(root, id);
      if (plan.requiresConsent) await confirmInstall(plan, opts.yes ?? false);
      const out = core.installSpore(root, id, opts.enable !== false);
      process.stdout.write(`${c.green('installed')} ${out.id}@${out.version} → ${out.dir}\n`);
    } catch (e) {
      fail((e as Error).message);
    }
  });

spores
  .command('remove <id>')
  .description('uninstall a spore (or disable a builtin)')
  .action((id: string, _opts: unknown, cmd: Command) => {
    const root = resolveRoot(cmd);
    try {
      core.uninstallSpore(root, id);
    } catch (e) {
      fail((e as Error).message);
    }
    process.stdout.write(`${c.green('removed')} ${id}\n`);
  });

for (const [verb, on] of [
  ['enable', true],
  ['disable', false],
] as const) {
  spores
    .command(`${verb} <id>`)
    .description(`${verb} an installed spore`)
    .action((id: string, _opts: unknown, cmd: Command) => {
      const root = resolveRoot(cmd);
      try {
        core.setSporeEnabled(root, id, on);
      } catch (e) {
        fail((e as Error).message);
      }
      process.stdout.write(`${c.green(on ? 'enabled' : 'disabled')} ${id}\n`);
    });
}

spores
  .command('set <id> <key> [value]')
  .description('record a value a spore declared it needs (omit value to clear it)')
  .action((id: string, key: string, value: string | undefined, _o: unknown, cmd: Command) => {
    const root = resolveRoot(cmd);
    const known = core.listSpores(root).find((s) => s.id === id || s.name === id);
    if (!known) {
      process.stderr.write(`${c.red('error')} no installed spore \`${id}\`\n`);
      process.exit(1);
    }
    core.setSporeSetting(root, known.id, key, value ?? null);
    process.stdout.write(
      value === undefined
        ? `${c.dim('cleared')} ${known.id} ${key}\n`
        : `${c.green('set')} ${known.id} ${key} = ${value}\n`,
    );
    const still = core.listSpores(root).find((s) => s.id === known.id);
    if (still?.missingSettings?.length) {
      process.stdout.write(`${c.yellow('still needed')} ${still.missingSettings.join(', ')}\n`);
    }
  });

spores
  .command('refresh [id]')
  .description('re-fetch spores that read a web API')
  .option('--json', 'machine-readable output')
  .action((id: string | undefined, opts: { json?: boolean }, cmd: Command) => {
    const root = resolveRoot(cmd);
    const result = core.refreshSpores(root, id ?? null, true);
    if (opts.json) {
      process.stdout.write(`${json(result)}\n`);
      process.exit(result.problems.length ? 1 : 0);
    }
    for (const problem of result.problems) {
      process.stderr.write(`${c.yellow('warn')} ${problem}\n`);
    }
    if (result.harvesters === 0) {
      // Nothing ran: either there is nothing to run, or every harvester was
      // blocked — and the warnings above already said which.
      if (result.problems.length === 0) {
        process.stdout.write(`${c.dim('no spores read a web API')}\n`);
        return;
      }
      process.exit(1);
    }
    process.stdout.write(
      `${c.green('refreshed')} ${result.harvesters} harvester(s) · ` +
        `${result.nodes} nodes · ${result.edges} edges\n`,
    );
    if (result.problems.length) process.exit(1);
  });

spores
  .command('validate <path>')
  .description('check a spore.json manifest')
  .action((file: string) => {
    const problems = core.validateSpore(path.resolve(file));
    if (problems.length === 0) {
      process.stdout.write(`${c.green('valid')} ${file}\n`);
      return;
    }
    for (const p of problems) process.stdout.write(`${c.red('✗')} ${p}\n`);
    process.exit(1);
  });

spores
  .command('verify')
  .description('check installed spores against the lockfile')
  .option('--json', 'machine-readable output')
  .action((opts: { json?: boolean }, cmd: Command) => {
    const root = resolveRoot(cmd);
    const drift = core.verifySpores(root);
    if (opts.json) {
      process.stdout.write(`${json(drift)}\n`);
      return;
    }
    if (drift.length === 0) {
      process.stdout.write(`${c.green('ok')} every installed spore matches the lockfile\n`);
      return;
    }
    for (const d of drift) {
      const where = d.file ? ` (${d.file})` : '';
      process.stdout.write(`${c.yellow(d.kind)} ${d.id}${where}\n`);
    }
  });

spores
  .command('update')
  .description('re-install installed spores at the versions the registries now list')
  .option('-y, --yes', 'skip consent prompts')
  .action(async (opts: { yes?: boolean }, cmd: Command) => {
    const root = resolveRoot(cmd);
    const installed = core.listSpores(root).filter((s) => s.location === 'workspace');
    if (installed.length === 0) {
      process.stdout.write(`${c.dim('no workspace spores installed')}\n`);
      return;
    }
    for (const s of installed) {
      try {
        const plan = core.planInstallSpore(root, s.id);
        if (plan.previousVersion === plan.version) {
          process.stdout.write(`${c.dim('current')} ${s.id}@${s.version}\n`);
          continue;
        }
        // Only a spore asking for more than it already had re-prompts.
        if (plan.requiresConsent) await confirmInstall(plan, opts.yes ?? false);
        const out = core.installSpore(root, s.id, true);
        process.stdout.write(
          `${c.green('updated')} ${out.id} ${out.previousVersion ?? '?'} → ${out.version}\n`,
        );
      } catch (e) {
        process.stderr.write(`${c.red('failed')} ${s.id}: ${(e as Error).message}\n`);
      }
    }
  });

spores
  .command('init [dir]')
  .description('scaffold a publishable spore repo')
  .requiredOption('-p, --publisher <name>', 'your marketplace publisher id (kebab-case)')
  .option('-n, --name <name>', 'spore name (kebab-case); defaults to the directory name')
  .option('--force', 'overwrite existing files')
  .action(
    (dir: string | undefined, opts: { publisher: string; name?: string; force?: boolean }) => {
      const target = path.resolve(dir ?? '.');
      const name = opts.name ?? path.basename(target);
      const kebab = /^[a-z0-9]+(-[a-z0-9]+)*$/;
      if (!kebab.test(opts.publisher)) fail(`publisher \`${opts.publisher}\` must be kebab-case`);
      if (!kebab.test(name)) fail(`name \`${name}\` must be kebab-case`);

      const spec = { publisher: opts.publisher, name };
      const files: Record<string, string> = {
        'spore.json': scaffold.manifest(spec),
        'README.md': scaffold.readme(spec),
        'AGENTS.md': scaffold.agents(spec),
        'fixtures/sample.md': scaffold.fixture(),
        '.github/workflows/validate.yml': scaffold.workflow(),
      };

      for (const [rel, body] of Object.entries(files)) {
        const out = path.join(target, rel);
        if (fs.existsSync(out) && !opts.force) fail(`${rel} already exists (use --force)`);
        fs.mkdirSync(path.dirname(out), { recursive: true });
        fs.writeFileSync(out, body);
      }

      // Seed the snapshot so the first `spores test` is a real comparison.
      writeSnapshot(
        target,
        core.testSpore(path.join(target, 'spore.json'), path.join(target, 'fixtures')),
      );

      process.stdout.write(`${c.green('created')} ${opts.publisher}.${name} in ${target}\n`);
      for (const rel of Object.keys(files)) process.stdout.write(`  ${c.dim(rel)}\n`);
      process.stdout.write(`  ${c.dim('fixtures/expected.json')}\n`);
      process.stdout.write(`\nnext: ${c.bold(`aneural spores test ${dir ?? '.'}`)}\n`);
    },
  );

interface Snapshot {
  nodes: unknown[];
  edges: unknown[];
}

function snapshotPath(dir: string): string {
  return path.join(dir, 'fixtures', 'expected.json');
}

function writeSnapshot(dir: string, result: Snapshot): void {
  fs.writeFileSync(snapshotPath(dir), `${JSON.stringify(result, null, 2)}\n`);
}

spores
  .command('test [dir]')
  .description('run a spore over its fixtures and diff the snapshot')
  .option('-u, --update', 'accept the current output as the snapshot')
  .action((dir: string | undefined, opts: { update?: boolean }) => {
    const target = path.resolve(dir ?? '.');
    const manifestFile = path.join(target, 'spore.json');
    if (!fs.existsSync(manifestFile)) fail(`no spore.json in ${target}`);

    const problems = core.validateSpore(manifestFile);
    if (problems.length) {
      for (const p of problems) process.stdout.write(`${c.red('✗')} ${p}\n`);
      process.exit(1);
    }

    const fixtures = path.join(target, 'fixtures');
    if (!fs.existsSync(fixtures)) fail(`no fixtures/ in ${target}`);
    const actual = core.testSpore(manifestFile, fixtures) as Snapshot;

    if (opts.update) {
      writeSnapshot(target, actual);
      process.stdout.write(
        `${c.green('updated')} ${actual.nodes.length} node(s), ${actual.edges.length} edge(s)\n`,
      );
      return;
    }

    const snapshot = snapshotPath(target);
    if (!fs.existsSync(snapshot)) {
      fail(`no fixtures/expected.json; run with --update to create it`);
    }
    const expected = JSON.parse(fs.readFileSync(snapshot, 'utf8')) as Snapshot;

    const a = JSON.stringify(actual, null, 2);
    const b = JSON.stringify(expected, null, 2);
    if (a === b) {
      process.stdout.write(
        `${c.green('ok')} ${actual.nodes.length} node(s), ${actual.edges.length} edge(s) match the snapshot\n`,
      );
      return;
    }

    process.stdout.write(`${c.red('changed')} output differs from fixtures/expected.json\n\n`);
    for (const line of diff(b, a)) process.stdout.write(`${line}\n`);
    process.stdout.write(`\n${c.dim('re-run with --update once this is what you meant')}\n`);
    process.exit(1);
  });

/** Line diff, just enough to read a snapshot change without a dependency. */
function diff(before: string, after: string): string[] {
  const b = before.split('\n');
  const a = after.split('\n');
  const out: string[] = [];
  let i = 0;
  let j = 0;
  while (i < b.length || j < a.length) {
    if (i < b.length && j < a.length && b[i] === a[j]) {
      i++;
      j++;
      continue;
    }
    // Resynchronise on the next line that matches, so one insertion does not
    // render the whole rest of the file as changed.
    const resync = a.indexOf(b[i] ?? '\u0000', j);
    if (i < b.length && resync === -1) {
      out.push(c.red(`- ${b[i]}`));
      i++;
    } else {
      while (j < resync) {
        out.push(c.green(`+ ${a[j]}`));
        j++;
      }
      if (i < b.length) {
        i++;
        j++;
      } else {
        out.push(c.green(`+ ${a[j]}`));
        j++;
      }
    }
    if (out.length > 60) {
      out.push(c.dim('  …'));
      break;
    }
  }
  return out;
}

spores
  .command('migrate')
  .description('bring a pre-marketplace .aneural/config.json up to date')
  .option('--write', 'apply the changes (default: show them)')
  .action((opts: { write?: boolean }, cmd: Command) => {
    const root = resolveRoot(cmd);
    const changes = core.migrateSporesConfig(root, opts.write ?? false);
    if (changes.length === 0) {
      process.stdout.write(`${c.green('ok')} config is already up to date\n`);
      return;
    }
    for (const change of changes) process.stdout.write(`  ${change}\n`);
    process.stdout.write(
      opts.write
        ? `${c.green('migrated')} ${changes.length} change(s)\n`
        : `${c.dim(`${changes.length} change(s); re-run with --write to apply`)}\n`,
    );
  });

// ---- registry --------------------------------------------------------------

program
  .command('registry')
  .argument('<index>', 'path to a registry index.json')
  .description('validate a registry index (what registry CI runs on a submission)')
  .option('--json', 'machine-readable output')
  .action((index: string, opts: { json?: boolean }) => {
    let problems: string[];
    try {
      problems = core.validateRegistryIndex(path.resolve(index));
    } catch (e) {
      fail((e as Error).message);
    }
    if (opts.json) {
      process.stdout.write(`${json(problems)}\n`);
      if (problems.length) process.exit(1);
      return;
    }
    if (problems.length === 0) {
      process.stdout.write(`${c.green('valid')} ${index}\n`);
      return;
    }
    for (const p of problems) process.stdout.write(`${c.red('✗')} ${p}\n`);
    process.exit(1);
  });

// ---- mcp -------------------------------------------------------------------

program
  .command('mcp')
  .description('serve the focused graph over MCP (stdio)')
  .action(async (_opts: unknown, cmd: Command) => {
    const root = resolveRoot(cmd);
    const { serveAneuralStdio } = await import('@aneural/mcp');
    const handle = await serveAneuralStdio({ root });
    const shutdown = (): void => {
      handle.close().finally(() => process.exit(0));
    };
    process.on('SIGINT', shutdown);
    process.on('SIGTERM', shutdown);
  });

// ---- doctor ----------------------------------------------------------------

program
  .command('doctor')
  .description('check the workspace, spores, icons and unresolved imports')
  .option('--json', 'machine-readable output')
  .action((opts: { json?: boolean }, cmd: Command) => {
    const root = resolveRoot(cmd);
    const diags = core.doctor(root);
    if (opts.json) {
      process.stdout.write(`${json(diags)}\n`);
    } else if (diags.length === 0) {
      process.stdout.write(`${c.green('healthy')} nothing to report\n`);
    } else {
      const paint: Record<string, (s: string) => string> = {
        error: c.red,
        warning: c.yellow,
        info: c.dim,
      };
      for (const level of ['error', 'warning', 'info']) {
        const rows = diags.filter((d) => d.level === level);
        if (rows.length === 0) continue;
        process.stdout.write(`${(paint[level] ?? c.dim)(`${level} (${rows.length})`)}\n`);
        for (const d of rows)
          process.stdout.write(
            `  ${c.dim(d.category.padEnd(10))} ${d.message}${d.path ? c.dim(`  ${d.path}`) : ''}\n`,
          );
      }
    }
    if (diags.some((d) => d.level === 'error')) process.exit(1);
  });

// ---- open ------------------------------------------------------------------

program
  .command('open')
  .description('open the workspace in the Aneural GUI')
  .action((_opts: unknown, cmd: Command) => {
    const root = resolveRoot(cmd);
    const child = spawn('aneural-gui', [root], { stdio: 'ignore', detached: true });
    child.on('error', () => {
      process.stdout.write(
        `${c.yellow('aneural-gui not found on PATH')}\nrun it from the repo: cargo run -p aneural-gui -- ${root}\n`,
      );
    });
    child.on('spawn', () => {
      child.unref();
      process.stdout.write(`${c.green('opened')} ${root}\n`);
    });
  });

program.parseAsync(process.argv).catch((e: Error) => fail(e.message));
