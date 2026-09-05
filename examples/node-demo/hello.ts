type User = { name: string };
function greet(u: User): string {
  return `hi ${u.name}`;
}
console.log(greet({ name: "ada" }));
