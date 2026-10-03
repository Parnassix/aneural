import { execFileSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { beforeAll, describe, expect, it } from 'vitest';

const here = path.dirname(fileURLToPath(import.meta.url));
const pkg = path.resolve(here, '..');
const cli = path.join(pkg, 'dist', 'cli.js');

let root = '';
let registryDir = '';

function run(
  args: string[],
  opts: { expectFail?: boolean } = {},
): { stdout: string; stderr: string; code: number } {
  try {
    const stdout = execFileSync(process.execPath, [cli, ...args], {
      cwd: root,
      encoding: 'utf8',
      env: { ...process.env, NO_COLOR: '1' },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    return { stdout, stderr: '', code: 0 };
  } catch (e) {
    const err = e as { stdout?: string; stderr?: string; status?: number };
    if (!opts.expectFail) throw new Error(`cli failed (${err.status}): ${err.stderr}`);
    return { stdout: err.stdout ?? '', stderr: err.stderr ?? '', code: err.status ?? 1 };
  }
}

const sha256 = (s: string): string => crypto.createHash('sha256').update(s).digest('hex');

function manifest(id: string, kind: string, capabilities: unknown[] = []): string {
  const [publisher, name] = id.split('.');
  return JSON.stringify(
    {
      publisher,
      name,
      version: '1.0.0',
      displayName: `${kind} spore`,
      description: `Adds ${kind} nodes from markdown.`,
      capabilities,
      nodeTypes: [{ kind, icon: 'LuCircleDot', color: '#8fd1a0' }],
      harvesters: [
        {
          id: 'docs',
          kind: 'markdown',
          include: ['**/*.md'],
          emit: {
            node: { kind, id: `${id}.item:{file}`, label: '{title}' },
            edges: [],
          },
        },
      ],
    },
    null,
    2,
  );
}

/** A registry served straight off disk — no server, no network. */
function publish(id: string, kind: string, capabilities: unknown[] = []) {
  const body = manifest(id, kind, capabilities);
  const dir = path.join(registryDir, id);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, 'spore.json'), body);
  return {
    id,
    version: '1.0.0',
    displayName: `${kind} spore`,
    description: `Adds ${kind} nodes from markdown.`,
    repo: registryDir,
    path: id,
    files: { 'spore.json': sha256(body) },
    nodeKinds: [kind],
    capabilities,
  };
}

beforeAll(() => {
  if (!fs.existsSync(cli)) {
    execFileSync('pnpm', ['exec', 'tsdown'], { cwd: pkg, stdio: 'inherit' });
  }
  root = fs.mkdtempSync(path.join(os.tmpdir(), 'aneural-market-'));
  registryDir = path.join(root, 'registry');
  fs.mkdirSync(registryDir, { recursive: true });

  run(['init', '.']);

  const index = {
    version: 1,
    name: 'test',
    spores: [
      publish('acme.adr', 'Decision'),
      publish('acme.ci', 'Job', [{ kind: 'subprocess', commands: ['make'] }]),
    ],
    revoked: [{ id: 'bad.thing', versions: ['*'], reason: 'malicious' }],
  };
  const indexPath = path.join(registryDir, 'index.json');
  fs.writeFileSync(indexPath, JSON.stringify(index, null, 2));

  const configPath = path.join(root, '.aneural', 'config.json');
  const config = JSON.parse(fs.readFileSync(configPath, 'utf8'));
  config.spores.registries = [{ name: 'test', url: indexPath }];
  fs.writeFileSync(configPath, `${JSON.stringify(config, null, 2)}\n`);
});

describe('registry validation', () => {
  it('refuses a listing that advertises less than its manifest asks for', () => {
    const dir = path.join(root, 'badreg');
    fs.mkdirSync(path.join(dir, 'acme.sneaky'), { recursive: true });
    const body = manifest('acme.sneaky', 'Thing', [{ kind: 'http', hosts: ['evil.example'] }]);
    fs.writeFileSync(path.join(dir, 'acme.sneaky', 'spore.json'), body);
    fs.writeFileSync(
      path.join(dir, 'index.json'),
      JSON.stringify({
        version: 1,
        spores: [
          {
            id: 'acme.sneaky',
            version: '1.0.0',
            repo: '.',
            path: 'acme.sneaky',
            files: { 'spore.json': sha256(body) },
            capabilities: [],
          },
        ],
      }),
    );

    const { code, stdout } = run(['registry', path.join(dir, 'index.json')], { expectFail: true });
    expect(code).toBe(1);
    expect(stdout).toContain('may not advertise less than the spore asks for');
  });

  it('accepts a listing that matches', () => {
    const dir = path.join(root, 'goodreg');
    fs.mkdirSync(path.join(dir, 'acme.adr'), { recursive: true });
    const body = manifest('acme.adr', 'Decision');
    fs.writeFileSync(path.join(dir, 'acme.adr', 'spore.json'), body);
    fs.writeFileSync(
      path.join(dir, 'index.json'),
      JSON.stringify({
        version: 1,
        spores: [
          {
            id: 'acme.adr',
            version: '1.0.0',
            repo: '.',
            path: 'acme.adr',
            files: { 'spore.json': sha256(body) },
            capabilities: [],
          },
        ],
      }),
    );
    expect(run(['registry', path.join(dir, 'index.json')]).stdout).toContain('valid');
  });
});

describe('authoring a spore', () => {
  it('scaffolds, tests clean, and catches a change', () => {
    const dir = path.join(root, 'authoring');
    fs.mkdirSync(dir, { recursive: true });

    const created = run(['spores', 'init', dir, '--publisher', 'acme']).stdout;
    expect(created).toContain('acme.authoring');
    // AGENTS.md is what teaches a coding agent the rules before it trips them.
    const agents = fs.readFileSync(path.join(dir, 'AGENTS.md'), 'utf8');
    expect(agents).toContain('acme.authoring.');

    expect(run(['spores', 'test', dir]).stdout).toContain('match the snapshot');

    // A fixture change must fail the snapshot rather than pass silently.
    fs.appendFileSync(path.join(dir, 'fixtures', 'sample.md'), '\n@thing another one\n');
    const changed = run(['spores', 'test', dir], { expectFail: true });
    expect(changed.code).toBe(1);
    expect(changed.stdout).toContain('differs from fixtures/expected.json');

    expect(run(['spores', 'test', dir, '--update']).stdout).toContain('updated');
    expect(run(['spores', 'test', dir]).stdout).toContain('match the snapshot');
  });

  it('rejects a scaffold that squats a core id prefix', () => {
    const dir = path.join(root, 'squatter');
    fs.mkdirSync(dir, { recursive: true });
    run(['spores', 'init', dir, '--publisher', 'acme', '--name', 'squat']);

    const manifestPath = path.join(dir, 'spore.json');
    const m = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));
    m.harvesters[0].emit.node.id = 'file:{file}';
    fs.writeFileSync(manifestPath, JSON.stringify(m, null, 2));

    const { code, stdout } = run(['spores', 'validate', manifestPath], { expectFail: true });
    expect(code).toBe(1);
    expect(stdout).toContain('reserved');
    expect(stdout).toContain('acme.squat.');
  });
});

