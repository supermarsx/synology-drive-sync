// A just-enough DOM for Vue 2's virtual-DOM patch to run under `node --test`
// without jsdom, which the UI build does not depend on and the lockfile does
// not carry. It implements the node operations Vue's patch uses
// (createElement, createTextNode, createComment, insertBefore, removeChild,
// appendChild, parentNode, nextSibling, tagName, textContent, attributes and
// listeners) and nothing else, so a test that needs layout, selectors or
// events must use a real browser.
//
// installFakeDom() must run before `vue` is first imported: Vue decides whether
// it is in a browser at module load.

class FakeNode {
  constructor(nodeType) {
    this.nodeType = nodeType;
    this.parentNode = null;
    this.childNodes = [];
  }

  get firstChild() { return this.childNodes[0] || null; }

  get nextSibling() {
    if (!this.parentNode) return null;
    const siblings = this.parentNode.childNodes;
    return siblings[siblings.indexOf(this) + 1] || null;
  }

  insertBefore(node, reference) {
    if (node.parentNode) node.parentNode.removeChild(node);
    const index = reference ? this.childNodes.indexOf(reference) : this.childNodes.length;
    if (reference && index < 0) throw new Error("FakeDOM: insertBefore reference is not a child");
    this.childNodes.splice(index, 0, node);
    node.parentNode = this;
    return node;
  }

  appendChild(node) { return this.insertBefore(node, null); }

  removeChild(node) {
    const index = this.childNodes.indexOf(node);
    if (index < 0) throw new Error("FakeDOM: removeChild of a node that is not a child");
    this.childNodes.splice(index, 1);
    node.parentNode = null;
    return node;
  }

  get textContent() {
    return this.childNodes.map((child) => child.textContent).join("");
  }

  set textContent(value) {
    for (const child of this.childNodes) child.parentNode = null;
    this.childNodes = [];
    if (value !== "") this.appendChild(new FakeText(String(value)));
  }
}

class FakeText extends FakeNode {
  constructor(data) { super(3); this.data = data; }
  get textContent() { return this.data; }
  set textContent(value) { this.data = String(value); }
}

class FakeComment extends FakeNode {
  constructor(data) { super(8); this.data = data; }
  get textContent() { return ""; }
  set textContent(value) { this.data = String(value); }
}

class FakeElement extends FakeNode {
  constructor(tagName) {
    super(1);
    this.tagName = String(tagName).toUpperCase();
    this.attributes = new Map();
    this.listeners = new Map();
    this.style = {};
  }

  setAttribute(name, value) { this.attributes.set(name, String(value)); }
  getAttribute(name) { return this.attributes.has(name) ? this.attributes.get(name) : null; }
  removeAttribute(name) { this.attributes.delete(name); }
  hasAttribute(name) { return this.attributes.has(name); }
  addEventListener(name, handler) { this.listeners.set(name, handler); }
  removeEventListener(name) { this.listeners.delete(name); }
  get children() { return this.childNodes.filter((node) => node.nodeType === 1); }
}

export function installFakeDom() {
  const document = {
    createElement: (tag) => new FakeElement(tag),
    createElementNS: (_namespace, tag) => new FakeElement(tag),
    createTextNode: (text) => new FakeText(text),
    createComment: (text) => new FakeComment(text),
    createEvent: () => ({ timeStamp: 0 }),
    querySelector: () => null,
    addEventListener() {},
    removeEventListener() {},
    hidden: false
  };
  const window = {
    document,
    navigator: { userAgent: "fake-dom Chrome/88" },
    performance: { now: () => 1 },
    addEventListener() {},
    removeEventListener() {},
    setTimeout,
    clearTimeout
  };
  globalThis.window = window;
  globalThis.document = document;
  return { document, window, FakeElement };
}
