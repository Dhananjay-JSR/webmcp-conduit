const t0 = Date.now();
export const performance = { now: () => Date.now() - t0, timeOrigin: t0,
  mark(){}, measure(){}, getEntries: () => [], getEntriesByName: () => [],
  getEntriesByType: () => [], clearMarks(){}, clearMeasures(){} };
export class PerformanceObserver { observe(){} disconnect(){} takeRecords(){ return []; } }
export class PerformanceEntry { constructor(){ this.name=''; this.entryType=''; this.startTime=0; this.duration=0; } }
export default { performance, PerformanceObserver, PerformanceEntry };
