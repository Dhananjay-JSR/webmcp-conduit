// happy-dom uses node:vm to evaluate page scripts inside its Window. conduit
// executes page scripts itself through QuickJS, so this never runs.
export class Script { constructor(code){ this.code = code; } runInContext(){ return undefined; } runInNewContext(){ return undefined; } }
export function createContext(o){ return o; }
export function runInContext(){ return undefined; }
export default { Script, createContext, runInContext };
