// tr46 (via whatwg-url) uses punycode for IDNA. Hostnames in a WebMCP harvest
// are ASCII in practice, so this passes them through rather than pulling in a
// full IDNA implementation.
export function toASCII(s){ return String(s); }
export function toUnicode(s){ return String(s); }
export const ucs2 = {
  decode: s => Array.from(String(s)).map(c => c.codePointAt(0)),
  encode: a => String.fromCodePoint(...a),
};
export default { toASCII, toUnicode, ucs2 };
