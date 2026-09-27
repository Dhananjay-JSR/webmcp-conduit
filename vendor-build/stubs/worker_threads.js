const h = { get: () => function(){ throw new Error('node:worker_threads unavailable'); } };
export default new Proxy({}, h);
