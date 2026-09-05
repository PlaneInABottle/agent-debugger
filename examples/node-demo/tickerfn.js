let tick = 0;
function onTick() {
  tick += 1;
}
setInterval(onTick, 200);
setTimeout(() => console.log("done"), 60000);
