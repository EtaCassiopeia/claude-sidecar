// Article extractor + markdown serializer, injected into the page by
// `browser.rs`. Like the text/html extractors it is a fixed template — never
// assembled from caller input — so widening the format set does not widen what
// a caller can run in the user's browser.
//
// Two passes over a *clone* of the live DOM (the user's tab is never mutated):
// strip site chrome, then score the surviving containers by the prose they hold
// and serialize the winner. The point is token cost — on a typical article the
// body copy is a small fraction of `body.innerText`, and the rest is nav,
// cookie banners, related-article rails, and comments.
//
// Markdown is emitted unescaped: the consumer is a language model, and escape
// sequences spend tokens without adding meaning. For the same reason link
// targets are opt-in (`keepLinks`) rather than default — measured across
// Wikipedia, MDN, and the Rust book, `](https://…)` runs 20-40% of the whole
// output, and a reader that only wants the prose pays for every one of them.
//
// Exports a function rather than self-invoking so the caller supplies
// `keepLinks` from a fixed pair of call sites; the page is never handed a
// script assembled from request data.
((keepLinks) => {
  "use strict";

  // Elements that never carry article prose.
  const STRIP =
    "script,style,noscript,template,svg,canvas,iframe,object,embed,form," +
    "input,select,textarea,button,nav,aside,dialog,link,meta,map,area";

  // Class/id fragments that mark site chrome. Bounded by separators so
  // "commentary" and "bannerman" do not match.
  const JUNK =
    /(^|[\s_-])(comment|comments|disqus|sidebar|footer|nav|navigation|menu|promo|banner|cookie|consent|gdpr|newsletter|subscribe|signup|paywall|related|recommend|share|social|advert|ads?|sponsor|popup|modal|overlay|breadcrumb|pagination|skip|masthead|toolbar|widget)([\s_-]|$)/i;

  const textOf = (el) => (el.textContent || "").replace(/\s+/g, " ").trim();

  const linkDensity = (el) => {
    const total = textOf(el).length;
    if (!total) return 1;
    let linked = 0;
    for (const a of el.querySelectorAll("a")) linked += textOf(a).length;
    return Math.min(linked / total, 1);
  };

  // Remove site chrome from the clone. Guarded so an unlucky class name on a
  // wrapper cannot delete the article itself.
  const strip = (root) => {
    const rootLen = textOf(root).length || 1;
    for (const el of root.querySelectorAll(STRIP)) el.remove();
    for (const el of root.querySelectorAll("[hidden],[aria-hidden=true]")) el.remove();

    // A masthead is a `header` at the top of the page; a `header` inside an
    // article is its title block and stays.
    for (const el of root.querySelectorAll("header,footer")) {
      if (!el.closest("article")) el.remove();
    }

    for (const el of root.querySelectorAll("[class],[id]")) {
      if (!root.contains(el)) continue; // an ancestor already took it
      const marks = `${el.getAttribute("class") || ""} ${el.id || ""}`;
      if (!JUNK.test(marks)) continue;
      if (el.querySelector("article")) continue;
      if (textOf(el).length > rootLen * 0.5) continue; // it *is* the content
      el.remove();
    }
    return root;
  };

  // Readability's scoring shape: credit each block of prose to its ancestors
  // with distance decay, so the tightest container holding the article wins
  // rather than `body`, which trivially contains everything.
  const pickContent = (root) => {
    const scores = new Map();
    for (const el of root.querySelectorAll("p,pre,blockquote,li,h2,h3,table")) {
      const len = textOf(el).length;
      if (len < 25) continue;
      const base = 1 + Math.min(len / 100, 3);
      let node = el.parentElement;
      for (let depth = 0; node && depth < 3; depth++) {
        scores.set(node, (scores.get(node) || 0) + base / (depth ? depth * 2 : 1));
        node = node.parentElement;
      }
    }

    let best = null;
    let bestScore = 0;
    scores.forEach((score, node) => {
      let s = score * (1 - linkDensity(node));
      const tag = node.tagName;
      if (tag === "ARTICLE" || tag === "MAIN" || node.getAttribute("role") === "main") {
        s *= 1.5;
      }
      if (s > bestScore) {
        bestScore = s;
        best = node;
      }
    });
    return best || root;
  };

  // --- markdown serialization ------------------------------------------------

  const HEADINGS = { H1: 1, H2: 2, H3: 3, H4: 4, H5: 5, H6: 6 };

  // Used only to recognise the whitespace *between* blocks. Source formatting
  // puts a newline between `</p>` and the next tag; collapsed naively that
  // becomes a space, and every paragraph and code fence comes out indented by
  // however many such nodes preceded it.
  const BLOCK = new Set([
    "P", "DIV", "SECTION", "ARTICLE", "MAIN", "HEADER", "FOOTER", "FIGURE",
    "FIGCAPTION", "UL", "OL", "LI", "DL", "DT", "DD", "BLOCKQUOTE", "PRE",
    "TABLE", "THEAD", "TBODY", "TR", "TD", "TH", "HR", "BR", "ASIDE",
    "H1", "H2", "H3", "H4", "H5", "H6",
  ]);

  const isBlock = (node) =>
    !!node && node.nodeType === Node.ELEMENT_NODE && BLOCK.has(node.tagName);

  const renderChildren = (node) => {
    let out = "";
    for (const child of node.childNodes) out += render(child);
    return out;
  };

  // Children flattened to a single line — for headings, links, table cells, and
  // the other places where block structure cannot be expressed.
  const inline = (node) => renderChildren(node).replace(/\s*\n+\s*/g, " ").trim();

  const fenceLang = (pre) => {
    const marks = `${pre.className} ${pre.querySelector("code")?.className || ""}`;
    return /(?:language|lang|highlight)[-_]([a-z0-9+#]+)/i.exec(marks)?.[1] || "";
  };

  const renderRow = (tr) =>
    `| ${[...tr.children].map((cell) => inline(cell) || " ").join(" | ")} |`;

  const renderTable = (table) => {
    const rows = [...table.querySelectorAll("tr")];
    if (!rows.length) return "";
    const rule = `|${" --- |".repeat(rows[0].children.length)}`;
    return [renderRow(rows[0]), rule, ...rows.slice(1).map(renderRow)].join("\n");
  };

  // Nested lists carry no indent of their own — the parent item indents every
  // continuation line it receives, which composes to the right depth.
  const renderList = (list) => {
    const ordered = list.tagName === "OL";
    let index = Number(list.getAttribute("start")) || 1;
    const items = [];
    for (const li of list.children) {
      if (li.tagName !== "LI") continue;
      const body = renderChildren(li).trim();
      if (!body) continue;
      const marker = ordered ? `${index++}. ` : "- ";
      const [first, ...rest] = body.split("\n");
      items.push([marker + first, ...rest.map((l) => (l ? `  ${l}` : l))].join("\n"));
    }
    return items.join("\n");
  };

  // Markdown for `node`. Blocks self-terminate with a blank line so callers can
  // concatenate children without tracking separators.
  const render = (node) => {
    if (node.nodeType === Node.TEXT_NODE) {
      const text = node.nodeValue.replace(/\s+/g, " ");
      if (text !== " ") return text;
      // A lone space only earns its place between two inline neighbours.
      const dividing =
        isBlock(node.previousSibling) ||
        isBlock(node.nextSibling) ||
        !node.previousSibling ||
        !node.nextSibling;
      return dividing ? "" : text;
    }
    if (node.nodeType !== Node.ELEMENT_NODE) return "";

    const tag = node.tagName;
    if (HEADINGS[tag]) {
      const text = inline(node);
      return text ? `${"#".repeat(HEADINGS[tag])} ${text}\n\n` : "";
    }

    switch (tag) {
      case "BR":
        return "\n";
      case "HR":
        return "---\n\n";
      case "A": {
        const text = inline(node);
        if (!text || !keepLinks) return text;
        // `.href` is resolved against the document, so relative links come back
        // absolute; anything not http(s) (mailto:, javascript:, #anchor) keeps
        // its text and drops the target.
        return /^https?:/.test(node.href) ? `[${text}](${node.href})` : text;
      }
      case "IMG": {
        // Alt text is the only part a model can act on, and CDN URLs are long,
        // so an image without alt contributes nothing worth its tokens.
        const alt = (node.getAttribute("alt") || "").trim();
        if (!alt) return "";
        return keepLinks ? `![${alt}](${node.src})` : `![${alt}]`;
      }
      case "STRONG":
      case "B": {
        const text = inline(node);
        return text ? `**${text}**` : "";
      }
      case "EM":
      case "I": {
        const text = inline(node);
        return text ? `*${text}*` : "";
      }
      case "CODE": {
        // `code` inside `pre` never reaches here — the PRE arm takes its text
        // directly rather than recursing.
        const text = node.textContent.replace(/\s+/g, " ").trim();
        return text ? `\`${text}\`` : "";
      }
      case "PRE": {
        const text = node.textContent.replace(/\s+$/, "");
        return text ? `\`\`\`${fenceLang(node)}\n${text}\n\`\`\`\n\n` : "";
      }
      case "BLOCKQUOTE": {
        const body = renderChildren(node).trim();
        if (!body) return "";
        return `${body.split("\n").map((l) => `> ${l}`).join("\n")}\n\n`;
      }
      case "UL":
      case "OL": {
        const body = renderList(node);
        return body ? `${body}\n\n` : "";
      }
      case "TABLE": {
        const body = renderTable(node);
        return body ? `${body}\n\n` : "";
      }
      case "P":
      case "FIGCAPTION":
      case "DT":
      case "DD": {
        const body = renderChildren(node).replace(/[ \t]+/g, " ").trim();
        return body ? `${body}\n\n` : "";
      }
      default:
        return renderChildren(node);
    }
  };

  // --- byline ----------------------------------------------------------------
  //
  // The strip pass drops the masthead, and on sites that do not wrap the
  // article in <article> the publication date and author go with it. Both are
  // usually declared in <head> anyway, which the clone never touches, so
  // recover them there and re-attach as one line — cheaper and more reliable
  // than trying to keep the right <header> in the body.

  const firstMeta = (selectors) => {
    for (const selector of selectors) {
      for (const el of document.querySelectorAll(selector)) {
        const raw = el.getAttribute("content") || el.getAttribute("datetime") || "";
        if (raw.trim()) return raw.trim();
      }
    }
    return "";
  };

  // Schema.org data is often the only place a date survives. Walk it for the
  // requested key, bounded so a pathological blob cannot hang the extraction.
  const fromJsonLd = (key) => {
    const name = (value) => {
      if (typeof value === "string") return value;
      if (Array.isArray(value)) return value.length ? name(value[0]) : "";
      if (value && typeof value.name === "string") return value.name;
      return "";
    };
    for (const script of document.querySelectorAll('script[type="application/ld+json"]')) {
      let parsed;
      try {
        parsed = JSON.parse(script.textContent);
      } catch {
        continue; // a malformed blob is not a reason to lose the whole byline
      }
      const stack = [parsed];
      for (let seen = 0; stack.length && seen < 500; seen++) {
        const node = stack.pop();
        if (Array.isArray(node)) {
          stack.push(...node);
        } else if (node && typeof node === "object") {
          const hit = name(node[key]).trim();
          if (hit) return hit;
          stack.push(...Object.values(node));
        }
      }
    }
    return "";
  };

  const published = (
    firstMeta([
      'meta[property="article:published_time"]',
      'meta[name="article:published_time"]',
      'meta[itemprop="datePublished"]',
      'meta[name="datePublished"]',
      'meta[name="citation_publication_date"]',
      'meta[name="pubdate"]',
      'meta[name="date"]',
      "article time[datetime]",
      "time[datetime]",
    ]) || fromJsonLd("datePublished")
  ).trim();

  const author = (
    firstMeta([
      'meta[name="author"]',
      'meta[property="article:author"]',
      'meta[name="citation_author"]',
      'meta[property="og:article:author"]',
    ]) || fromJsonLd("author")
  ).trim();

  // Plenty of sites declare no metadata at all and put the byline in a plain
  // div beside the prose — a sibling of the container the scorer picks, so it
  // is lost without ever passing through the strip pass. Look for one, but
  // demand it read like a byline: a year or a leading "By", and short. That
  // rules out the menus and promo blocks a bare class match would drag in.
  const domByline = () => {
    const MARKS =
      /(^|[\s_-])(byline|dateline|publish|published|post-?date|post-?meta|author|posted)([\s_-]|$)/i;
    for (const el of document.querySelectorAll("[class],[id]")) {
      if (!MARKS.test(`${el.getAttribute("class") || ""} ${el.id || ""}`)) continue;
      if (el.closest("nav,footer,aside")) continue;
      const text = (el.textContent || "").replace(/\s+/g, " ").trim();
      if (text.length <= 120 && (/\b(19|20)\d{2}\b/.test(text) || /^by\s/i.test(text))) {
        return text;
      }
    }
    return "";
  };

  // `article:author` is frequently a profile URL rather than a name, and some
  // sites stuff a whole sentence into `author` — neither is worth the tokens.
  const usableAuthor = author && author.length <= 80 && !/^https?:/.test(author) ? author : "";
  // Prefer the date alone: "2024-09-05T00:00:00Z" says nothing "2024-09-05" does not.
  const isoDay = /^(\d{4}-\d{2}-\d{2})/.exec(published);
  const usableDate = isoDay ? isoDay[1] : published.length <= 30 ? published : "";

  // --- entry point -----------------------------------------------------------

  const body = document.body ? document.body.cloneNode(true) : null;
  const content = body
    ? render(pickContent(strip(body)))
        .replace(/[ \t]+\n/g, "\n")
        .replace(/\n{3,}/g, "\n\n")
        .trim()
    : "";

  // Skip the byline when the extractor already kept one, so pages that do use
  // <article> are not annotated twice.
  const opening = content.slice(0, 300);
  const parts = [];
  if (usableDate && !opening.includes(usableDate)) parts.push(usableDate);
  if (usableAuthor && !opening.includes(usableAuthor)) parts.push(usableAuthor);

  let bylineText = parts.join(" · ");
  if (!usableDate && !usableAuthor) {
    const found = domByline();
    if (found && !opening.includes(found)) bylineText = found;
  }
  const byline = bylineText && content ? `*${bylineText}*\n\n` : "";

  return JSON.stringify({
    url: location.href,
    title: document.title,
    content: byline + content,
  });
})
