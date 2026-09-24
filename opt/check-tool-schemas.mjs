import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));
const tooling = fileURLToPath(new URL("./", import.meta.url));

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: root,
    stdio: "inherit",
    ...options,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
  return result.stdout;
}

run("npm", ["ci", "--ignore-scripts", "--no-audit", "--no-fund"], {
  cwd: tooling,
});
const { lintToolSchema } = await import("tool-schema");

// A supplied export also lets CI check fixtures without rebuilding Frances.
const exported = process.argv[2]
  ? readFileSync(process.argv[2], "utf8")
  : run("cargo", [
    "run",
    "--quiet",
    "-p",
    "frances",
    "--",
    "--export-tool-schemas",
  ], {
    stdio: ["ignore", "pipe", "inherit"],
    encoding: "utf8",
  });
const tools = JSON.parse(exported);
if (!Array.isArray(tools) || tools.length === 0) {
  throw new Error("Expected a nonempty array of exported tools");
}

// tool-schema 0.4.2 misses unconstrained schemas and arrays without items.
// These are legal JSON Schema, but OpenAI rejects them in strict mode.
function checkStrictTypes(schema, path, issues) {
  if (!schema || typeof schema !== "object" || Array.isArray(schema)) {
    issues.push({ path, message: "Strict mode requires a schema object" });
    return;
  }
  if (!schema.type && !schema.$ref && !schema.anyOf) {
    issues.push({ path, message: "Strict mode requires type, $ref, or anyOf" });
  }
  const types = Array.isArray(schema.type) ? schema.type : [schema.type];
  if (types.includes("array") && schema.items === undefined) {
    issues.push({ path, message: "Strict arrays require items" });
  }
  for (const key of ["properties", "$defs", "definitions"]) {
    for (const [name, child] of Object.entries(schema[key] ?? {})) {
      checkStrictTypes(child, `${path}/${key}/${name}`, issues);
    }
  }
  if (schema.items !== undefined) {
    checkStrictTypes(schema.items, `${path}/items`, issues);
  }
  for (const key of ["anyOf", "oneOf", "allOf"]) {
    for (const [index, child] of (schema[key] ?? []).entries()) {
      checkStrictTypes(child, `${path}/${key}/${index}`, issues);
    }
  }
}

let failures = 0;
for (const tool of tools) {
  if (
    tool.type !== "function" || !tool.name ||
    typeof tool.strict !== "boolean" || !tool.parameters
  ) {
    throw new Error("Invalid exported tool definition");
  }
  const target = tool.strict ? "openai-strict" : "openai";
  const { issues } = lintToolSchema(tool.parameters, { target });
  if (tool.strict) checkStrictTypes(tool.parameters, "", issues);
  for (const issue of issues) {
    console.error(
      `${tool.name} (${target}) ${issue.path || "/"}: ${issue.message}`,
    );
    failures++;
  }
}
if (failures) process.exit(1);
console.log(
  `Validated ${tools.length} tool schemas (${
    tools.filter((tool) => tool.strict).length
  } strict).`,
);
