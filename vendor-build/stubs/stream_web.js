export const ReadableStream = globalThis.ReadableStream || function(){};
export const WritableStream = globalThis.WritableStream || function(){};
export const TransformStream = globalThis.TransformStream || function(){};
export default { ReadableStream, WritableStream, TransformStream };
