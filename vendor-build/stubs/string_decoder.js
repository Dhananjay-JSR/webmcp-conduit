const h = { get: () => function(){ throw new Error('node:string_decoder unavailable'); } };
export default new Proxy({}, h);
