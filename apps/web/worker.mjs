// Cloudflare Worker in front of the static site in `dist/` (Workers Static Assets, binding
// `ASSETS`). It reproduces what `scripts/serve.mjs` does in local preview, so the deployed site
// answers the same way:
// - only GET and HEAD are served; anything else is 405 with `Allow: GET, HEAD`;
// - `/` asked for as Markdown (`Accept: text/markdown`, or `?agent=1`) returns `/index.md`, the
//   agent-readable page, and the answer varies on `Accept`;
// - everything else is the static file; directory paths resolve to their `index.html` and an
//   unknown path gets `404.html` with status 404 (both set in wrangler.jsonc);
// - every answer carries `x-content-type-options: nosniff` and the same referrer policy.
// No request data is logged or stored.

export const SECURITY_HEADERS = Object.freeze({
  "x-content-type-options": "nosniff",
  "referrer-policy": "strict-origin-when-cross-origin",
});

/** Does this request for `/` ask for the agent-readable Markdown page? */
export function wantsMarkdown(url, accept) {
  return url.pathname === "/" && (url.searchParams.get("agent") === "1" || (accept ?? "").includes("text/markdown"));
}

export default {
  async fetch(request, env) {
    if (request.method !== "GET" && request.method !== "HEAD") {
      return new Response(null, { status: 405, headers: { allow: "GET, HEAD", ...SECURITY_HEADERS } });
    }
    const url = new URL(request.url);
    const target = wantsMarkdown(url, request.headers.get("accept")) ? new Request(new URL("/index.md", url), request) : request;
    const asset = await env.ASSETS.fetch(target);
    const response = new Response(asset.body, asset);
    for (const [name, value] of Object.entries(SECURITY_HEADERS)) response.headers.set(name, value);
    if (url.pathname === "/") response.headers.append("vary", "Accept");
    return response;
  },
};
