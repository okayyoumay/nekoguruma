---
paths:
  - "**/*.html"
  - "**/*.css"
  - "**/*.js"
  - "**/*.mjs"
  - "**/*.ts"
  - "**/*.tsx"
  - "**/*.jsx"
  - "**/*.vue"
  - "**/*.svelte"
---

# Browser code (Web UI)

The Web UI (design 9.6, R4) runs in an ordinary browser. When you write or change HTML, CSS or
client-side JavaScript/TypeScript, consult Modern Web Guidance first (ADR-490): a set of
guides on current web platform APIs, accessibility, performance and fallbacks, stewarded by the
Chrome, Edge, Firefox and MDN teams (https://github.com/GoogleChrome/modern-web-guidance).

- Search, then retrieve the matching guide, before writing an ad-hoc solution or adding a
  dependency. Use the pinned version and keep telemetry off:

  ```sh
  DISABLE_TELEMETRY=1 npx -y modern-web-guidance@1.0.1 search "<2-5 word capability>"
  DISABLE_TELEMETRY=1 npx -y modern-web-guidance@1.0.1 retrieve "<guide-id>"
  ```

  Queries name a generic UI capability ("light dismiss dialog", "responsive data table"),
  never vehicle, customer or job data.
- Raise the pinned version only in a pull request of its own that says what changed upstream.
- Until the design names a browser support target, aim at the tool's default (Baseline Widely
  available) and say in the pull request when code relies on anything newer.
- Cite a guide by its ID in comments and pull requests and paraphrase it; do not paste guide
  text into the repository.
- The guidance covers browser code only. Server routes, the Web API (`api/openapi.yaml`) and
  the Rust crates follow their own rules.
