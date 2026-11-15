import { defineConfig } from "vite";
import { fileURLToPath } from "node:url";
import {
  demoProxies,
  demoRoute,
  issueDemoEvidence,
  invocationRedirect,
  localIssuer,
  permittedProductDocsRequest,
  permittedStaffRequest,
} from "./product-proxy.js";

const mode = process.env.EKYC_PORTAL_MODE || "staff";
if (!["demo", "staff"].includes(mode))
  throw new Error("EKYC_PORTAL_MODE must be demo or staff");
const demo = mode === "demo";
const target = process.env.EKYC_API_URL || "http://127.0.0.1:8090";
const issuer = localIssuer(
  process.env.EKYC_ISSUER_URL || "http://127.0.0.1:9805",
);
function restrictPortalRequests(server) {
  server.middlewares.use((request, response, next) => {
    const url = request.url || "";
    if (url === "/portal-config") {
      response.writeHead(200, {
        "Content-Type": "application/json",
        "Cache-Control": "no-store",
      });
      return response.end(JSON.stringify({ mode }));
    }
    if (
      !demo &&
      (url.startsWith("/invocations/") || url.startsWith("/product-demo/"))
    )
      return response.writeHead(404).end("Unavailable in staff mode");
    if (url.startsWith("/invocations/")) {
      const location = invocationRedirect(
        url,
        process.env.EKYC_RESTATE_ADMIN_PORT,
      );
      if (!location || request.method !== "GET")
        return response.writeHead(404).end("Unknown invocation");
      return response
        .writeHead(302, { Location: location, "Cache-Control": "no-store" })
        .end();
    }
    const docs = url.startsWith("/docs-api/");
    const staff = url.startsWith("/staff-api/");
    if (!docs && !staff && !url.startsWith("/product-demo/")) return next();
    if (
      request.headers["sec-fetch-site"] === "cross-site" ||
      (request.headers.origin &&
        !["http://", "https://"].some(
          (scheme) => request.headers.origin === scheme + request.headers.host,
        ))
    )
      return response.writeHead(403).end("Cross-origin portal request");
    if (staff)
      return permittedStaffRequest(
        request.method,
        url.slice("/staff-api".length),
      )
        ? next()
        : response.writeHead(404).end("Unknown staff route");
    const route = docs ? null : demoRoute(request.method, url);
    if (docs ? !permittedProductDocsRequest(request.method, url) : !route)
      return response.writeHead(404).end("Unknown API route");
    if (route?.product === "issuer")
      return void issueDemoEvidence(request, response, route.path, issuer);
    next();
  });
}
const listener = {
  host: "127.0.0.1",
  port: 5173,
  strictPort: true,
  cors: false,
  proxy: {
    ...(demo ? demoProxies(target, process.env) : {}),
    "/staff-api/": {
      target,
      changeOrigin: true,
      rewrite: (path) => path.slice("/staff-api".length),
      configure(proxy) {
        proxy.on("proxyReq", (req) => req.removeHeader("Cookie"));
      },
    },
    "/docs-api/": {
      target,
      changeOrigin: true,
      rewrite: (path) => path.slice("/docs-api".length),
      configure(proxy) {
        proxy.on("proxyReq", (req) => req.removeHeader("Cookie"));
      },
    },
  },
};
export default defineConfig({
  esbuild: { jsx: "automatic" },
  build: {
    rollupOptions: {
      input: {
        portal: fileURLToPath(new URL("./index.html", import.meta.url)),
        guide: fileURLToPath(new URL("./docs/index.html", import.meta.url)),
      },
    },
  },
  server: listener,
  preview: listener,
  plugins: [
    {
      name: "local-portal",
      configureServer: restrictPortalRequests,
      configurePreviewServer: restrictPortalRequests,
    },
  ],
});
