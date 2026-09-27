const h = { get: () => function(){ throw new Error('node:querystring unavailable'); } };
export default new Proxy({}, h);
