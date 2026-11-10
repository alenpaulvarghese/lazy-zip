export interface Header {
  name: string;
  value: string;
}

/** The headers to send, trimmed and without blank rows, or why they can't be sent. */
export function headersToSend(rows: Header[]): { headers: Header[] } | { error: string } {
  const headers: Header[] = [];
  for (const row of rows) {
    const name = row.name.trim();
    const value = row.value.trim();
    if (!name && !value) continue;
    if (!name) return { error: "A header has a value but no name. Add a name or remove the row." };
    headers.push({ name, value });
  }
  return { headers };
}

/** Splits a pasted line like `Authorization: Bearer abc` into its name and value. */
export function splitHeaderLine(text: string): Header | null {
  const colon = text.indexOf(":");
  if (colon <= 0) return null;
  const name = text.slice(0, colon).trim();
  if (!name || /\s/.test(name)) return null;
  return { name, value: text.slice(colon + 1).trim() };
}

/** Points to the headers when a server without them asks for a login. */
export function withLoginHint(error: string, sentHeaders: boolean): string {
  if (sentHeaders || !/\bHTTP 40[13]\b/.test(error)) return error;
  return `${error}. If it needs a login or a token, add it under Headers.`;
}

export function sameHeaders(a: Header[], b: Header[]): boolean {
  return a.length === b.length && a.every((header, i) => header.name === b[i].name && header.value === b[i].value);
}
