/**
 * Make a name or a message safe to read.
 *
 * Profile names and file names reach the dashboard as plain text, and Vue
 * escapes markup, but a bidirectional override (U+202E), an isolate, a
 * zero-width space or a byte-order mark is not markup: it is a character that
 * changes what a reader believes the name says. The case-collision and
 * type-conflict messages depend on the reader being able to tell two names
 * apart, so these characters are shown as their code points instead of being
 * acted on. Surrounding blanks carry no information and are trimmed.
 *
 * Imports nothing, so the widget model, the outcomes copy and the AppWindow all
 * apply the same rule. The ranges are numeric so that no invisible character
 * appears in this source file.
 */

// Inclusive code point ranges: soft hyphen, combining grapheme joiner, Arabic
// letter mark, Hangul and Khmer fillers, Mongolian variation selectors,
// zero-width and directional marks (200B-200F), embeddings and overrides
// (202A-202E), word joiner through the deprecated format controls and isolates
// (2060-206F), Hangul filler, variation selectors, byte-order mark, halfwidth
// filler and the interlinear annotation anchors.
const INVISIBLE_RANGES = [
  [0x00ad, 0x00ad], [0x034f, 0x034f], [0x061c, 0x061c], [0x115f, 0x1160], [0x17b4, 0x17b5],
  [0x180b, 0x180e], [0x200b, 0x200f], [0x202a, 0x202e], [0x2060, 0x206f], [0x3164, 0x3164],
  [0xfe00, 0xfe0f], [0xfeff, 0xfeff], [0xffa0, 0xffa0], [0xfff9, 0xfffb]
];

function isInvisible(code) {
  return INVISIBLE_RANGES.some(([low, high]) => code >= low && code <= high);
}

/** Names: invisible and directional characters shown as \u{XXXX}, blanks trimmed. */
export function visibleText(value) {
  const text = typeof value === "string" ? value : "";
  let out = "";
  for (const character of text) {
    const code = character.codePointAt(0);
    out += isInvisible(code) ? `\\u{${code.toString(16).toUpperCase().padStart(4, "0")}}` : character;
  }
  return out.trim();
}

/** Messages: control, invisible and directional characters become blanks, runs of blanks one. */
export function plainText(value, limit = 300) {
  const text = typeof value === "string" ? value : String(value);
  let out = "";
  for (const character of text) {
    const code = character.codePointAt(0);
    out += code < 0x20 || (code >= 0x7f && code <= 0x9f) || isInvisible(code) ? " " : character;
  }
  return out.replace(/ +/g, " ").trim().slice(0, limit);
}

/**
 * Paths in sync.log and in a profile's failed_path are percent-encoded by the
 * package (every byte outside A-Za-z0-9._/:,- becomes %XX) so a hostile file name
 * cannot forge a log line. This turns the encoding back into the name for display.
 * A run of %XX bytes that is not valid UTF-8, or that decodes to a quote, a
 * backslash or a control character, is left encoded: showing it raw would either
 * be wrong or break the line it sits in.
 */
export function decodePercentPath(value) {
  const text = typeof value === "string" ? value : "";
  return text.replace(/(?:%[0-9A-Fa-f]{2})+/g, (run) => {
    const bytes = new Uint8Array(run.length / 3);
    for (let index = 0; index < bytes.length; index += 1) bytes[index] = parseInt(run.slice(index * 3 + 1, index * 3 + 3), 16);
    let decoded;
    try {
      decoded = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
    } catch (_error) {
      return run;
    }
    for (const character of decoded) {
      const code = character.codePointAt(0);
      if (code < 0x20 || code === 0x7f || character === '"' || character === "\\") return run;
    }
    return decoded;
  });
}

/** A sync.log JSON line with its "path" member decoded for display. */
export function decodeLogPaths(line) {
  if (typeof line !== "string") return "";
  // No regular expression here on purpose: validate_spk.py's string-literal scan of
  // the bundle is lexical, and a quote inside a regex literal derails it.
  const marker = '"path":"';
  let out = "";
  let from = 0;
  for (;;) {
    const start = line.indexOf(marker, from);
    if (start < 0) break;
    const valueStart = start + marker.length;
    const end = line.indexOf('"', valueStart);
    if (end < 0) break;
    const value = line.slice(valueStart, end);
    // A value holding an escape is left alone: it is not the package's encoding.
    out += line.slice(from, valueStart) + (value.includes("\\") ? value : decodePercentPath(value));
    from = end;
  }
  return out + line.slice(from);
}
