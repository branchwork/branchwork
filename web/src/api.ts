const BASE = "";

export class HttpError extends Error {
  constructor(public status: number, public statusText: string, public body?: unknown) {
    super(`${status} ${statusText}`);
  }
}

export async function fetchJson<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`${BASE}${path}`, {
    credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    ...init,
  });
  if (!res.ok) {
    let body: unknown;
    try {
      body = await res.json();
    } catch {
      // body was not JSON — fine, leave undefined
    }
    throw new HttpError(res.status, res.statusText, body);
  }
  return res.json() as Promise<T>;
}

export function postJson<T>(path: string, body: unknown): Promise<T> {
  return fetchJson<T>(path, {
    method: "POST",
    body: JSON.stringify(body),
  });
}

export function putJson<T>(path: string, body: unknown): Promise<T> {
  return fetchJson<T>(path, {
    method: "PUT",
    body: JSON.stringify(body),
  });
}

export function deleteJson<T>(path: string): Promise<T> {
  return fetchJson<T>(path, { method: "DELETE" });
}
