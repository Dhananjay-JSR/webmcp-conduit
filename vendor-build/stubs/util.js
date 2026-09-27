const t0 = Date.now();
export class TextEncoder {
  encode(s){ s = String(s); const a = new Uint8Array(s.length);
    for (let i=0;i<s.length;i++) a[i] = s.charCodeAt(i) & 255; return a; }
}
export class TextDecoder {
  decode(b){ if (!b) return ''; let o = '';
    for (let i=0;i<b.length;i++) o += String.fromCharCode(b[i]); return o; }
}
export function inherits(){}
export function promisify(f){ return f; }
export const types = { isTypedArray: () => false, isDate: v => v instanceof Date };
export function inspect(v){ try { return JSON.stringify(v); } catch(e) { return String(v); } }
export function format(...a){ return a.map(String).join(' '); }
export default { TextEncoder, TextDecoder, inherits, promisify, types, inspect, format };
