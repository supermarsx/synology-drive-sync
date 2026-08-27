const RUNTIME_STYLE_ID = "sdsync-current-runtime-style";

/**
 * Keep the stylesheet coupled to the JavaScript bundle DSM actually executed.
 * DSM's AppWindow loader de-duplicates stylesheet URLs, so an upgraded package
 * can otherwise run current markup and JavaScript against an older cached CSS
 * response for the same package URL.
 */
export function installRuntimeStyles(cssText, targetDocument = document) {
  let style = targetDocument.getElementById(RUNTIME_STYLE_ID);
  if (!style) {
    style = targetDocument.createElement("style");
    style.id = RUNTIME_STYLE_ID;
    style.type = "text/css";
    style.setAttribute("data-sdsync-runtime-style", "current");
    (targetDocument.head || targetDocument.documentElement).appendChild(style);
  }
  if (style.textContent !== cssText) style.textContent = cssText;
  return style;
}
