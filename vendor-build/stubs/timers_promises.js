const h = { get: () => function(){ throw new Error('node:timers/promises unavailable'); } };
export default new Proxy({}, h);
