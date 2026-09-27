// The web platform layer QuickJS does not provide and happy-dom assumes.
//
// These are maintained, spec-tracking implementations rather than anything
// hand-written here. Writing them by hand is the same mistake as writing a
// DOM by hand, one layer down.
import {
  ReadableStream, WritableStream, TransformStream,
  ByteLengthQueuingStrategy, CountQueuingStrategy,
} from 'web-streams-polyfill/es5';
// core-js rather than whatwg-url: whatwg-url builds its interface objects
// through webidl2js, and the result is not constructible under QuickJS
// ("not a constructor"). core-js targets old engines and works here.
import 'core-js/web/url';
import 'core-js/web/url-search-params';
import structuredClone from '@ungap/structured-clone';

function def(name, value) {
  if (typeof globalThis[name] === 'undefined') {
    try {
      Object.defineProperty(globalThis, name, {
        value, writable: true, configurable: true, enumerable: false,
      });
    } catch (e) { /* read-only global; leave it */ }
  }
}

def('ReadableStream', ReadableStream);
def('WritableStream', WritableStream);
def('TransformStream', TransformStream);
def('ByteLengthQueuingStrategy', ByteLengthQueuingStrategy);
def('CountQueuingStrategy', CountQueuingStrategy);
def('structuredClone', structuredClone);
