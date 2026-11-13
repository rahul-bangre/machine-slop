const [lane, ...parts] = Deno.args;
if (!lane || !parts.length) {
    throw new Error("usage: vm.ts <lane> <shell command>");
}
const response = await fetch("http://127.0.0.1:4300/exec", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ lane, command: parts.join(" ") }),
});
const result = await response.json();
console.log(result.output ?? result.error);
if (result.exit_code !== 0 || result.error) Deno.exit(1);
