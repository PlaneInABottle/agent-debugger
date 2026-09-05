function boom() {
  throw new Error("kaboom");
}
function main() {
  boom();
}
main();
