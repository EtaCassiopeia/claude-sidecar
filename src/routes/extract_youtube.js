// YouTube watch-page extractor, injected into the page by `browser.rs` ahead
// of the article extractor. Like every other extractor here it is a fixed
// template — never assembled from caller input.
//
// A YouTube watch page is the worst case for the article extractor: what a
// reader wants — what is *said* in the video — is not in the page's prose at
// all, and what is in the page is a sidebar of unrelated videos and a comment
// thread. So this reads three sources instead of scraping:
//
//   * `ytInitialPlayerResponse` — title, channel, duration, views, description
//   * `ytInitialData`           — the related-video sidebar
//   * the rendered transcript panel — the spoken text, with timestamps
//
// Returns `null`, not an error, when the page is not a YouTube watch page:
// `browser.rs` chains this in front of the article extractor, and `null` is
// how it hands the page over. Every *other* shortfall (no captions, panel not
// open) still returns a page — metadata and related videos are useful on their
// own, and the transcript section says plainly why it is empty rather than
// letting a caller read silence as "the video said nothing".
(() => {
  "use strict";

  const MAX_RELATED = 20;
  const MAX_DESCRIPTION_CHARS = 2000;
  // Cues arrive one short line at a time. Regrouping them into blocks of about
  // this many characters costs a few timestamps and saves a great many
  // newlines, which on a long video is a real share of the tokens.
  const PARAGRAPH_CHARS = 600;
  const VIDEO_ID = /^[\w-]{11}$/;

  // --- is this a watch page? -------------------------------------------------

  if (!/(^|\.)youtube\.com$/.test(location.hostname)) return null;
  const shorts = /^\/shorts\/([\w-]{11})/.exec(location.pathname);
  const videoId = new URLSearchParams(location.search).get("v") || (shorts ? shorts[1] : null);
  if (!videoId || !VIDEO_ID.test(videoId)) return null;

  const watchUrl = (id) => `https://www.youtube.com/watch?v=${id}`;

  const isObject = (value) => !!value && typeof value === "object";

  // --- reading YouTube's inline JSON ----------------------------------------

  // Slice the balanced object literal starting at or after `from`. The blobs are
  // assigned inside a `<script>` body, so there is no parser to borrow — but
  // they are strict JSON, which makes brace matching enough.
  const jsonAt = (text, from) => {
    const start = text.indexOf("{", from);
    if (start < 0) return null;
    let depth = 0;
    let inString = false;
    let escaped = false;
    for (let i = start; i < text.length; i++) {
      const ch = text[i];
      if (inString) {
        if (escaped) escaped = false;
        else if (ch === "\\") escaped = true;
        else if (ch === '"') inString = false;
        continue;
      }
      if (ch === '"') inString = true;
      else if (ch === "{") depth++;
      else if (ch === "}" && --depth === 0) {
        try {
          return JSON.parse(text.slice(start, i + 1));
        } catch {
          return null; // a truncated or non-JSON match is not this blob
        }
      }
    }
    return null;
  };

  // A blob's name also appears in the code that *reads* it, and brace-slicing
  // from such a mention can still land on valid-but-unrelated JSON. So every
  // candidate must be confirmed by `accept` — parsing successfully is not
  // evidence of having found the right object.
  const findBlob = (text, marker, accept) => {
    for (
      let at = text.indexOf(marker);
      at >= 0;
      at = text.indexOf(marker, at + marker.length)
    ) {
      const parsed = jsonAt(text, at + marker.length);
      if (parsed && accept(parsed)) return parsed;
    }
    return null;
  };

  // The AppleScript verb `browser.rs` drives (`execute javascript`) runs in an
  // isolated world: the DOM is shared with the page, but `window` is not. So
  // `ytInitialPlayerResponse` and `ytInitialData` are never reachable as
  // globals however the page sets them, and the inline `<script>` text they are
  // assigned in is the only place to read them from.
  const fromScripts = (marker, accept) => {
    for (const script of document.querySelectorAll("script")) {
      const found = findBlob(script.textContent || "", marker, accept);
      if (found) return found;
    }
    return null;
  };

  // --- fetching --------------------------------------------------------------

  // Chrome's `execute javascript` returns a value synchronously and cannot
  // await a promise, so anything fetched here has to block. These requests are
  // same-origin and carry the tab's cookies, which is also what lets
  // member-only and age-gated captions come back at all.
  const httpGet = (url) => {
    try {
      const xhr = new XMLHttpRequest();
      xhr.open("GET", url, false);
      xhr.send(null);
      if (xhr.status !== 200) return { text: "", error: `HTTP ${xhr.status}` };
      return { text: xhr.responseText || "", error: "" };
    } catch (err) {
      return { text: "", error: String(err && err.message ? err.message : err) };
    }
  };

  // --- locating this video's player response ---------------------------------

  const describesThisVideo = (blob) => blob.videoDetails?.videoId === videoId;

  // YouTube is a single-page app: once the user clicks through to another
  // video, the inline scripts still describe whichever video the tab was
  // *opened* on. Re-requesting the current URL gets the server's own HTML for
  // the video actually on screen. `/browser/fetch` opens a fresh tab so it
  // never needs this; `/browser/tab` on a clicked-through video always does.
  const player =
    fromScripts("ytInitialPlayerResponse", describesThisVideo) ||
    findBlob(httpGet(location.href).text, "ytInitialPlayerResponse", describesThisVideo);
  // No data for the video in front of us. Returning null hands the page to the
  // article extractor, which is a poor result but an honest one — far better
  // than reporting some other video's transcript under this video's URL.
  if (!player) return null;

  // --- rendered-text helpers -------------------------------------------------

  // YouTube wraps display strings in half a dozen shapes depending on which
  // generation of renderer produced them.
  const pickText = (value) => {
    if (typeof value === "string") return value.trim();
    if (!isObject(value)) return "";
    if (typeof value.simpleText === "string") return value.simpleText.trim();
    if (typeof value.content === "string") return value.content.trim();
    if (Array.isArray(value.runs)) {
      return value.runs.map((run) => run.text || "").join("").trim();
    }
    if (typeof value.text === "string") return value.text.trim();
    return "";
  };

  const stamp = (seconds) => {
    const total = Math.max(0, Math.round(seconds));
    const pad = (n) => String(n).padStart(2, "0");
    const hours = Math.floor(total / 3600);
    const minutes = Math.floor((total % 3600) / 60);
    const secs = total % 60;
    return hours ? `${hours}:${pad(minutes)}:${pad(secs)}` : `${minutes}:${pad(secs)}`;
  };

  const groupDigits = (value) => {
    const n = Number(value);
    return Number.isFinite(n) && n > 0 ? n.toLocaleString("en-US") : "";
  };

  // --- the transcript --------------------------------------------------------
  //
  // Read the transcript panel out of the DOM rather than fetching the caption
  // track the player response names. That `timedtext` URL arrives signed and
  // looks usable, but YouTube now answers it `200` with an empty body unless
  // the request carries a token only the player itself can mint — and the
  // `get_transcript` InnerTube endpoint rejects an unsigned request with
  // `FAILED_PRECONDITION`. The rendered panel is the one source that is
  // reliably readable, and it is the same text the user sees.
  //
  // `browser.rs` opens the panel before extracting on `/browser/fetch`. On
  // `/browser/tab` it deliberately does not, since that would reach into the
  // page the user is looking at, so there the panel is read only if the user
  // already had it open.

  const listed = player.captions?.playerCaptionsTracklistRenderer?.captionTracks;
  const tracks = Array.isArray(listed) ? listed : [];

  // Panel timestamps are display strings: "4:31", or "1:04:31" past an hour.
  const parseStamp = (text) => {
    const parts = text.trim().split(":").map(Number);
    if (!parts.length || parts.some((part) => !Number.isFinite(part))) return 0;
    return parts.reduce((total, part) => total * 60 + part, 0);
  };

  const cues = [];
  for (const segment of document.querySelectorAll("ytd-transcript-segment-renderer")) {
    const text = (segment.querySelector(".segment-text")?.textContent || "")
      .replace(/\s+/g, " ")
      .trim();
    if (!text) continue;
    const at = parseStamp(segment.querySelector(".segment-timestamp")?.textContent || "0");
    cues.push({ at, text });
  }

  // Say which of the two it is, so an empty transcript is never mistaken for a
  // video in which nothing was said.
  const blocked = player.playabilityStatus?.reason;
  const transcriptError = blocked
    ? `this video is not playable here (${blocked})`
    : tracks.length
      ? "no transcript panel was open on the page"
      : "this video has no caption track";

  // The panel repeats a cue when a caption line spans two segments.
  const paragraphs = [];
  let current = null;
  let previous = "";
  for (const cue of cues) {
    if (cue.text === previous) continue;
    previous = cue.text;
    if (current) current.text += ` ${cue.text}`;
    else current = { at: cue.at, text: cue.text };
    if (current.text.length >= PARAGRAPH_CHARS) {
      paragraphs.push(current);
      current = null;
    }
  }
  if (current) paragraphs.push(current);

  // --- related videos --------------------------------------------------------

  const TITLE_KEYS = ["title", "headline", "name"];

  const deepTitle = (node, depth) => {
    if (depth > 5 || !isObject(node)) return "";
    if (Array.isArray(node)) {
      for (const item of node) {
        const found = deepTitle(item, depth + 1);
        if (found) return found;
      }
      return "";
    }
    for (const key of TITLE_KEYS) {
      const text = pickText(node[key]);
      if (text) return text;
    }
    for (const key of Object.keys(node)) {
      if (key === "navigationEndpoint" || key === "thumbnail" || key === "menu") continue;
      const found = deepTitle(node[key], depth + 1);
      if (found) return found;
    }
    return "";
  };

  // Every rendered string under a sidebar entry, outermost first. The channel
  // and the duration keep moving — `compactVideoRenderer` held them in
  // `longBylineText` and `lengthText`, `lockupViewModel` put the channel in a
  // metadata row and the duration in a thumbnail badge — so gather the strings
  // the entry displays and recognise them by what they look like rather than by
  // where this month's renderer happens to keep them.
  const deepTexts = (node, depth, out) => {
    if (out.length >= 10 || depth > 10 || !isObject(node)) return out;
    if (Array.isArray(node)) {
      for (const item of node) deepTexts(item, depth + 1, out);
      return out;
    }
    const text = pickText(node);
    if (text) {
      // A rendered string is a leaf: nothing below it is another string.
      if (!out.includes(text)) out.push(text);
      return out;
    }
    for (const key of Object.keys(node)) deepTexts(node[key], depth + 1, out);
    return out;
  };

  const DURATION = /^\d{1,3}(:\d{2}){1,2}$/;

  // The sidebar renderer has been rewritten repeatedly — `compactVideoRenderer`,
  // then `lockupViewModel` — so rather than track whichever shape is current,
  // walk the blob for anything carrying a video id and take the nearest title
  // from its subtree. Bounded by depth and result count so a large payload
  // cannot stall the page.
  const collectRelated = (root) => {
    // Keyed by id rather than appended, because one video can appear as more
    // than one node — the "up next" entry is announced by a lean autoplay
    // renderer before the sidebar's full one — and whichever comes first should
    // not fix how much is known about it.
    const byId = new Map();
    const walk = (node, depth) => {
      if (depth > 14 || !isObject(node)) return;
      if (Array.isArray(node)) {
        for (const item of node) walk(item, depth + 1);
        return;
      }
      const id = typeof node.videoId === "string" ? node.videoId : node.contentId;
      if (typeof id === "string" && VIDEO_ID.test(id) && id !== videoId) {
        const title = deepTitle(node, 0);
        const known = byId.get(id);
        if (title && (known || byId.size < MAX_RELATED)) {
          const texts = deepTexts(node, 0, []).filter((text) => text !== title);
          // A view count ("1.2M views · 8 years ago") is the other short row an
          // entry displays, and it dates badly next to a cached answer.
          const channel = texts.find(
            (text) => text.length <= 60 && !DURATION.test(text) && !/\bviews?\b/i.test(text)
          );
          const length = texts.find((text) => DURATION.test(text));
          if (known) {
            known.channel = known.channel || channel;
            known.length = known.length || length;
          } else {
            byId.set(id, { id, title, channel, length });
          }
        }
      }
      for (const key of Object.keys(node)) walk(node[key], depth + 1);
    };
    walk(root, 0);
    return [...byId.values()];
  };

  // Accept only the blob that actually carries the watch-next tree: the name
  // appears in several scripts, and the sidebar subtree is the point of it.
  // Walking the whole blob instead would pull in playlist entries and
  // end-screen cards as if they were suggestions.
  const sidebarOf = (blob) =>
    blob.contents?.twoColumnWatchNextResults?.secondaryResults || null;
  const initialData = fromScripts("ytInitialData", (blob) => !!sidebarOf(blob));
  const related = initialData ? collectRelated(sidebarOf(initialData)) : [];

  // --- assembly --------------------------------------------------------------

  const details = player.videoDetails || {};
  const micro = player.microformat?.playerMicroformatRenderer || {};
  const title = details.title || pickText(micro.title) || document.title;

  const facts = [];
  const addFact = (label, value) => {
    if (value) facts.push(`- **${label}:** ${value}`);
  };
  addFact("Channel", details.author || micro.ownerChannelName);
  addFact("Channel URL", micro.ownerProfileUrl);
  addFact("Video", watchUrl(videoId));
  addFact("Published", (micro.publishDate || micro.uploadDate || "").slice(0, 10));
  addFact("Duration", details.lengthSeconds ? stamp(Number(details.lengthSeconds)) : "");
  addFact("Views", groupDigits(details.viewCount || micro.viewCount));
  addFact("Category", micro.category);
  if (details.isLiveContent) facts.push("- **Live:** yes");
  // Which of the listed tracks the panel actually rendered is not recoverable
  // from the DOM — the selected language lives behind a shadow root. So name a
  // language only when there is exactly one to name; announcing the first of
  // thirteen would be a guess dressed up as a fact.
  if (tracks.length === 1) {
    const only = tracks[0];
    const language = pickText(only.name) || only.languageCode || "unknown";
    addFact("Captions", `${language}${only.kind === "asr" ? " (auto-generated)" : ""}`);
  } else if (tracks.length) {
    addFact("Captions", `${tracks.length} languages`);
  }
  addFact("Keywords", Array.isArray(details.keywords) ? details.keywords.join(", ") : "");

  const sections = [`# ${title}`, "", facts.join("\n")];

  const rawDescription = (details.shortDescription || pickText(micro.description) || "").trim();
  if (rawDescription) {
    const clipped = rawDescription.length > MAX_DESCRIPTION_CHARS;
    const description = clipped
      ? `${rawDescription.slice(0, MAX_DESCRIPTION_CHARS)}\n\n_(description truncated)_`
      : rawDescription;
    sections.push("", "## Description", "", description);
  }

  // Related videos go above the transcript because the transcript is by far the
  // longest section: with `max_chars` set, putting it last means truncation
  // eats transcript tail rather than swallowing the links and the metadata.
  if (related.length) {
    const items = related.map((video) => {
      const suffix = [video.channel, video.length].filter(Boolean).join(" · ");
      return `- [${video.title}](${watchUrl(video.id)})${suffix ? ` — ${suffix}` : ""}`;
    });
    sections.push("", "## Related videos", "", items.join("\n"));
  }

  sections.push("", "## Transcript", "");
  if (paragraphs.length) {
    sections.push(paragraphs.map((p) => `[${stamp(p.at)}] ${p.text}`).join("\n\n"));
  } else {
    sections.push(`_Transcript unavailable: ${transcriptError || "no cues were returned"}._`);
  }

  return JSON.stringify({
    url: location.href,
    title,
    content: sections.join("\n").replace(/\n{3,}/g, "\n\n").trim(),
  });
})
