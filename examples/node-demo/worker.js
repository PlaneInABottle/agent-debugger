const { Worker, isMainThread, workerData } = require('worker_threads');
if (isMainThread) {
  const w = new Worker(__filename, { workerData: { n: 21 } });
  w.on('message', (m) => console.log("main got", m));
} else {
  const result = workerData.n * 2;
  require('worker_threads').parentPort.postMessage(result);
}
