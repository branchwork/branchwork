import { describe, expect, it } from "vitest";
import { HttpError } from "../api.js";
import { errorMessage, httpErrorBody } from "./error.js";

describe("errorMessage", () => {
  it("surfaces HttpError body.error when present", () => {
    const e = new HttpError(400, "Bad Request", { error: "folder_not_found" });
    expect(errorMessage(e)).toBe("folder_not_found");
  });

  it("falls back to <status> <statusText> when body has no error field", () => {
    const e = new HttpError(503, "Service Unavailable", { foo: "bar" });
    expect(errorMessage(e)).toBe("503 Service Unavailable");
  });

  it("falls back to <status> <statusText> when body is undefined", () => {
    const e = new HttpError(500, "Internal Server Error");
    expect(errorMessage(e)).toBe("500 Internal Server Error");
  });

  it("returns Error.message for plain Errors", () => {
    expect(errorMessage(new Error("boom"))).toBe("boom");
  });

  it("string-coerces non-Error throwables", () => {
    expect(errorMessage("oops")).toBe("oops");
    expect(errorMessage(42)).toBe("42");
    expect(errorMessage(null)).toBe("null");
  });
});

describe("httpErrorBody", () => {
  it("returns the typed body when e is an HttpError with a body", () => {
    const e = new HttpError(400, "Bad Request", { error: "x", extra: 1 });
    const body = httpErrorBody<{ error: string; extra: number }>(e);
    expect(body).toEqual({ error: "x", extra: 1 });
  });

  it("returns undefined when e is not an HttpError", () => {
    expect(httpErrorBody(new Error("boom"))).toBeUndefined();
    expect(httpErrorBody("string")).toBeUndefined();
  });

  it("returns undefined when body is missing", () => {
    expect(httpErrorBody(new HttpError(500, "x"))).toBeUndefined();
  });
});
