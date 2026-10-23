// The bindings App.vue and WidgetPanel.vue import from the pure modules, for the
// older test harnesses that evaluate a component's <script> with a hand-written
// stub table (they strip the import statements). New module imports are added
// here once instead of in every harness.
import { causeSentence, errorCause } from "../../src/errorCopy.mjs";
import {
  ACTIVITY_SCHEMA, LOGS_SCHEMA, activityContentKey, feedDocumentProblem, feedSignature, runStderrBlock, uniqueRowKeys
} from "../../src/feedModel.mjs";
import {
  outcomesUnavailableItem, runIsActive, runIsStale, spanLabel, staleRunCovers
} from "../../src/outcomes.mjs";
import { decodeLogPaths, plainText, visibleText } from "../../src/textSafety.mjs";
import { patchGuard } from "../../src/uiErrors.mjs";
import { WIDGET_CLOCK_TICK_MS } from "../../src/widgetModel.mjs";

export const APP_MODULE_STUBS = Object.freeze({
  ACTIVITY_SCHEMA, LOGS_SCHEMA, WIDGET_CLOCK_TICK_MS,
  activityContentKey, causeSentence, decodeLogPaths, errorCause, feedDocumentProblem, feedSignature,
  outcomesUnavailableItem, patchGuard, plainText, runIsActive, runIsStale, runStderrBlock, spanLabel, staleRunCovers,
  uniqueRowKeys, visibleText
});
