/**
 * Expo config plugin (security review M4 / M8), applied at prebuild:
 *  - the whole app window drops touches while another window covers it
 *    (tapjacking: an overlay hiding the Approve button), via the decor view;
 *  - MainActivity gets an empty taskAffinity, so another app's task cannot be
 *    stacked into ours.
 */
const { withAndroidManifest, withMainActivity, AndroidConfig } = require('expo/config-plugins');

const LINE = 'window.decorView.filterTouchesWhenObscured = true';

/** Inserts the filter right after MainActivity's `super.onCreate(...)`; idempotent. */
function patchMainActivity(src) {
  if (src.includes(LINE)) return src;
  const re = /^([ \t]*)super\.onCreate\([^)]*\)[ \t]*$/m;
  if (!re.test(src)) throw new Error('withAndroidHardening: no super.onCreate(...) in MainActivity');
  return src.replace(re, (m, indent) => `${m}\n${indent}${LINE}`);
}

const withAndroidHardening = (config) => {
  config = withMainActivity(config, (c) => {
    if (c.modResults.language !== 'kt') {
      throw new Error('withAndroidHardening: MainActivity is not Kotlin');
    }
    c.modResults.contents = patchMainActivity(c.modResults.contents);
    return c;
  });
  return withAndroidManifest(config, (c) => {
    AndroidConfig.Manifest.getMainActivityOrThrow(c.modResults).$['android:taskAffinity'] = '';
    return c;
  });
};

module.exports = withAndroidHardening;
module.exports.patchMainActivity = patchMainActivity;
