// Run with the release-please version bundled by the pinned GitHub Action.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const {execFileSync} = require('node:child_process');
const Ajv = require('ajv');
const toml = require('@iarna/toml');
const {Version} = require('release-please/build/src/version');
const {Generic} = require('release-please/build/src/updaters/generic');
const {GenericToml} = require('release-please/build/src/updaters/generic-toml');
const {DefaultVersioningStrategy} = require('release-please/build/src/versioning-strategies/default');

const root = path.resolve(__dirname, '..');
const config = JSON.parse(fs.readFileSync(path.join(root, 'release-please-config.json')));
const validate = new Ajv({strict: false, formats: {"uri-reference": true}}).compile(require('release-please/schemas/config.json'));
assert(validate(config), JSON.stringify(validate.errors));
const settings = config.packages['.'];
const strategy = new DefaultVersioningStrategy({
  bumpMinorPreMajor: settings['bump-minor-pre-major'],
  bumpPatchForMinorPreMajor: settings['bump-patch-for-minor-pre-major'],
});
for (const [type, breaking, expected] of [['fix', false, '0.1.1'], ['feat', false, '0.2.0'], ['feat', true, '0.2.0']]) {
  const next = strategy.bump(Version.parse('0.1.0'), [{type, breaking, notes: []}]);
  assert.equal(next.toString(), expected);
}
const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'vigil-release-test-'));
try {
  for (const file of ['Cargo.toml', 'Cargo.lock', 'packaging/vigil.spec', 'scripts/check-version.py']) {
    fs.mkdirSync(path.dirname(path.join(temp, file)), {recursive: true});
    fs.copyFileSync(path.join(root, file), path.join(temp, file));
  }
  fs.cpSync(path.join(root, 'crates'), path.join(temp, 'crates'), {recursive: true});
  const version = Version.parse('0.2.0');
  fs.writeFileSync(path.join(temp, settings['version-file']), `${version}\n`);
  for (const extra of settings['extra-files']) {
    const file = path.join(temp, extra.path);
    const updater = extra.type === 'toml'
      ? new GenericToml(extra.jsonpath, version)
      : new Generic({version});
    fs.writeFileSync(file, updater.updateContent(fs.readFileSync(file, 'utf8')));
  }
  const oldLock = toml.parse(fs.readFileSync(path.join(root, 'Cargo.lock'), 'utf8'));
  const newLock = toml.parse(fs.readFileSync(path.join(temp, 'Cargo.lock'), 'utf8'));
  assert.deepEqual(newLock.package.filter(p => p.source), oldLock.package.filter(p => p.source));
  const check = ['scripts/check-version.py', '--tag', 'v0.2.0'];
  execFileSync('python3', check, {cwd: temp});
  assert.throws(() => execFileSync('python3', ['scripts/check-version.py', '--tag', 'v9.0.0'], {cwd: temp, stdio: 'pipe'}));
  fs.writeFileSync(path.join(temp, 'VERSION'), '0.3.0\n');
  assert.throws(() => execFileSync('python3', check, {cwd: temp, stdio: 'pipe'}));
  console.log('Release config passed: bump rules, workspace inheritance, lockfile/spec updates, dependency preservation, and tag/version mismatch rejection.');
} finally {
  fs.rmSync(temp, {recursive: true, force: true});
}
