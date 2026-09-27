const h = { get: () => function(){ throw new Error('node:events unavailable'); } };
export default new Proxy({}, h);
