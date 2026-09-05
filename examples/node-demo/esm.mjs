const lang = (process.argv[2] || "node").toUpperCase();
export function greet(name) {
  return `${lang} says hi to ${name}`;
}
const out = greet("ada");
console.log(out);
