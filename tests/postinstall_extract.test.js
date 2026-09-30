// extractArchive tar-slip guard: only the `agent-debugger` member is
// extracted, so `../` or symlink members in a tampered archive never
// write outside the dest dir (and no live symlink is planted in it).
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { execSync } = require('node:child_process');

const postinstall = require('../scripts/postinstall.js');

function mktar(dir, members) {
  const src = path.join(dir, 'src');
  fs.mkdirSync(src, { recursive: true });
  fs.writeFileSync(path.join(src, 'agent-debugger'), 'BINARY');
  const archive = path.join(dir, 'pkg.tar.gz');
  execSync(`tar -czf "${archive}" -C "${src}" agent-debugger`, { stdio: 'ignore' });
  for (const m of members || []) m(archive, src);
  return archive;
}

test('extractArchive extracts the binary from a legit archive', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'adb-slip-'));
  try {
    const archive = mktar(dir);
    const dest = path.join(dir, 'dest');
    fs.mkdirSync(dest);
    postinstall.extractArchive(archive, dest);
    assert.equal(fs.readFileSync(path.join(dest, 'agent-debugger'), 'utf8'), 'BINARY');
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('extractArchive ignores ../ and symlink members (no escape, no planted link)', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'adb-slip-'));
  try {
    const outside = path.join(dir, 'outside');
    fs.mkdirSync(outside, { recursive: true });
    const archive = path.join(dir, 'pkg.tar.gz');
    execSync(
      `python3 - <<'PY'\n` +
      `import tarfile, io\n` +
      `p = ${JSON.stringify(archive)}\n` +
      `with tarfile.open(p, 'w:gz') as t:\n` +
      `    b = b'BINARY'\n` +
      `    bi = tarfile.TarInfo('agent-debugger'); bi.size = len(b); t.addfile(bi, io.BytesIO(b))\n` +
      `    e = b'EVIL'\n` +
      `    ei = tarfile.TarInfo('../evil-planted.txt'); ei.size = len(e); t.addfile(ei, io.BytesIO(e))\n` +
      `    li = tarfile.TarInfo('sub'); li.type = tarfile.SYMTYPE; li.linkname = ${JSON.stringify(outside)}; t.addfile(li)\n` +
      `PY`,
      { stdio: 'ignore' },
    );
    const dest = path.join(dir, 'dest');
    fs.mkdirSync(dest);
    postinstall.extractArchive(archive, dest);
    assert.equal(fs.readFileSync(path.join(dest, 'agent-debugger'), 'utf8'), 'BINARY');
    assert.ok(!fs.existsSync(path.join(dir, 'evil-planted.txt')), 'no ../ escape');
    assert.ok(!fs.existsSync(path.join(outside, 'evil-planted.txt')), 'no escape into link target');
    assert.ok(!fs.existsSync(path.join(dest, 'sub')), 'no live symlink planted in dest');
    assert.ok(!fs.existsSync(path.join(dest, 'evil-planted.txt')), 'no evil member in dest');
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