describe('spores marketplace', () => {
  it('searches the configured registry', () => {
    const { stdout } = run(['spores', 'search', 'adr']);
    expect(stdout).toContain('acme.adr');
    expect(stdout).not.toContain('acme.ci');
  });

  it('shows what a spore adds and what it may do', () => {
    const { stdout } = run(['spores', 'info', 'acme.adr']);
    expect(stdout).toContain('Decision');
    expect(stdout).toContain('It will be allowed to:');
    // A purely declarative spore needs no capabilities at all.
    expect(stdout).toContain('Nothing.');
    // Third-party code always carries the disclaimer.
    expect(stdout).toContain('without warranty');
  });

  it('refuses to install without consent when not a TTY', () => {
    const { code, stderr } = run(['spores', 'add', 'acme.adr'], { expectFail: true });
    expect(code).toBe(2);
    expect(stderr).toContain('--yes');
  });

  it('installs, enables, verifies and removes', () => {
    expect(run(['spores', 'add', 'acme.adr', '--yes']).stdout).toContain(
      'installed acme.adr@1.0.0',
    );

    const installed = path.join(root, '.aneural', 'spores', 'acme.adr', 'spore.json');
    expect(fs.existsSync(installed)).toBe(true);

    const lock = JSON.parse(fs.readFileSync(path.join(root, '.aneural', 'spores.lock'), 'utf8'));
    expect(lock.spores['acme.adr'].registry).toBe('test');
    expect(lock.spores['acme.adr'].tier).toBe('declarative');
    // Provenance records the URL the user configured, not a transport-relative
    // path, or the lockfile cannot say where the code actually came from.
    expect(lock.spores['acme.adr'].registryUrl).toBe(path.join(registryDir, 'index.json'));

    expect(run(['spores', 'verify']).stdout).toContain('ok');

    const list = JSON.parse(run(['spores', 'list', '--json']).stdout);
    expect(list.find((s: { id: string }) => s.id === 'acme.adr')?.enabled).toBe(true);

    run(['spores', 'disable', 'acme.adr']);
    const afterDisable = JSON.parse(run(['spores', 'list', '--json']).stdout);
    expect(afterDisable.find((s: { id: string }) => s.id === 'acme.adr')?.enabled).toBe(false);

    run(['spores', 'remove', 'acme.adr']);
    expect(fs.existsSync(installed)).toBe(false);
  });

  it('reports a file edited after install as drift', () => {
    run(['spores', 'add', 'acme.adr', '--yes']);
    const installed = path.join(root, '.aneural', 'spores', 'acme.adr', 'spore.json');
    fs.writeFileSync(installed, `${fs.readFileSync(installed, 'utf8')}\n`);

    const { stdout } = run(['spores', 'verify']);
    expect(stdout).toContain('modified');
    expect(stdout).toContain('acme.adr');
    run(['spores', 'remove', 'acme.adr']);
  });

  it('will not install a tier it has no runner for', () => {
    const { code, stderr } = run(['spores', 'add', 'acme.ci', '--yes'], { expectFail: true });
    expect(code).toBe(1);
    expect(stderr).toContain('native');
    expect(fs.existsSync(path.join(root, '.aneural', 'spores', 'acme.ci'))).toBe(false);
  });

  it('lists the shipped http spore with its tier and what it still needs', () => {
    const list = JSON.parse(run(['spores', 'list', '--json']).stdout);
    const gh = list.find((s: { id: string }) => s.id === 'aneural.github');
    expect(gh).toBeTruthy();
    expect(gh.tier).toBe('http');
    // The tier is derived from the capabilities, never declared.
    expect(gh.consentLines).toContain('make web requests to api.github.com');
    expect(gh.missingSettings).toEqual(['repo']);
  });

  it('records a setting and clears what the spore was missing', () => {
    run(['spores', 'set', 'aneural.github', 'repo', 'acme/widget']);

    const list = JSON.parse(run(['spores', 'list', '--json']).stdout);
    const gh = list.find((s: { id: string }) => s.id === 'aneural.github');
    expect(gh.missingSettings).toEqual([]);
    expect(gh.settings[0]).toMatchObject({ key: 'repo', value: 'acme/widget', required: true });

    // It lands in the workspace config, not in a hidden store.
    const config = JSON.parse(fs.readFileSync(path.join(root, '.aneural', 'config.json'), 'utf8'));
    expect(config.spores.settings['aneural.github'].repo).toBe('acme/widget');

    run(['spores', 'set', 'aneural.github', 'repo']);
    const after = JSON.parse(run(['spores', 'list', '--json']).stdout);
    expect(after.find((s: { id: string }) => s.id === 'aneural.github').missingSettings).toEqual([
      'repo',
    ]);
  });

  it('refuses to set a value on a spore that is not installed', () => {
    const { code, stderr } = run(['spores', 'set', 'acme.nope', 'x', 'y'], { expectFail: true });
    expect(code).toBe(1);
    expect(stderr).toContain('no installed spore');
  });

  it('reports what a web-reading spore is missing instead of fetching', () => {
    run(['spores', 'set', 'aneural.github', 'repo', 'acme/widget']);
    run(['spores', 'enable', 'aneural.github']);

    // No token in the environment: it must say so rather than send a request
    // without one. Nothing here touches the network.
    const { code, stderr } = run(['spores', 'refresh'], { expectFail: true });
    expect(code).toBe(1);
    expect(stderr).toContain('ANEURAL_SECRET_GITHUB_TOKEN');

    run(['spores', 'disable', 'aneural.github']);
    run(['spores', 'set', 'aneural.github', 'repo']);
  });

  it('says plainly when nothing reads a web api', () => {
    const { stdout } = run(['spores', 'refresh']);
    expect(stdout).toContain('no spores read a web API');
  });

  it('refuses a manifest asking for more than its listing declared', () => {
    // Republish the same id with a capability the index does not advertise.
    const body = manifest('acme.adr', 'Decision', [{ kind: 'http', hosts: ['evil.example'] }]);
    fs.writeFileSync(path.join(registryDir, 'acme.adr', 'spore.json'), body);
    const indexPath = path.join(registryDir, 'index.json');
    const index = JSON.parse(fs.readFileSync(indexPath, 'utf8'));
    index.spores[0].files['spore.json'] = sha256(body);
    fs.writeFileSync(indexPath, JSON.stringify(index, null, 2));

    const { code, stderr } = run(['spores', 'add', 'acme.adr', '--yes'], { expectFail: true });
    expect(code).toBe(1);
    expect(stderr).toContain('more than its listing');
  });
});
