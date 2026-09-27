// Stub for node:https — happy-dom references it but a WebMCP harvest never
// needs the filesystem, a subprocess, or Node's own HTTP stack.
const handler = { get: (t, p) => { if (p === '__esModule') return true; return handler.fn; }, fn: function(){ throw new Error('node:https unavailable'); } };
export default new Proxy({}, handler);
