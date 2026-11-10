import { docsPages } from "./data.ts";

const style = `
  :root { font-family: Georgia, serif; color: #222; line-height: 1.6; }
  body { margin: 0; display: grid; grid-template-columns: 240px 1fr; }
  nav { padding: 24px; background: #f2efe9; min-height: 100vh; }
  nav a { display: block; padding: 4px 0; }
  main { padding: 24px 48px; max-width: 760px; }
  table { border-collapse: collapse; margin: 16px 0; }
  th, td { border: 1px solid #ccc; padding: 6px 12px; text-align: left; }
  pre { background: #1e1e1e; color: #eee; padding: 12px; overflow-x: auto; }
  iframe { width: 100%; height: 60px; border: 1px solid #ccc; }
`;

const filler = [
    "Requests are evaluated against the pool that owns the calling key.",
    "Clients should treat every limit as a ceiling rather than a target.",
    "Responses carry headers describing the remaining allowance.",
    "Retries must back off exponentially and respect the reset header.",
    "Limits are enforced per region and do not transfer between regions.",
];

function navigation(): string {
    return `<nav aria-label="Documentation">${
        docsPages.map((page) =>
            `<a href="/docs/${page.slug}">${page.title}</a>`
        ).join("")
    }</nav>`;
}

function sections(topic: string): string {
    return Array.from({ length: 24 }, (_, index) => `
        <h2 id="section-${index + 1}">${topic} part ${index + 1}</h2>
        <p>${filler[index % filler.length]} ${
        filler[(index + 2) % filler.length]
    }</p>
        <pre><code>curl https://api.fieldstone.test/v1/${
        topic.toLowerCase().replaceAll(" ", "-")
    }?part=${index + 1}</code></pre>`).join("");
}

function layout(title: string, main: string): string {
    return `<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <title>${title}</title>
    <style>${style}</style>
  </head>
  <body>
    ${navigation()}
    <main>${main}</main>
  </body>
</html>`;
}

const rateLimits = `
    <h1>Rate limits</h1>
    <p><a href="#changelog">Jump to changelog</a></p>
    <table>
      <thead><tr><th scope="col">Limit</th><th scope="col">Value</th></tr></thead>
      <tbody>
        <tr><td>Sustained limit</td><td>600 requests per minute</td></tr>
        <tr><td>Concurrent connections</td><td>32 per key</td></tr>
      </tbody>
    </table>
    <details id="burst">
      <summary>Burst limits</summary>
      <p>Burst allowance is 240 requests per 10 seconds.</p>
    </details>
    <iframe src="/docs/embed/status" title="Service status"></iframe>
    ${sections("Rate limits")}
    <h2 id="changelog">Changelog</h2>
    <ul id="changelog-entries"></ul>
    <script>
      new IntersectionObserver(async (entries, observer) => {
        if (!entries.some((entry) => entry.isIntersecting)) return;
        observer.disconnect();
        const response = await fetch("/api/changelog");
        const list = document.querySelector("#changelog-entries");
        for (const text of await response.json()) {
          const item = document.createElement("li");
          item.textContent = text;
          list.append(item);
        }
      }).observe(document.querySelector("#changelog-entries"));
    </script>`;

export function renderDocs(path: string): string | null {
    if (path === "/docs/") {
        return layout(
            "Fieldstone API documentation",
            "<h1>Fieldstone API documentation</h1><p>Choose a topic.</p>",
        );
    }
    if (path === "/docs/embed/status") {
        return "<!doctype html><title>Status</title><p>All systems operational</p>";
    }
    const page = docsPages.find((entry) => `/docs/${entry.slug}` === path);
    if (!page) return null;
    return layout(
        page.title,
        page.slug === "rate-limits"
            ? rateLimits
            : `<h1>${page.title}</h1>${sections(page.title)}`,
    );
}
