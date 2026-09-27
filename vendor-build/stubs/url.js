export const URL = globalThis.URL;
export const URLSearchParams = globalThis.URLSearchParams;
export function fileURLToPath(u){ return String(u).replace(/^file:\/\//, ''); }
export function pathToFileURL(p){ return { href: 'file://' + p }; }
export function format(u){ return String(u); }
export function parse(u){ try { return new globalThis.URL(u); } catch(e) { return {}; } }
export default { URL, URLSearchParams, fileURLToPath, pathToFileURL, format, parse };
