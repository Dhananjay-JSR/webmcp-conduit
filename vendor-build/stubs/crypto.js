function rand(a){ for (let i=0;i<a.length;i++) a[i] = (Math.random()*256)|0; return a; }
export const webcrypto = { getRandomValues: rand, randomUUID: () =>
  'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, c => {
    const r = Math.random()*16|0; return (c === 'x' ? r : (r&0x3|0x8)).toString(16); }),
  subtle: {} };
export const randomUUID = webcrypto.randomUUID;
export function getRandomValues(a){ return rand(a); }
export function createHash(){ return { update(){ return this; }, digest(){ return ''; } }; }
export default { webcrypto, randomUUID, getRandomValues, createHash };
