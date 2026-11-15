import scalarUrl from "../node_modules/@scalar/api-reference/dist/browser/standalone.js?url";
import guide from "../guide.md?raw";

async function loadDocument() {
  const response = await fetch("/openapi.json");
  if (!response.ok) throw new Error("The OpenAPI document could not be loaded.");
  const document = await response.json();
  document.info.description = guide.trim() || document.info.description;
  // The downloadable document keeps the direct API address for service clients.
  document.servers = [
    { url: `${window.location.origin}/docs-api`, description: "Local API via the portal" },
  ];
  return document;
}

async function showReference() {
  const [content] = await Promise.all([
    loadDocument(),
    import(/* @vite-ignore */ scalarUrl),
  ]);
  const reference = document.querySelector("#reference");
  reference.replaceChildren();
  window.Scalar.createApiReference(reference, {
    content,
    withDefaultFonts: false,
    agent: { disabled: true },
    telemetry: false,
    persistAuth: false,
    authentication: { preferredSecurityScheme: "callerBearer" },
    showDeveloperTools: "never",
    hideClientButton: true,
    documentDownloadType: "none",
  });
}

showReference().catch(() => {
  document.querySelector("#reference").textContent =
    "The API guide could not be loaded. Refresh this page or open the OpenAPI JSON above.";
});
