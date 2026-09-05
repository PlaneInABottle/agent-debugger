function add(a, b) {
  const sum = a + b;
  return sum;
}
function cartTotal(items) {
  let total = 0;
  for (const it of items) {
    total += add(it.price, 0);
  }
  return total;
}
function main() {
  const items = [{ name: "apple", price: 3 }, { name: "bread", price: 2 }];
  const total = cartTotal(items);
  console.log("total=", total);
  if (window.location.search.includes("boom")) {
    null.missing();
  }
}
main();
