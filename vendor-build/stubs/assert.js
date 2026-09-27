const h = { get: () => function(){ throw new Error('node:assert unavailable'); } };
export default new Proxy({}, h);
