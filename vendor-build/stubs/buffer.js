export const Buffer = { from: () => ({}), isBuffer: () => false, alloc: () => ({}) };
export const Blob = globalThis.Blob || function Blob(){};
export const File = globalThis.File || function File(){};
export const atob = globalThis.atob || function(s){ return s; };
export const btoa = globalThis.btoa || function(s){ return s; };
export default { Buffer, Blob, File, atob, btoa };
