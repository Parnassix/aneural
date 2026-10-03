import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { beforeAll, describe, expect, it } from 'vitest';

const here = path.dirname(fileURLToPath(import.meta.url));
const pkg = path.resolve(here, '..');
const cli = path.join(pkg, 'dist', 'cli.js');
const fixture = path.resolve(pkg, '../../fixtures/sample-workspace');
let root = '';

function run(
  args: string[],
  opts: { cwd?: string; expectFail?: boolean } = {},
): { stdout: string; code: number } {
  try {
    const stdout = execFileSync(process.execPath, [cli, ...args], {
      cwd: opts.cwd ?? root,
      encoding: 'utf8',
      env: { ...process.env, NO_COLOR: '1' },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    return { stdout, code: 0 };
  } catch (e) {
    const err = e as { stdout?: string; status?: number; stderr?: string };
    if (!opts.expectFail) throw new Error(`cli failed (${err.status}): ${err.stderr}`);
    return { stdout: err.stdout ?? '', code: err.status ?? 1 };
  }
}

beforeAll(() => {
  if (!fs.existsSync(cli)) {
    execFileSync('pnpm', ['exec', 'tsdown'], { cwd: pkg, stdio: 'inherit' });
  }
  root = fs.mkdtempSync(path.join(os.tmpdir(), 'aneural-cli-'));
  fs.cpSync(fixture, root, { recursive: true });
  fs.rmSync(path.join(root, '.aneural'), { recursive: true, force: true });
  fs.mkdirSync(path.join(root, 'apps/web/.git'), { recursive: true });
  root = fs.realpathSync(root);
});

describe('aneural cli', () => {
  it('init --claude creates .aneural and .mcp.json', () => {
    fs.writeFileSync(
      path.join(root, '.mcp.json'),
      JSON.stringify({ mcpServers: { other: { command: 'x' } } }),
    );
    const { stdout } = run(['init', root, '--claude', '--name', 'cli-test']);
    expect(stdout).toContain('created');
    expect(fs.existsSync(path.join(root, '.aneural/config.json'))).toBe(true);
    const mcp = JSON.parse(fs.readFileSync(path.join(root, '.mcp.json'), 'utf8')) as {
      mcpServers: Record<string, { command: string; args?: string[] }>;
    };
    expect(mcp.mcpServers.other?.command).toBe('x');
    expect(mcp.mcpServers.aneural?.args).toEqual(['mcp']);
    expect(run(['init', root], { expectFail: true }).code).not.toBe(0);
  });

  it('index --json reports stats', () => {
    const stats = JSON.parse(run(['index', '--json']).stdout) as {
      nodes: number;
      filesIndexed: number;
    };
    expect(stats.nodes).toBeGreaterThan(50);
    expect(stats.filesIndexed).toBeGreaterThan(20);
  });

  it('query finds nodes and neighbors lists edges', () => {
    const nodes = JSON.parse(run(['query', 'util', '--json']).stdout) as { id: string }[];
    expect(nodes.map((n) => n.id)).toContain('file:apps/web/src/lib/util.ts');
    const text = run(['query', 'util', '--kind', 'File'], {
      cwd: path.join(root, 'apps/web'),
    }).stdout;
    expect(text).toContain('file:apps/web/src/lib/util.ts');
    const nb = run(['neighbors', 'file:apps/web/src/index.ts', '--direction', 'out']).stdout;
    expect(nb).toContain('file:apps/web/src/app.ts');
  });

  it('focus set/show/clear round-trips', () => {
    expect(
      run(['focus', 'set', 'file:apps/web/src/index.ts', 'file:apps/web/src/app.ts']).stdout,
    ).toContain('focus');
    const shown = JSON.parse(run(['focus', 'show', '--json']).stdout) as {
      selection: { primary: string; pinned: string[] };
    };
    expect(shown.selection.primary).toBe('file:apps/web/src/index.ts');
    expect(shown.selection.pinned).toEqual(['file:apps/web/src/app.ts']);
    expect(run(['focus', 'set', 'file:nope.ts'], { expectFail: true }).code).not.toBe(0);
    run(['focus', 'clear']);
    expect(run(['focus']).stdout).toContain('no focus yet');
  });

  it('spores list/validate/remove and doctor', () => {
    expect(run(['spores', 'list']).stdout).toContain('comments');
    expect(
      run(['spores', 'validate', path.resolve(pkg, '../../spores/comments/spore.json')]).stdout,
    ).toContain('valid');
    const bad = path.join(root, 'bad-spore.json');
    fs.writeFileSync(bad, JSON.stringify({ name: 'Bad Name', version: 'x' }));
    expect(run(['spores', 'validate', bad], { expectFail: true }).code).toBe(1);
    run(['spores', 'remove', 'icebox']);
    const config = JSON.parse(fs.readFileSync(path.join(root, '.aneural/config.json'), 'utf8')) as {
      spores: { enabled: string[] };
    };
    expect(config.spores.enabled).not.toContain('icebox');
    expect(run(['doctor']).code).toBe(0);
  });
});
