// computer-mcp-bridge.js — written by openab-pty at every session spawn.
// Do not edit: the runtime owns this file and rewrites it on the next spawn.
//
// The `computer` MCP server, stdio to Streamable HTTP. The session's coding CLI
// launches this as a local server; every line on stdin is one JSON-RPC message,
// POSTed to $OPENAB_TOOLS_MCP_URL — the per-session loopback endpoint the
// runtime puts in this session's environment. The workspace mcp.json is shared
// by every session in the pod, which is exactly why the URL lives in the
// environment and not in the file: each session's own env routes it to its own
// session, and a rotated key needs no rewrite.

"use strict";

const endpoint = process.env.OPENAB_TOOLS_MCP_URL;
// A literal "${OPENAB_TOOLS_MCP_URL}" here means the CLI did not expand the
// server env — fail loudly instead of fetch-throwing on every request.
if (!endpoint || endpoint.includes("${")) {
  process.stderr.write(
    "computer-mcp-bridge: OPENAB_TOOLS_MCP_URL is unset or was not expanded; " +
      "the tools MCP only exists inside an openab-pty session with the tools " +
      "plane enabled, and kiro must expand env for this wiring to work\n"
  );
  process.exit(1);
}

// Streamable HTTP carries the server session id in a response header and wants
// it echoed on every later request; openab-pty does not issue one today, but a
// deployment that routes this endpoint differently may.
let sessionId = null;

function errorResult(id, message) {
  return { jsonrpc: "2.0", id, error: { code: -32000, message } };
}

// `data:` payload of each SSE event block; every one is a complete JSON-RPC
// message from the server.
function* sseData(body) {
  for (const block of body.split(/\r?\n\r?\n/)) {
    const data = block
      .split(/\r?\n/)
      .filter((line) => line.startsWith("data:"))
      .map((line) => line.slice(5).replace(/^ /, ""))
      .join("\n")
      .trim();
    if (data && data !== "[DONE]") yield data;
  }
}

function write(message) {
  process.stdout.write(message + "\n");
}

async function forward(line) {
  const headers = {
    "content-type": "application/json",
    accept: "application/json, text/event-stream",
  };
  if (sessionId) headers["mcp-session-id"] = sessionId;
  const response = await fetch(endpoint, { method: "POST", headers, body: line });
  const sid = response.headers.get("mcp-session-id");
  if (sid) sessionId = sid;
  // A notification's whole acknowledgement is the empty 2xx.
  if (response.status === 202 || response.status === 204) return [];
  const body = await response.text();
  if (!response.ok) return [{ httpError: response.status, body }];
  const type = response.headers.get("content-type") || "";
  if (type.includes("text/event-stream")) {
    return [...sseData(body)].map((data) => ({ data }));
  }
  return body.trim() ? [{ data: body }] : [];
}

async function handle(line) {
  let id = null;
  try {
    const parsed = JSON.parse(line);
    id = parsed && parsed.id !== undefined && parsed.id !== null ? parsed.id : null;
  } catch {
    // Not JSON: still forwarded; the endpoint's own parse-error answer applies.
  }
  try {
    for (const out of await forward(line)) {
      if (out.httpError !== undefined) {
        // A request with an id must get an answer or the client waits forever.
        if (id !== null) {
          write(
            JSON.stringify(
              errorResult(
                id,
                `computer-mcp-bridge: tools endpoint returned HTTP ${out.httpError}: ${String(
                  out.body
                ).slice(0, 200)}`
              )
            )
          );
        }
        continue;
      }
      // One JSON-RPC message must occupy exactly one stdout line — an SSE
      // block joined multi-line `data:` fields, so re-encode compactly rather
      // than relaying the raw text.
      try {
        write(JSON.stringify(JSON.parse(out.data)));
      } catch {
        write(out.data.replace(/\r?\n/g, " "));
      }
    }
  } catch (error) {
    if (id !== null) {
      write(JSON.stringify(errorResult(id, `computer-mcp-bridge: ${String(error)}`)));
    }
  }
}

let buffered = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffered += chunk;
  let at;
  while ((at = buffered.indexOf("\n")) >= 0) {
    const line = buffered.slice(0, at).trim();
    buffered = buffered.slice(at + 1);
    if (line) handle(line);
  }
});
process.stdin.on("end", () => process.exit(0));
