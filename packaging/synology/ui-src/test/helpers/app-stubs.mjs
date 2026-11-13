// The bindings App.vue and WidgetPanel.vue import from the pure modules, for the
// older test harnesses that evaluate a component's <script> with a hand-written
// stub table (they strip the import statements). New module imports are added
// here once instead of in every harness.
import { causeSentence, errorCause } from "../../src/errorCopy.mjs";
import {
  ACTIVITY_SCHEMA, LOGS_SCHEMA, activityContentKey, activityIsUnchanged, activityVersionOf, feedDocumentProblem, feedSignature, runStderrBlock, uniqueRowKeys
} from "../../src/feedModel.mjs";
import {
  CONTROLLER_TICK_STALE_SECONDS, outcomesUnavailableItem, runIsActive, runIsStale, serviceAttentionItem, spanLabel, staleRunCovers
} from "../../src/outcomes.mjs";
import { decodeLogPaths, plainText, visibleText } from "../../src/textSafety.mjs";
import { clearRenderErrors, patchGuard, uiErrorSerial } from "../../src/uiErrors.mjs";
import { WIDGET_CLOCK_TICK_MS } from "../../src/widgetModel.mjs";

export const APP_MODULE_STUBS = Object.freeze({
  ACTIVITY_SCHEMA, CONTROLLER_TICK_STALE_SECONDS, LOGS_SCHEMA, WIDGET_CLOCK_TICK_MS,
  activityContentKey, activityIsUnchanged, activityVersionOf, causeSentence, clearRenderErrors, decodeLogPaths, errorCause, feedDocumentProblem, feedSignature,
  outcomesUnavailableItem, patchGuard, plainText, runIsActive, runIsStale, runStderrBlock, serviceAttentionItem, spanLabel, staleRunCovers,
  uiErrorSerial, uniqueRowKeys, visibleText
});
