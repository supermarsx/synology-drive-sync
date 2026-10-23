/**
 * Pure rules for the two read-only feeds on the Activity route.
 *
 * Kept out of App.vue so the properties that matter -- a row key that cannot
 * collide, a document that is not mistaken for an empty feed -- are tested
 * without a bundler.
 */

export const ACTIVITY_SCHEMA = "sdsync.dsm-activity.v1";
export const LOGS_SCHEMA = "sdsync.dsm-logs.v1";

/**
 * One key per item, guaranteed unique within the list.
 *
 * Vue 2's keyed patch assumes keys are unique. Two Activity events in the same
 * second with the same code, profile, category and level and no client request
 * ID are ordinary during a failure burst, and a shared key makes `updateChildren`
 * throw out of the scheduler flush -- after which no component repaints until
 * the page is reloaded. So the content key is suffixed with how many identical
 * keys came before it. That stays stable while the 200-line window slides, since
 * an event keeps the same count among its identical predecessors.
 */
export function uniqueRowKeys(items, contentKey) {
  const seen = new Map();
  return items.map((item) => {
    const base = String(contentKey(item));
    const occurrence = seen.get(base) || 0;
    seen.set(base, occurrence + 1);
    return `${base}#${occurrence}`;
  });
}

/** Content part of an Activity row key. */
export function activityContentKey(event) {
  return [event.epoch, event.code, event.profile, event.category, event.level, event.client_request_id].join(":");
}

/**
 * "" when the document is the shape the reader expects, otherwise the reason it
 * is not. A document of another schema, or one with no list, must not be read as
 * "nothing was recorded".
 */
export function feedDocumentProblem(document, schema, listKey, label) {
  if (!document || typeof document !== "object" || Array.isArray(document)) {
    return `the ${label} answer was not a document`;
  }
  if (document.schema !== schema) {
    const found = typeof document.schema === "string" && /^[A-Za-z0-9_.:-]{1,64}$/.test(document.schema) ? document.schema : "none";
    return `the ${label} answer has schema ${found}, expected ${schema}`;
  }
  if (!Array.isArray(document[listKey])) return `the ${label} answer has no ${listKey} list`;
  return "";
}

/** A cheap fingerprint for "did this poll bring anything new". */
export function feedSignature(items) {
  try {
    return JSON.stringify(items);
  } catch (_error) {
    return "";
  }
}

/**
 * The lines of one run's block in run-stderr. The runner starts each block with
 * `# epoch=<finished_epoch> scope=... operation=... exit=N`; the block runs to the
 * next header. { found, lines } -- found is false when no header names the epoch
 * (the run was rotated away), which is not the same as a run with no output.
 */
export function runStderrBlock(lines, finishedEpoch) {
  const header = (line) => /^# epoch=\d+(\s|$)/.test(line);
  const wanted = `# epoch=${finishedEpoch}`;
  let start = -1;
  for (let index = lines.length - 1; index >= 0; index -= 1) {
    if (lines[index] === wanted || lines[index].startsWith(`${wanted} `)) {
      start = index;
      break;
    }
  }
  if (start < 0) return { found: false, lines: [] };
  const block = [];
  for (let index = start + 1; index < lines.length && !header(lines[index]); index += 1) {
    if (lines[index].trim()) block.push(lines[index]);
  }
  return { found: true, lines: block.slice(-40) };
}
