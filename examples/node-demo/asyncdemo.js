async function fetchUser(id) {
  const row = { id, name: "ada" };
  return row;
}
async function main() {
  const user = await fetchUser(7);
  console.log("user=", user.name);
}
main();
