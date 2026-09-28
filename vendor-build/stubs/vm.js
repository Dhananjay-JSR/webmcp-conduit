// happy-dom uses node:vm to make its Window the global object of a real VM
// context. conduit has no vm, and does not need one: host-post.js hoists the
// Window's properties onto globalThis so window, self and globalThis are the
// same object, which is what a browser gives you anyway.
//
// isContext returning true makes setupVMContext a no-op, which is the correct
// outcome here rather than a workaround — there is nothing left for it to do.
export function isContext() { return true; }
export function createContext(o) { return o; }
export function runInContext() { return undefined; }
export function runInNewContext() { return undefined; }
export class Script {
  constructor(code) { this.code = code; }
  runInContext() { return undefined; }
  runInNewContext() { return undefined; }
  runInThisContext() { return undefined; }
}
export default { isContext, createContext, runInContext, runInNewContext, Script };
