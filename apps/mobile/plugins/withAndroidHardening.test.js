const { test } = require('node:test');
const assert = require('node:assert/strict');
const { patchMainActivity } = require('./withAndroidHardening');

const TEMPLATE = [
  'class MainActivity : ReactActivity() {',
  '  override fun onCreate(savedInstanceState: Bundle?) {',
  '    setTheme(R.style.AppTheme)',
  '    super.onCreate(null)',
  '  }',
  '}',
].join('\n');

test('the touch filter lands right after super.onCreate, once', () => {
  const out = patchMainActivity(TEMPLATE);
  assert.match(out, /super\.onCreate\(null\)\n    window\.decorView\.filterTouchesWhenObscured = true\n  \}/);
  assert.equal(patchMainActivity(out), out);
});

test('a MainActivity without onCreate fails loudly', () => {
  assert.throws(() => patchMainActivity('class MainActivity {}'), /super\.onCreate/);
});
