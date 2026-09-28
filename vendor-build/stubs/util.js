// node:util, as far as happy-dom needs it.
//
// TextEncoder/TextDecoder deliberately re-export the real globals rather than
// defining anything: encoding.js installs a proper UTF-8 implementation before
// this loads, and a naive stand-in here would be hoisted over it and silently
// corrupt every non-ASCII character on the page.
export const TextEncoder = globalThis.TextEncoder;
export const TextDecoder = globalThis.TextDecoder;
export function inherits() {}
export function promisify(f) { return f; }
export const types = {
  isTypedArray: (v) => ArrayBuffer.isView(v),
  isDate: (v) => v instanceof Date,
};
export function inspect(v) {
  try { return JSON.stringify(v); } catch (e) { return String(v); }
}
export function format(...a) { return a.map(String).join(' '); }
export default { TextEncoder, TextDecoder, inherits, promisify, types, inspect, format };
