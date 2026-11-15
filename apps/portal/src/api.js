import { useEffect, useState } from "react";

export async function api(path, options = {}) {
  const response = await fetch(path, {
    cache: "no-store",
    signal: AbortSignal.timeout(18000),
    ...options,
  });
  const data = await response.json().catch(() => ({}));
  if (!response.ok) {
    const message =
      response.status === 429
        ? "Submission limit reached. Retry in " +
          (response.headers.get("Retry-After") || "a few") +
          " seconds."
        : data.message ||
          data.error ||
          "Service returned HTTP " + response.status;
    throw Object.assign(new Error(message), {
      status: response.status,
      code: data.error,
    });
  }
  return data;
}

export function post(path, body, key, token) {
  return api(path, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "Idempotency-Key": key,
      ...(token ? { Authorization: "Bearer " + token } : {}),
    },
    body: JSON.stringify(body),
  });
}

// A rolling report may retain its last successful result while a newer time
// window loads. Every other read is isolated by its exact URL.
export function useRead(
  path,
  revision = 0,
  interval = 0,
  displayKey = path,
  token,
) {
  const [state, setState] = useState({
    path: null,
    displayKey: null,
    data: null,
    error: "",
  });
  useEffect(() => {
    if (!path) return;
    const controller = new AbortController();
    let timer;
    async function read() {
      try {
        const data = await api(path, {
          headers: token ? { Authorization: "Bearer " + token } : {},
          signal: AbortSignal.any([
            controller.signal,
            AbortSignal.timeout(13000),
          ]),
        });
        if (controller.signal.aborted) return;
        setState({ path, displayKey, data, error: "" });
      } catch (err) {
        if (controller.signal.aborted) return;
        setState((old) => ({
          path,
          displayKey,
          data: old.displayKey === displayKey ? old.data : null,
          error: err.message,
        }));
      }
      if (interval) timer = setTimeout(read, interval);
    }
    read();
    return () => {
      controller.abort();
      clearTimeout(timer);
    };
  }, [path, displayKey, revision, interval, token]);
  if (path && state.displayKey === displayKey) {
    return state.path === path ? state : { ...state, error: "" };
  }
  return { path: null, data: null, error: "" };
}
