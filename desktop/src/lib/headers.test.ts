import { describe, expect, it } from "vitest";
import { headersToSend, sameHeaders, splitHeaderLine, withLoginHint } from "./headers";

describe("headersToSend", () => {
  it("trims headers and skips blank rows", () => {
    expect(
      headersToSend([
        { name: " Authorization ", value: " Bearer abc " },
        { name: "", value: "" },
        { name: "  ", value: " " },
        { name: "X-Empty", value: "" },
      ]),
    ).toEqual({
      headers: [
        { name: "Authorization", value: "Bearer abc" },
        { name: "X-Empty", value: "" },
      ],
    });
  });

  it("refuses a value without a name, without repeating the value", () => {
    const result = headersToSend([{ name: "", value: "secret" }]);
    expect(result).toHaveProperty("error");
    expect(JSON.stringify(result)).not.toContain("secret");
  });
});

describe("splitHeaderLine", () => {
  it("splits a pasted header line", () => {
    expect(splitHeaderLine("Authorization: Bearer a:b")).toEqual({ name: "Authorization", value: "Bearer a:b" });
    expect(splitHeaderLine("X-API-Key:123")).toEqual({ name: "X-API-Key", value: "123" });
  });

  it("leaves text that isn't a header line alone", () => {
    expect(splitHeaderLine("Authorization")).toBeNull();
    expect(splitHeaderLine(": value")).toBeNull();
    expect(splitHeaderLine("Bearer abc: def")).toBeNull();
  });
});

describe("withLoginHint", () => {
  it("suggests headers only for a login error without them", () => {
    expect(withLoginHint("The server returned HTTP 401", false)).toContain("add it under Headers");
    expect(withLoginHint("The server returned HTTP 403", false)).toContain("add it under Headers");
    expect(withLoginHint("The server returned HTTP 403", true)).toBe("The server returned HTTP 403");
    expect(withLoginHint("The server returned HTTP 404", false)).toBe("The server returned HTTP 404");
  });
});

describe("sameHeaders", () => {
  it("compares names, values and order", () => {
    const a = [{ name: "A", value: "1" }];
    expect(sameHeaders(a, [{ name: "A", value: "1" }])).toBe(true);
    expect(sameHeaders(a, [{ name: "A", value: "2" }])).toBe(false);
    expect(sameHeaders(a, [])).toBe(false);
  });
});
