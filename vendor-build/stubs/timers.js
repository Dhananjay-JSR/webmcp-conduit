const h = { get: () => function(){ throw new Error('node:timers unavailable'); } };
export default new Proxy({}, h);
