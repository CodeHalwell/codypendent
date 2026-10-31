import { describe, expect, it } from "vitest";
import { parseWorkflowInputs, workflowIntent, type WorkflowCatalogEntry } from "../src/components/workflowInputs.js";

const definition = (inputs: WorkflowCatalogEntry["inputs"]): WorkflowCatalogEntry => ({
  id: "review", version: 1, definition_digest: "digest", node_count: 2, inputs, source: { type: "BuiltIn" },
});

describe("workflow input preparation", () => {
  it("omits blank optional inputs while preserving explicit null and domain values", () => {
    const prepared = parseWorkflowInputs(definition({
      optional: { input_type: "RepositorySummary", required: false },
      explicit: { input_type: "ReviewContext", required: true },
      value: { input_type: "UserGoal", required: true },
    }), new Map([["optional", " \n "], ["explicit", "null"], ["value", '"Keep the public API"']]));
    expect({ inputs: prepared.inputs, errors: [...prepared.errors] }).toEqual({ inputs: { explicit: null, value: "Keep the public API" }, errors: [] });
  });

  it("reports required blanks and malformed JSON without inventing a domain schema", () => {
    const prepared = parseWorkflowInputs(definition({
      missing: { input_type: "DomainObject", required: true },
      malformed: { input_type: "DomainObject", required: false },
      accepted: { input_type: "DomainObject", required: true },
    }), new Map([["malformed", "plain unquoted text"], ["accepted", "false"]]));
    expect([...prepared.errors.keys()]).toEqual(["missing", "malformed"]);
    expect(prepared.inputs).toEqual({ accepted: false });
  });

  it("treats prototype-like manifest keys as ordinary own inputs", () => {
    const inputs = Object.fromEntries(["__proto__", "constructor", "toString"].map(name => [name, { input_type: "Any", required: true }]));
    const prepared = parseWorkflowInputs(definition(inputs), new Map([["__proto__", '{"polluted":true}'], ["constructor", "null"], ["toString", '"safe"']]));
    expect(Object.keys(prepared.inputs)).toEqual(["__proto__", "constructor", "toString"]);
    expect(Object.getPrototypeOf(prepared.inputs)).toBe(Object.prototype);
    expect(Object.hasOwn(Object.prototype, "polluted")).toBe(false);
    expect(JSON.parse(JSON.stringify(prepared.inputs))["__proto__"]).toEqual({ polluted: true });
  });

  it("keeps one intent across JSON formatting but distinguishes definitions, workflows and values", () => {
    const original = workflowIntent("review", "one", JSON.parse('{"payload":{"b":2,"a":1}}'));
    expect(workflowIntent("review", "one", JSON.parse('{"payload": {"a": 1, "b": 2}}'))).toBe(original);
    expect(workflowIntent("review", "two", { payload: { a: 1, b: 2 } })).not.toBe(original);
    expect(workflowIntent("other", "one", { payload: { a: 1, b: 2 } })).not.toBe(original);
    expect(workflowIntent("review", "one", { payload: { a: 1, b: 3 } })).not.toBe(original);
  });

  it("refuses numeric overflow instead of silently sending null", () => {
    const prepared = parseWorkflowInputs(definition({ amount: { input_type: "Amount", required: true } }), new Map([["amount", "1e999"]]));
    expect(prepared.inputs).toEqual({});
    expect(prepared.errors.get("amount")).toContain("finite numbers");
  });

  it("caps raw JSON bytes before parsing, including multi-byte text below the character limit", () => {
    const prepared = parseWorkflowInputs(definition({ text: { input_type: "Text", required: true } }), new Map([["text", JSON.stringify("界".repeat(22_000))]]));
    expect(prepared.inputs).toEqual({});
    expect(prepared.errors.get("text")).toContain("65,536-byte");
  });

  it("enforces the total serialized input budget across individually valid fields", () => {
    const prepared = parseWorkflowInputs(definition({
      left: { input_type: "Text", required: true }, right: { input_type: "Text", required: true },
    }), new Map([["left", JSON.stringify("x".repeat(40_000))], ["right", JSON.stringify("y".repeat(40_000))]]));
    expect(prepared.errors.get("left")).toContain("Combined workflow inputs");
  });
});
