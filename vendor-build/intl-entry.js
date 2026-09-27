// QuickJS ships no Intl at all, so anything reaching for Intl.Segmenter dies
// on `cannot read property 'Segmenter' of undefined`. That is not exotic: a
// text editor uses it for grapheme and word boundaries, and it is what stops
// OpenAI's own Margin demo from booting.
//
// polyfill-force installs unconditionally, which is what we want — there is
// nothing here to feature-detect against.
if (typeof globalThis.Intl === 'undefined') {
  globalThis.Intl = {};
}
// Order matters: the Segmenter polyfill calls Intl.getCanonicalLocales at
// load time, so that has to exist first.
import '@formatjs/intl-getcanonicallocales/polyfill-force.js';
import '@formatjs/intl-segmenter/polyfill-force.js';
