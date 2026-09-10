"use strict";

/**
 * Drop CSS comments, per-line indentation and blank lines. Nothing else.
 *
 * The AppWindow stylesheet is embedded verbatim into the JavaScript bundle as a
 * string literal (see src/runtimeStyles.js), so every byte of it is parse work
 * DSM does before the window paints. A conventional minifier would buy roughly
 * twice as much, but the reviewed contracts in test/ and validate_spk.py pin the
 * built stylesheet at byte level -- the " > " combinator spacing, the space
 * before "!important", "0.8s" over ".8s", "@media (max-width: 980px)" over
 * "@media (max-width:980px)", "from" over "0%", and selectors anchored to the
 * start of a line -- so this transform is deliberately restricted to bytes no
 * rule can express an opinion about. csso, cssnano and clean-css each break
 * eleven of those checks; see docs/dsm/ui-bundle-budget.md.
 *
 * It runs as a pre-loader on every .css module, which is what keeps the
 * `?runtime` (asset/source) copy and the MiniCssExtractPlugin copy identical:
 * both consumers are fed by this one transform, upstream of the oneOf fork.
 */

/** Remove comments, keeping the newlines they spanned so lines still line up. */
function stripComments(css) {
  let out = "";
  let index = 0;
  let quote = null;
  while (index < css.length) {
    const character = css[index];
    if (quote) {
      out += character;
      if (character === "\\") {
        out += css[index + 1] === undefined ? "" : css[index + 1];
        index += 2;
        continue;
      }
      if (character === quote) quote = null;
      index += 1;
      continue;
    }
    if (character === '"' || character === "'") {
      quote = character;
      out += character;
      index += 1;
      continue;
    }
    if (character === "/" && css[index + 1] === "*") {
      const end = css.indexOf("*/", index + 2);
      if (end < 0) throw new Error("unterminated CSS comment");
      const spanned = css.slice(index, end + 2);
      // CSS drops a comment entirely rather than collapsing it to a space, so
      // this leaves nothing behind: a hypothetical `a/**/b` must not become
      // `a b`. The newlines it covered stay, so the line structure is unchanged.
      out += "\n".repeat((spanned.match(/\n/g) || []).length);
      index = end + 2;
      continue;
    }
    out += character;
    index += 1;
  }
  if (quote) throw new Error("unterminated CSS string");
  return out;
}

function condense(css) {
  const lines = [];
  for (const line of stripComments(css).split("\n")) {
    const trimmed = line.trim();
    if (trimmed !== "") lines.push(trimmed);
  }
  return lines.join("\n") + "\n";
}

// What this loader handed each consumer, keyed by source file. Both .css rules
// -- the `?runtime` asset/source copy and the css-loader copy -- are fed from
// here, so a divergence between them can only come from something downstream.
// AssertRuntimeStyleParityPlugin reads this to prove that did not happen.
const condensed = new Map();

module.exports = function condenseCssLoader(source) {
  const output = condense(source);
  const previous = condensed.get(this.resourcePath);
  if (previous !== undefined && previous !== output) {
    throw new Error(
      `condense-css-loader produced different bytes for ${this.resourcePath} ` +
        "on two requests of the same file"
    );
  }
  condensed.set(this.resourcePath, output);
  return output;
};

module.exports.condense = condense;
module.exports.condensed = condensed;
