import { readFile, rename, rm, mkdir, stat, writeFile } from "node:fs/promises";
import { randomUUID } from "node:crypto";
import { basename, dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { compile } from "json-schema-to-typescript";

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const schemaRoot = join(packageRoot, "schema");
const canonicalOutputRoot = join(packageRoot, "src", "generated");

const targets = [
  {
    schema: "workflow-catalog.schema.json",
    output: "workflow-catalog.ts",
    exports: ["WorkflowCatalog", "WorkflowCatalogEntry", "WorkflowCatalogInput", "WorkflowCatalogSource", "WorkflowCatalogDiagnostic"],
  },
  {
    schema: "command.schema.json",
    output: "commands.ts",
    // `CanaryMetrics` is deliberately absent: it stopped being a wire type when
    // `ObserveCanary` lost its payload and canary evidence became server-measured.
    exports: ["Command", "CommandBody", "PromotionAction"],
  },
  {
    schema: "session-event.schema.json",
    output: "events.ts",
    exports: ["Actor", "EventBody", "SessionEvent", "UsageCoverage"],
  },
  {
    schema: "payload.schema.json",
    output: "payload.ts",
    exports: ["FileMatchWire", "Payload", "UiPluginLifecycleStatus"],
  },
  {
    schema: "envelope.schema.json",
    output: "envelope.ts",
    exports: ["Envelope"],
  },
  {
    schema: "id-catalog.schema.json",
    output: "ids.ts",
    exports: ["IdCatalog"],
  },
  {
    schema: "session-catalog.schema.json",
    output: "session.ts",
    exports: [
      "EditorActionContext",
      "EditorNativeAction",
      "SessionDeepLink",
      "SessionDeletionMode",
      "SessionExportFormat",
      "SessionExportOptions",
      "SessionLifecycleAction",
      "SessionHistoryPage",
      "SessionSearchPage",
      "SessionSearchFilters",
      "SessionSearchQuery",
      "SessionSearchResult",
      "SessionSearchScope",
      "SessionSearchSource",
      "SessionSummary",
    ],
  },
  {
    schema: "inbox-catalog.schema.json",
    output: "inbox.ts",
    exports: [
      "InboxDeepLink",
      "InboxEntry",
      "InboxEntryKind",
      "InboxEntryState",
      "InboxListFilters",
      "InboxListQuery",
      "InboxMutation",
      "InboxPage",
      "InboxSource",
      "InboxSourceIdentity",
    ],
  },
  {
    schema: "analytics-catalog.schema.json",
    output: "analytics.ts",
    exports: [
      "AnalyticsBucket",
      "AnalyticsCompletion",
      "AnalyticsDimensionCoverage",
      "AnalyticsExportFormat",
      "AnalyticsExportRequest",
      "AnalyticsExportResult",
      "AnalyticsFilters",
      "AnalyticsGrouping",
      "AnalyticsMetrics",
      "AnalyticsPage",
      "AnalyticsQuery",
      "AnalyticsTimeRange",
      "MeasurementCoverage",
    ],
  },
  {
    schema: "automation-catalog.schema.json",
    output: "automation.ts",
    exports: [
      "AutomationApprovalMode",
      "AutomationBinding",
      "AutomationBindingDraft",
      "AutomationBindingPatch",
      "AutomationBindingPage",
      "AutomationBindingQuery",
      "AutomationBindingRequest",
      "BudgetCeiling",
      "ConcurrencyPolicy",
      "DeduplicationPolicy",
      "InvocationPolicy",
      "MissedRunPolicy",
      "TriggerFilters",
      "TriggerRetryPolicy",
      "TriggerSource",
      "WebhookSignatureScheme",
    ],
  },
  {
    schema: "bundle-catalog.schema.json",
    output: "bundle.ts",
    exports: [
      "BundleCollisionPolicy",
      "BundleEntryKind",
      "BundleEntryManifest",
      "BundleExportReceipt",
      "BundleExportRequest",
      "BundleIdentityKind",
      "BundleIdentityMapping",
      "BundleImportProvenance",
      "BundleImportReceipt",
      "BundleImportRequest",
      "BundleInclusionPolicy",
      "BundleManifest",
      "BundleRedactionPolicy",
      "BundleRedactionSummary",
    ],
  },
];

const jsonValueType =
  "type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };";

function outputDirectory() {
  const arguments_ = process.argv.slice(2);
  if (arguments_.length !== 2 || arguments_[0] !== "--output-dir") {
    throw new Error("usage: node scripts/generate.mjs --output-dir <directory>");
  }
  return isAbsolute(arguments_[1]) ? resolve(arguments_[1]) : resolve(packageRoot, arguments_[1]);
}

function selectExports(source, expectedExports, filename) {
  const expected = new Set(expectedExports);
  const found = new Set();
  const rendered = source.replace(/^export (interface|type) ([A-Za-z_$][\w$]*)/gm, (match, kind, name) => {
    if (!expected.has(name)) return `${kind} ${name}`;
    found.add(name);
    return match;
  });
  const missing = expectedExports.filter((name) => !found.has(name));
  if (missing.length > 0) {
    throw new Error(`${filename} did not generate expected exports: ${missing.join(", ")}`);
  }
  return rendered;
}

const annotationKeys = new Set(["$comment", "default", "deprecated", "description", "examples", "readOnly", "title", "writeOnly"]);
const schemaMapKeys = new Set(["$defs", "definitions", "dependentSchemas", "patternProperties", "properties"]);
const schemaArrayKeys = new Set(["allOf", "anyOf", "oneOf"]);
const schemaValueKeys = new Set([
  "additionalItems",
  "additionalProperties",
  "contains",
  "else",
  "if",
  "items",
  "not",
  "propertyNames",
  "then",
  "unevaluatedItems",
  "unevaluatedProperties",
]);

function normalizeUnconstrainedSchemas(value) {
  if (value === true) return { tsType: "JsonValue" };
  if (value === false) return value;
  if (value === null || typeof value !== "object" || Array.isArray(value)) return value;
  const entries = Object.entries(value);
  if (entries.every(([key]) => annotationKeys.has(key))) return { tsType: "JsonValue" };
  return Object.fromEntries(
    entries.map(([key, child]) => {
      if (schemaMapKeys.has(key) && child !== null && typeof child === "object" && !Array.isArray(child)) {
        return [
          key,
          Object.fromEntries(Object.entries(child).map(([name, schema]) => [name, normalizeUnconstrainedSchemas(schema)])),
        ];
      }
      if (schemaArrayKeys.has(key) && Array.isArray(child)) {
        return [key, child.map(normalizeUnconstrainedSchemas)];
      }
      if (schemaValueKeys.has(key)) {
        if (key === "additionalProperties" && child === true && value.properties) {
          return [key, child];
        }
        return [
          key,
          Array.isArray(child) ? child.map(normalizeUnconstrainedSchemas) : normalizeUnconstrainedSchemas(child),
        ];
      }
      return [key, child];
    }),
  );
}

function typeName(property) {
  if (!/^[a-z][a-z0-9_]*$/.test(property)) throw new Error(`cannot derive a type name from ${property}`);
  return property
    .split("_")
    .map((part) => `${part[0].toUpperCase()}${part.slice(1)}`)
    .join("");
}

async function exists(path) {
  try {
    await stat(path);
    return true;
  } catch (error) {
    if (error?.code === "ENOENT") return false;
    throw error;
  }
}

async function render(outputRoot) {
  await mkdir(outputRoot, { recursive: true });

  for (const target of targets) {
    const schema = normalizeUnconstrainedSchemas(JSON.parse(await readFile(join(schemaRoot, target.schema), "utf8")));
    const source = await compile(schema, schema.title, {
      additionalProperties: false,
      bannerComment:
        "/**\n * Generated from the authoritative Rust protocol schema.\n * Do not edit by hand; run `npm run generate`.\n */",
      cwd: schemaRoot,
      declareExternallyReferenced: true,
      enableConstEnums: false,
      format: true,
      strictIndexSignatures: true,
      style: {
        bracketSpacing: true,
        printWidth: 120,
        semi: true,
        singleQuote: false,
        tabWidth: 2,
        trailingComma: "all",
        useTabs: false,
      },
      unknownAny: true,
      unreachableDefinitions: false,
    });
    let rendered = selectExports(source, target.exports, target.output);
    if (target.output === "ids.ts") {
      const properties = Object.keys(schema.properties ?? {}).sort();
      rendered += `\n${properties
        .map((property) => `export type ${typeName(property)} = IdCatalog[${JSON.stringify(property)}];`)
        .join("\n")}\n\nexport ${jsonValueType}\n`;
    } else {
      rendered += `\n${jsonValueType}\n`;
    }
    await writeFile(join(outputRoot, target.output), rendered.replaceAll("\r\n", "\n"));
  }

  const index = targets
    .map(({ output }) => `export type * from ${JSON.stringify(`./${output.replace(/\.ts$/, ".js")}`)};`)
    .join("\n");
  await writeFile(join(outputRoot, "index.ts"), `${index}\n`);
}

async function generate(outputRoot) {
  const outputExists = await exists(outputRoot);
  if (outputExists && outputRoot !== canonicalOutputRoot) {
    throw new Error(`refusing to replace an existing non-canonical output directory: ${outputRoot}`);
  }
  const parent = dirname(outputRoot);
  await mkdir(parent, { recursive: true });
  const staging = join(parent, `.${basename(outputRoot)}.tmp-${randomUUID()}`);
  const backup = join(parent, `.${basename(outputRoot)}.backup-${randomUUID()}`);
  let preserveBackup = false;
  try {
    await render(staging);
    if (outputExists) await rename(outputRoot, backup);
    try {
      await rename(staging, outputRoot);
    } catch (installError) {
      if (outputExists) {
        try {
          await rename(backup, outputRoot);
        } catch (rollbackError) {
          preserveBackup = true;
          throw new AggregateError(
            [installError, rollbackError],
            `failed to install generated output and restore the previous output; backup preserved at ${backup}`,
          );
        }
      }
      throw installError;
    }
    if (outputExists) await rm(backup, { recursive: true });
  } finally {
    await rm(staging, { recursive: true, force: true });
    if (!preserveBackup) await rm(backup, { recursive: true, force: true });
  }
}

await generate(outputDirectory());
