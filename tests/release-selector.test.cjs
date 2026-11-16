"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");

const selectorData = require("../docs/theme/release-selector-data.js");
const selector = require("../docs/theme/release-selector.js");

const selectorSource = fs.readFileSync(
    path.join(__dirname, "..", "docs", "theme", "release-selector.js"),
    "utf8",
);

function mutableSelectorData() {
    return JSON.parse(JSON.stringify(selectorData));
}

function loadSelectorWithData(data) {
    const context = vm.createContext({ SDSYNC_RELEASE_SELECTOR_DATA: data });
    vm.runInContext(selectorSource, context, { filename: "release-selector.js" });
    return context.SDSyncReleaseSelector;
}

function resolve(overrides) {
    const supplied = overrides || {};
    const dsmVersion = Object.hasOwn(supplied, "dsmVersion")
        ? supplied.dsmVersion
        : "7.2.2-72806";
    const parsed = selector.parseDsmVersion(dsmVersion);

    return selector.resolveSelection({
        purpose: "dsm-spk",
        model: "DS419slim",
        productLine: "dsm",
        osVersion: parsed && parsed.major === 7 ? `dsm-7.${parsed.minor}` : "dsm-7.2",
        dsmVersion,
        reportedArch: "ARMv7 Processor rev 1 (v7l)",
        ...supplied,
    });
}

function createFakeElement(tagName) {
    return {
        tagName: String(tagName).toUpperCase(),
        id: "",
        className: "",
        attributes: new Map(),
        children: [],
        dataset: {},
        disabled: false,
        focused: false,
        hidden: false,
        listeners: new Map(),
        readOnly: false,
        textContent: "",
        value: "",
        addEventListener(type, listener) {
            const listeners = this.listeners.get(type) || [];
            listeners.push(listener);
            this.listeners.set(type, listeners);
        },
        append(...children) {
            for (const child of children) {
                if (child && child.isFragment) {
                    this.children.push(...child.children);
                } else {
                    this.children.push(child);
                }
            }
        },
        closest(selectorText) {
            return selectorText === "[role=option]" && this.attributes.get("role") === "option"
                ? this
                : null;
        },
        dispatch(type, properties) {
            const event = {
                defaultPrevented: false,
                preventDefault() {
                    event.defaultPrevented = true;
                },
                results: [],
                ...(properties || {}),
            };

            for (const listener of this.listeners.get(type) || []) {
                event.results.push(listener(event));
            }
            return event;
        },
        focus() {
            this.focused = true;
        },
        getAttribute(name) {
            return this.attributes.has(name) ? this.attributes.get(name) : null;
        },
        querySelector() {
            return null;
        },
        querySelectorAll() {
            return [];
        },
        removeAttribute(name) {
            this.attributes.delete(name);
        },
        replaceChildren(...children) {
            this.children = children;
        },
        scrollIntoView() {},
        setAttribute(name, value) {
            this.attributes.set(name, String(value));
        },
    };
}

function elementText(element) {
    return [element.textContent, ...(element.children || []).map(elementText)].join(" ");
}

function createSelectorDomHarness() {
    const names = [
        "purpose",
        "modelInput",
        "modelValue",
        "listbox",
        "modelResults",
        "modelCount",
        "modelFact",
        "osVersion",
        "osAuto",
        "dsmVersion",
        "archInput",
        "archSource",
        "archOverride",
        "productLine",
        "status",
        "result",
        "dsmFields",
        "desktopFields",
        "form",
        "container",
    ];
    const els = Object.fromEntries(names.map((name) => [name, createFakeElement("div")]));
    const documentRef = {
        defaultView: {
            FormData: class FakeFormData {
                constructor(form) {
                    assert.equal(form, els.form);
                    this.values = new Map([
                        ["purpose", els.purpose.value],
                        ["model", els.modelValue.value],
                        ["productLine", els.productLine.value],
                        ["osVersion", els.osVersion.value],
                        ["dsmVersion", els.dsmVersion.value],
                        ["reportedArch", els.archInput.value],
                        ["desktopOs", "linux"],
                        ["desktopCpu", "x86_64"],
                    ]);
                }

                get(name) {
                    return this.values.has(name) ? this.values.get(name) : null;
                }
            },
        },
        createDocumentFragment() {
            const fragment = createFakeElement("fragment");
            fragment.isFragment = true;
            return fragment;
        },
        createElement: createFakeElement,
        querySelectorAll(selectorText) {
            return selectorText === "[data-release-selector]" ? [els.container] : [];
        },
    };

    els.purpose.value = "dsm-spk";
    els.osVersion.value = "auto";
    els.productLine.value = "auto";
    els.archOverride.hidden = true;
    els.listbox.id = "synology-model-list";
    els.listbox.hidden = true;
    els.modelInput.disabled = true;
    for (const element of Object.values(els)) {
        element.ownerDocument = documentRef;
    }

    els.form.querySelector = function querySelector(selectorText) {
        return selectorText === "[name=purpose]" ? els.purpose : null;
    };
    els.dsmFields.querySelectorAll = function querySelectorAll(selectorText) {
        return selectorText === "input, select" ? [els.modelInput] : [];
    };
    els.container.querySelector = function querySelector(selectorText) {
        return new Map([
            ["form", els.form],
            ["[data-model-input]", els.modelInput],
            ["[data-model-select]", els.modelValue],
            ["[data-model-listbox]", els.listbox],
            ["[data-model-results]", els.modelResults],
            ["[data-model-count]", els.modelCount],
            ["[data-model-fact]", els.modelFact],
            ["[data-os-version]", els.osVersion],
            ["[data-os-auto]", els.osAuto],
            ["[data-dsm-version]", els.dsmVersion],
            ["[data-arch-input]", els.archInput],
            ["[data-arch-source]", els.archSource],
            ["[data-arch-override]", els.archOverride],
            ["[data-selector-status]", els.status],
            ["[data-selector-result]", els.result],
            ["[data-dsm-fields]", els.dsmFields],
            ["[data-desktop-fields]", els.desktopFields],
        ]).get(selectorText) || null;
    };

    return {
        ...els,
        documentRef,
        optionValues() {
            return els.listbox.children
                .filter((child) => child.getAttribute("role") === "option")
                .map((child) => child.dataset.value);
        },
        options() {
            return els.listbox.children.filter((child) => child.getAttribute("role") === "option");
        },
        type(text) {
            els.modelInput.value = text;
            els.modelInput.dispatch("input");
        },
        key(name) {
            return els.modelInput.dispatch("keydown", { key: name });
        },
        async submit() {
            const event = els.form.dispatch("submit");
            await Promise.all(event.results);
        },
    };
}

test("selector form remains one raw HTML block for mdBook", () => {
    const source = fs.readFileSync(
        path.join(__dirname, "..", "docs", "release-selector.md"),
        "utf8",
    );
    const block = source.match(
        /(<div class="release-selector" data-release-selector>[\s\S]*?<\/div>)\r?\n\r?\n<noscript>/,
    );

    assert.ok(block, "release selector raw HTML block is missing");
    assert.doesNotMatch(block[1], /\r?\n\s*\r?\n/);
    assert.doesNotMatch(block[1], /\r?\n {4}/);
    for (const control of [
        'name="purpose"',
        'name="model"',
        'name="productLine"',
        'name="osVersion"',
        'name="dsmVersion"',
        'name="reportedArch"',
        'name="desktopOs"',
        'name="desktopCpu"',
        "data-selector-result",
    ]) {
        assert.match(block[1], new RegExp(control));
    }
    assert.match(block[1], /Required for DSM 7\.0 and 7\.4/);
    assert.match(block[1], /optional for DSM 7\.1–7\.3/);
    // One accessible combobox plus a hidden field that only an explicit choice can set.
    assert.match(
        block[1],
        /<input\b(?=[^>]*\bid="synology-model")(?=[^>]*\brole="combobox")(?=[^>]*\baria-autocomplete="list")(?=[^>]*\baria-expanded="false")(?=[^>]*\baria-controls="synology-model-list")(?=[^>]*\bdata-model-input\b)[^>]*>/,
    );
    assert.match(
        block[1],
        /<input\b(?=[^>]*\btype="hidden")(?=[^>]*\bname="model")(?=[^>]*\bvalue=""\s)(?=[^>]*\bdata-model-select\b)[^>]*>/,
    );
    assert.match(
        block[1],
        /<ul\b(?=[^>]*\bid="synology-model-list")(?=[^>]*\brole="listbox")(?=[^>]*\bdata-model-listbox\b)[^>]*>/,
    );
    assert.doesNotMatch(block[1], /<select\b[^>]*\bname="model"/);
    assert.match(block[1], /data-model-count/);
    assert.match(block[1], /aria-live="polite" data-model-results/);
    assert.match(block[1], /<noscript>[^<]*JavaScript[^<]*<\/noscript>/);
    assert.doesNotMatch(block[1], /<datalist\b|\blist="synology-models"/);
    // Automatic is the default for the product line and DSM branch, with a resolved-value line.
    assert.match(block[1], /<select id="product-line"[^>]*>\s*<option value="auto" selected>/);
    assert.match(block[1], /<select id="os-version"[^>]*data-os-version[^>]*>\s*<option value="auto" selected>/);
    assert.match(block[1], /data-os-auto aria-live="polite"/);
    // The architecture is filled from the model and can be overridden.
    assert.match(block[1], /\bname="reportedArch"[^>]*data-arch-input/);
    assert.match(block[1], /data-arch-source aria-live="polite"/);
    assert.match(block[1], /<button type="button" class="selector-secondary" data-arch-override hidden>/);
    assert.match(source, /searches all 233 exact model names/);
    assert.doesNotMatch(source, /native, visibly populated select/);
    assert.match(source, /Free\s+text never resolves to a nearest-looking model/);
    const submitButton = block[1].match(/<button type="submit">([\s\S]*?)<\/button>/);
    assert.ok(submitButton, "release selector submit button is missing");
    assert.match(submitButton[1], /<svg\b[^>]*class="selector-action-icon"/);
    assert.match(submitButton[1], /\bstroke="currentColor"/);
    assert.match(submitButton[1], /\baria-hidden="true"/);
    assert.match(submitButton[1], /\bfocusable="false"/);
    assert.match(submitButton[1], /Find my release/);
    assert.match(
        source,
        /DSM package safety hold:[\s\S]*releases `26\.5`, `26\.6`, or[\s\S]*`26\.20`/,
    );
    assert.match(
        source,
        /\[GitHub Releases\]\(https:\/\/github\.com\/supermarsx\/synology-drive-sync\/releases\)/,
    );
});

test("captured official table has unique normalized model mappings", () => {
    const models = selector.listModels();
    const normalized = new Set(models.map((record) => record.model.toUpperCase()));

    assert.equal(selectorData.snapshotCapturedDate, "2026-08-26");
    assert.equal(
        selectorData.sourceUrl,
        "https://kb.synology.com/en-us/DSM/tutorial/What_kind_of_CPU_does_my_NAS_have",
    );
    assert.equal(models.length, 233);
    assert.equal(normalized.size, models.length);
    assert.equal(selectorData.compatibilityGroups.flatMap((group) => group.models).length, 233);
    assert.match(selectorData.lifecycleSourceUrl, /last_upgradable_software_version/);
    assert.equal(selectorData.archiveSources.length, 5);
    assert.equal(selectorData.supplementalModelSources.length, 2);
    assert.match(selectorData.supplementalModelSources[0].url, /DSM\/7\.4\.1-90080$/);
    assert.match(selectorData.supplementalModelSources[1].url, /SynoOnlinePack_v2\/1071$/);
    assert.deepEqual(selector.lookupModel(" ds419SLIM "), {
        model: "DS419slim",
        cpuArch: "armv7",
        packageArch: "armada38x",
        assetArch: "armv7",
    });
    assert.deepEqual(selector.modelCompatibility("DS419slim"), {
        productLine: "DSM",
        status: "supported",
        minMinor: 0,
        maxMinor: 4,
        minPatch: null,
        minBuild: null,
        lastVersion: null,
    });
});

test("every model group declares an explicit package-to-asset mapping", () => {
    const expectedAssetByPackage = new Map([
        ["88f628x", null],
        ["alpine", "armv7"],
        ["alpine4k", "armv7"],
        ["armada370", "armv7"],
        ["armada375", "armv7"],
        ["armada37xx", "armv8"],
        ["armada38x", "armv7"],
        ["armadaxp", "armv7"],
        ["apollolake", "x86_64"],
        ["avoton", "x86_64"],
        ["braswell", "x86_64"],
        ["broadwell", "x86_64"],
        ["broadwellnk", "x86_64"],
        ["broadwellnkv2", "x86_64"],
        ["broadwellntbap", "x86_64"],
        ["bromolow", "x86_64"],
        ["cedarview", "x86_64"],
        ["comcerto2k", "armv7"],
        ["denverton", "x86_64"],
        ["epyc7002", "x86_64"],
        ["epyc7003", "x86_64"],
        ["evansport", "i686"],
        ["geminilake", "x86_64"],
        ["geminilakenk", "x86_64"],
        ["grantley", "x86_64"],
        ["icelaked", "x86_64"],
        ["monaco", "armv7"],
        ["ppc853x", null],
        ["purley", "x86_64"],
        ["qoriq", null],
        ["r1000", "x86_64"],
        ["r1000nk", "x86_64"],
        ["rtd1296", "armv8"],
        ["rtd1619b", "armv8"],
        ["v1000", "x86_64"],
        ["v1000nk", "x86_64"],
        ["x86", null],
    ]);
    const expectedModelOverrides = new Map([["PAS7700", null]]);
    const actualPackages = new Set();

    for (const group of selectorData.modelGroups) {
        assert.equal(
            Object.hasOwn(group, "assetArch"),
            true,
            `${group.packageArch} must declare assetArch instead of deriving it from cpuArch`,
        );
        actualPackages.add(group.packageArch);

        for (const model of group.models) {
            const expectedAssetArch = expectedModelOverrides.has(model)
                ? expectedModelOverrides.get(model)
                : expectedAssetByPackage.get(group.packageArch);

            assert.equal(group.assetArch, expectedAssetArch, `${model}/${group.packageArch}`);
            assert.deepEqual(selector.lookupModel(model), {
                model,
                cpuArch: group.cpuArch,
                packageArch: group.packageArch,
                assetArch: group.assetArch,
            });
        }
    }

    assert.deepEqual(actualPackages, new Set(expectedAssetByPackage.keys()));
});

test("selector rejects missing, processor-derived, and conflicting asset mappings", () => {
    const missing = mutableSelectorData();
    delete missing.modelGroups[0].assetArch;
    assert.throws(
        () => loadSelectorWithData(missing),
        /invalid Synology model snapshot group/,
    );

    const processorDerived = mutableSelectorData();
    processorDerived.modelGroups.find((group) => group.packageArch === "88f628x").assetArch =
        "armv7";
    assert.throws(
        () => loadSelectorWithData(processorDerived),
        /invalid Synology model snapshot mapping: 88f628x/,
    );

    const packageConflict = mutableSelectorData();
    packageConflict.modelGroups.push({
        cpuArch: "armv7",
        packageArch: "epyc7003",
        assetArch: "armv7",
        models: ["CONFLICT-TEST-MODEL"],
    });
    assert.throws(
        () => loadSelectorWithData(packageConflict),
        /conflicting DSM package asset mapping: epyc7003/,
    );
});

test("catalog preserves representative real model names and their exact mappings", () => {
    const expectedModels = new Map([
        ["DS419slim", ["armv7", "armada38x", "armv7"]],
        ["DS223", ["armv8", "rtd1619b", "armv8"]],
        ["DS923+", ["x86_64", "r1000", "x86_64"]],
        ["DS214play", ["i686", "evansport", "i686"]],
        ["DVA3221", ["x86_64", "denverton", "x86_64"]],
        ["DVA7400", ["x86_64", "v1000nk", "x86_64"]],
        ["FS6420", ["x86_64", "epyc7003", "x86_64"]],
        ["HD6500", ["x86_64", "purley", "x86_64"]],
        ["RS2423RP+II", ["x86_64", "v1000nk", "x86_64"]],
        ["RS11626xs+", ["x86_64", "epyc7003", "x86_64"]],
        ["SA6400", ["x86_64", "epyc7002", "x86_64"]],
        ["PAS7700", ["x86_64", "epyc7003", null]],
        ["DS212j", ["armv5", "88f628x", null]],
        ["DS213+", ["ppc", "qoriq", null]],
    ]);

    for (const [model, [cpuArch, packageArch, assetArch]] of expectedModels) {
        assert.deepEqual(selector.lookupModel(model), {
            model,
            cpuArch,
            packageArch,
            assetArch,
        });
    }
});

test("model lifecycle and explicit asset mapping partition every selectable model", () => {
    const counts = {
        x86_64: 0,
        armv8: 0,
        armv7: 0,
        i686: 0,
        manual: 0,
    };

    for (const model of selector.listModels()) {
        const compatibility = selector.modelCompatibility(model);
        const bucket = compatibility.status === "supported" && model.assetArch
            ? model.assetArch
            : "manual";

        counts[bucket] += 1;
    }

    assert.deepEqual(counts, {
        x86_64: 133,
        armv8: 14,
        armv7: 32,
        i686: 2,
        manual: 52,
    });
});

function chooseModel(harness, text, value) {
    harness.type(text);
    const option = harness.options().find((candidate) => candidate.dataset.value === value);

    assert.ok(option, `${value} must be offered for "${text}"`);
    harness.listbox.dispatch("click", { target: option });
}

test("selector initialization builds an ARIA combobox over every exact model with processor, package, and asset facts", () => {
    const harness = createSelectorDomHarness();
    const models = selector.listModels();

    selector.initSelectors(harness.documentRef, null);

    assert.equal(harness.modelInput.disabled, false);
    assert.equal(harness.modelInput.getAttribute("aria-expanded"), "false");
    assert.equal(harness.listbox.hidden, true);
    assert.equal(harness.modelCount.textContent, "233 exact models loaded");
    assert.equal(harness.modelValue.value, "");

    harness.modelInput.dispatch("focus");
    assert.equal(harness.listbox.hidden, false);
    assert.equal(harness.modelInput.getAttribute("aria-expanded"), "true");
    assert.equal(harness.options().length, 61, "60 rendered models plus the manual-review option");
    assert.match(elementText(harness.listbox), /Showing 60 of 233 models/);

    // Typing a unique prefix renders each remaining option with its facts.
    for (const model of models) {
        const compatibility = selector.modelCompatibility(model);
        const assetLabel = compatibility.status === "supported" && model.assetArch
            ? `synology-drive-sync-YY.N-${model.assetArch}.spk`
            : "manual review / no SPK";

        harness.type(model.model);
        const option = harness.options().find((candidate) => candidate.dataset.value === model.model);

        assert.ok(option, model.model);
        assert.equal(option.getAttribute("role"), "option", model.model);
        assert.equal(
            elementText(option).includes(
                `processor ${model.cpuArch} · Package Arch ${model.packageArch} · ${assetLabel}`,
            ),
            true,
            model.model,
        );
        assert.equal(harness.options().at(-1).dataset.value, "__unknown__", model.model);
        assert.equal(harness.modelValue.value, "", "typing must never select");
    }

    selector.initSelectors(harness.documentRef, null);
    harness.type("");
    assert.equal(harness.options().length, 61, "initialization must be idempotent");
});

test("typing filters with fuzzy-normalised matching but never selects; Enter needs an active option or an exact name", () => {
    const harness = createSelectorDomHarness();

    selector.initSelectors(harness.documentRef, null);

    harness.type("ds416");
    assert.deepEqual(harness.optionValues(), [
        "DS416",
        "DS416j",
        "DS416play",
        "DS416slim",
        "__unknown__",
    ]);
    assert.equal(harness.modelValue.value, "");
    assert.match(harness.modelResults.textContent, /4 of 233 models match/);

    harness.type("DS 416");
    assert.equal(harness.optionValues()[0], "DS416");
    harness.type("rs11626");
    assert.deepEqual(harness.optionValues(), ["RS11626xs+", "__unknown__"]);
    harness.type("zzzz");
    assert.deepEqual(harness.optionValues(), ["__unknown__"]);
    assert.match(harness.modelResults.textContent, /No catalog model matches/);

    // Partial text plus Enter does nothing: free text never resolves to a nearest model.
    harness.type("ds41");
    const partial = harness.key("Enter");
    assert.equal(partial.defaultPrevented, true);
    assert.equal(harness.modelValue.value, "");
    assert.equal(harness.archInput.value, "");

    // Text that is exactly one catalog model name may be committed with Enter.
    harness.type("ds 416");
    harness.key("Enter");
    assert.equal(harness.modelValue.value, "DS416");
    assert.equal(harness.modelInput.value, "DS416");
    assert.equal(harness.listbox.hidden, true);

    // Editing the chosen text clears the choice and the fields derived from it.
    harness.type("DS416x");
    assert.equal(harness.modelValue.value, "");
    assert.equal(harness.archInput.value, "");
    assert.equal(harness.archInput.readOnly, false);
});

test("combobox keyboard, mouse, and touch selection follow the ARIA 1.2 pattern", () => {
    const harness = createSelectorDomHarness();

    selector.initSelectors(harness.documentRef, null);
    harness.type("ds416");

    const down = harness.key("ArrowDown");
    assert.equal(down.defaultPrevented, true);
    assert.equal(harness.modelInput.getAttribute("aria-activedescendant"), harness.options()[0].id);
    assert.equal(harness.options()[0].getAttribute("aria-selected"), "true");
    harness.key("ArrowUp");
    assert.equal(
        harness.modelInput.getAttribute("aria-activedescendant"),
        harness.options().at(-1).id,
        "ArrowUp wraps to the last option",
    );
    harness.key("ArrowDown");
    harness.key("ArrowDown");
    harness.key("Enter");
    assert.equal(harness.modelValue.value, "DS416j");
    assert.equal(harness.modelInput.getAttribute("aria-expanded"), "false");

    // Escape closes an open list, then clears the text and the choice.
    harness.modelInput.dispatch("focus");
    assert.equal(harness.listbox.hidden, false);
    harness.key("Escape");
    assert.equal(harness.listbox.hidden, true);
    assert.equal(harness.modelValue.value, "DS416j", "the first Escape only closes the list");
    harness.key("Escape");
    assert.equal(harness.modelInput.value, "");
    assert.equal(harness.modelValue.value, "");

    // Pointer and touch taps arrive as mousedown (focus is kept) then click.
    harness.type("ds416");
    const mouseDown = harness.listbox.dispatch("mousedown");
    assert.equal(mouseDown.defaultPrevented, true);
    harness.listbox.dispatch("click", { target: harness.options()[0] });
    assert.equal(harness.modelValue.value, "DS416");
    assert.equal(harness.modelInput.focused, true);

    // Blur restores the chosen label and an unchosen draft stays unselected.
    harness.type("ds4");
    harness.modelInput.dispatch("blur");
    assert.equal(harness.modelValue.value, "");
    assert.equal(harness.listbox.hidden, true);
});

test("choosing DS416 auto-fills armv7 with its source, and Override can disagree with the catalog", async () => {
    const harness = createSelectorDomHarness();

    selector.initSelectors(harness.documentRef, null);
    chooseModel(harness, "ds416", "DS416");

    assert.equal(harness.modelValue.value, "DS416");
    assert.equal(harness.archInput.value, "armv7");
    assert.equal(harness.archInput.readOnly, true);
    assert.equal(harness.archInput.dataset.autofilled, "true");
    assert.equal(harness.archOverride.hidden, false);
    assert.equal(harness.archOverride.textContent, "Override");
    assert.match(
        harness.archSource.textContent,
        /Auto-filled from Synology's CPU table, captured 2026-08-26: DS416 is Package Arch alpine4k, which maps to the armv7 SPK \(synology-drive-sync-YY\.N-armv7\.spk\)/,
    );
    assert.match(harness.osAuto.textContent, /Automatic: DSM 7\.4 for this model/);
    assert.match(harness.osAuto.textContent, /No exact build is needed/);

    // DS416 + Automatic resolves to the armv7 SPK with no build entered.
    assert.equal(harness.dsmVersion.value, "");
    await harness.submit();
    assert.equal(harness.result.dataset.state, "fallback");
    assert.match(elementText(harness.result), /synology-drive-sync-YY\.N-armv7\.spk/);
    assert.match(elementText(harness.result), /DSM 7\.4 \(Automatic: newest for this model; exact build not supplied\)/);

    // An exact build sets the branch, and the whole path still resolves to the armv7 SPK.
    harness.dsmVersion.value = "7.2.2-72806";
    harness.dsmVersion.dispatch("input");
    assert.match(harness.osAuto.textContent, /Automatic: DSM 7\.2, read from the exact build 7\.2\.2-72806/);
    await harness.submit();
    assert.equal(harness.result.dataset.state, "fallback");
    assert.match(elementText(harness.result), /synology-drive-sync-YY\.N-armv7\.spk/);
    assert.match(elementText(harness.result), /Release alpine4k|Package Arch alpine4k/);

    // Override, then a mismatch with the catalog stops the selector as before.
    harness.archOverride.dispatch("click");
    assert.equal(harness.archInput.readOnly, false);
    assert.equal(harness.archOverride.textContent, "Use the auto-filled value");
    assert.match(harness.archSource.textContent, /Manual override/);
    harness.archInput.value = "x86_64";
    await harness.submit();
    assert.equal(harness.result.dataset.state, "blocked");
    assert.match(elementText(harness.result), /says armv7\/alpine4k, but the reported architecture resolves to x86_64/);

    // The auto-filled value comes back on request.
    harness.archOverride.dispatch("click");
    assert.equal(harness.archInput.value, "armv7");
    assert.equal(harness.archInput.readOnly, true);

    // Picking a branch by hand remains possible.
    harness.osVersion.value = "dsm-7.1";
    harness.osVersion.dispatch("change");
    harness.dsmVersion.value = "";
    assert.equal(harness.osAuto.textContent, "");
    await harness.submit();
    assert.equal(harness.result.dataset.state, "fallback");
});

test("manual-review and no-SPK models auto-fill nothing and keep the stop", async () => {
    const harness = createSelectorDomHarness();

    selector.initSelectors(harness.documentRef, null);

    chooseModel(harness, "ds416", "DS416");
    assert.equal(harness.archInput.value, "armv7");

    // The explicit manual-review option clears the auto-filled value.
    harness.type("my brand new nas");
    harness.listbox.dispatch("click", {
        target: harness.options().find((option) => option.dataset.value === "__unknown__"),
    });
    assert.equal(harness.modelValue.value, "__unknown__");
    assert.equal(harness.modelInput.value, "My model is not listed — manual review / no SPK");
    assert.equal(harness.archInput.value, "");
    assert.equal(harness.archInput.readOnly, false);
    assert.equal(harness.archOverride.hidden, true);
    assert.match(harness.archSource.textContent, /Nothing auto-filled/);
    harness.archInput.value = "armv7l";
    await harness.submit();
    assert.equal(harness.result.dataset.state, "blocked");
    assert.match(elementText(harness.result), /not in the captured physical Synology model catalog/);

    // A catalog model with no DSM 7 package fills nothing and stops with its lifecycle message.
    harness.archInput.value = "";
    chooseModel(harness, "ds1511", "DS1511+");
    assert.equal(harness.archInput.value, "");
    assert.match(harness.archSource.textContent, /DS1511\+ is manual review \/ no SPK/);
    assert.match(harness.osAuto.textContent, /Automatic stops: DS1511\+ cannot run DSM 7/);
    harness.archInput.value = "x86_64";
    await harness.submit();
    assert.equal(harness.result.dataset.state, "blocked");
    assert.match(elementText(harness.result), /last upgradable DSM as 6\.2/);

    // With nothing chosen at all, the selector refuses instead of guessing from typed text.
    harness.type("DS416x");
    assert.equal(harness.modelValue.value, "");
    await harness.submit();
    assert.equal(harness.result.dataset.state, "blocked");
    assert.match(elementText(harness.result), /typed text alone never selects a model/);
});

test("ARMv7-A model resolves to the shared hard-float SPK", () => {
    const result = resolve({});

    assert.equal(result.ok, true);
    assert.equal(result.assetTemplate, "synology-drive-sync-{tag}-armv7.spk");
    assert.match(result.detected, /armada38x/);
});

test("all four DSM 7 binary families resolve to their exact asset suffix", () => {
    const cases = [
        ["DS923+", "7.2.2-72806", "x86_64", "x86_64"],
        ["DS223", "DSM 7.2.2-72806 Update 4", "aarch64", "armv8"],
        ["DS419slim", "7.2.2-72806", "armv7l", "armv7"],
        ["DS214play", "7.1.1-42962", "i686", "i686"],
    ];

    for (const [model, dsmVersion, reportedArch, assetArch] of cases) {
        const result = resolve({ model, dsmVersion, reportedArch });

        assert.equal(result.ok, true, `${model} should resolve`);
        assert.equal(
            result.assetTemplate,
            `synology-drive-sync-{tag}-${assetArch}.spk`,
        );
    }
});

test("official DSM 7.0 through 7.4 platform intervals are complete", () => {
    const rangeGroups = [
        [
            { min: 0, max: 1 },
            ["armada370", "armada375", "armadaxp", "cedarview", "comcerto2k", "evansport"],
        ],
        [
            { min: 0, max: 4 },
            [
                "alpine",
                "alpine4k",
                "apollolake",
                "armada37xx",
                "armada38x",
                "avoton",
                "braswell",
                "broadwell",
                "broadwellnk",
                "bromolow",
                "denverton",
                "geminilake",
                "grantley",
                "monaco",
                "purley",
                "rtd1296",
                "v1000",
            ],
        ],
        [
            { min: 1, max: 4 },
            ["broadwellnkv2", "epyc7002", "r1000", "rtd1619b"],
        ],
        [
            { min: 2, max: 4 },
            ["broadwellntbap", "geminilakenk", "r1000nk", "v1000nk"],
        ],
        [{ min: 3, max: 4 }, ["epyc7003", "icelaked"]],
    ];
    const expectedPackageArchs = new Set();

    for (const [bounds, packageArchs] of rangeGroups) {
        for (const packageArch of packageArchs) {
            assert.deepEqual(selector.dsmMinorBounds(packageArch), bounds, packageArch);
            expectedPackageArchs.add(packageArch);
        }
    }

    assert.equal(expectedPackageArchs.size, 33);
    assert.equal(selector.dsmMinorBounds("88f628x"), null);
    assert.equal(selector.dsmMinorBounds("epyc7003ntb"), null);
});

test("model-backed DSM platform introductions fail before 7.1 and 7.2", () => {
    const cases = [
        ["FS3410", "broadwellnkv2", 1],
        ["DS225+", "geminilakenk", 2],
    ];

    for (const [model, packageArch, firstMinor] of cases) {
        const before = resolve({
            model,
            dsmVersion: `7.${firstMinor - 1}-99999`,
            reportedArch: "x86_64",
        });
        const introduced = resolve({
            model,
            dsmVersion: `7.${firstMinor}-99999`,
            reportedArch: "x86_64",
        });

        assert.equal(before.code, "model_dsm_conflict", `${packageArch} before DSM 7.${firstMinor}`);
        assert.equal(introduced.ok, true, `${packageArch} on DSM 7.${firstMinor}`);
        assert.match(introduced.detected, new RegExp(`Toolkit DSM 7\\.${firstMinor}–7\\.4`));
    }
});

test("all 233 models have one explicit product lifecycle", () => {
    const counts = new Map();

    for (const model of selector.listModels()) {
        const compatibility = selector.modelCompatibility(model);
        const key = compatibility.status === "supported"
            ? `${compatibility.productLine}:supported:${compatibility.minMinor}-${compatibility.maxMinor}`
            : `${compatibility.productLine}:${compatibility.status}:${compatibility.lastVersion}`;

        assert.ok(compatibility, model.model);
        counts.set(key, (counts.get(key) || 0) + 1);
    }

    assert.deepEqual(Object.fromEntries(counts), {
        "DSM:legacy:5.2": 10,
        "DSM:supported:0-4": 92,
        "DSM:legacy:6.2": 41,
        "DSM:supported:0-1": 42,
        "DSM:supported:2-4": 22,
        "DSM:supported:1-4": 7,
        "DSM:supported:4-4": 18,
        "DSM Enterprise:unsupported-product-line:1.0": 1,
    });
});

test("model-specific DSM introduction and retirement bounds fail closed", () => {
    const cases = [
        ["DS415+", "dsm-7.2", "7.2.2-72806", "x86_64", false],
        ["DS1522+", "dsm-7.0", "7.0.1-42218", "x86_64", false],
        ["DS1522+", "dsm-7.1", "7.1.1-42962", "x86_64", true],
        ["DS124", "dsm-7.1", "7.1.1-42962", "aarch64", false],
        ["DS124", "dsm-7.2", "7.2.2-72806", "aarch64", true],
        ["FS3420", "dsm-7.3", "7.3-81180", "x86_64", false],
        ["FS3420", "dsm-7.4", "7.4-90075", "x86_64", true],
        ["DS925neo+", "dsm-7.3", "7.3-81180", "x86_64", false],
        ["DS925neo+", "dsm-7.4", "7.4.1-90080", "x86_64", true],
        ["DVA7400", "dsm-7.3", "7.3-81180", "x86_64", false],
        ["DVA7400", "dsm-7.4", "7.4.1-90080", "x86_64", true],
        ["RS11626xs+", "dsm-7.3", "7.3-81180", "x86_64", false],
        ["RS11626xs+", "dsm-7.4", "7.4.1-90080", "x86_64", true],
    ];

    for (const [model, osVersion, dsmVersion, reportedArch, supported] of cases) {
        const result = resolve({ model, osVersion, dsmVersion, reportedArch });

        assert.equal(result.ok, supported, `${model} on ${osVersion}`);
        if (!supported) assert.equal(result.code, "model_dsm_conflict");
    }
});

test("models first published in DSM 7.4.1 enforce their exact build floor", () => {
    for (const model of [
        "DS1525neo+",
        "DS1825neo+",
        "DS725neo+",
        "DS925neo+",
        "DVA7400",
        "RS11626xs+",
    ]) {
        const before = resolve({
            model,
            osVersion: "dsm-7.4",
            dsmVersion: "7.4-99999",
            reportedArch: "x86_64",
        });
        const beforeBuild = resolve({
            model,
            osVersion: "dsm-7.4",
            dsmVersion: "7.4.1-90079",
            reportedArch: "x86_64",
        });
        const firstPublished = resolve({
            model,
            osVersion: "dsm-7.4",
            dsmVersion: "7.4.1-90080",
            reportedArch: "x86_64",
        });
        const laterPatchWithResetBuild = resolve({
            model,
            osVersion: "dsm-7.4",
            dsmVersion: "7.4.2-1",
            reportedArch: "x86_64",
        });
        const compatibility = selector.modelCompatibility(model);

        assert.equal(before.code, "model_build_too_old", model);
        assert.equal(beforeBuild.code, "model_build_too_old", model);
        assert.equal(firstPublished.ok, true, model);
        assert.equal(laterPatchWithResetBuild.ok, true, model);
        assert.equal(firstPublished.assetTemplate, "synology-drive-sync-{tag}-x86_64.spk");
        assert.equal(compatibility.minMinor, 4);
        assert.equal(compatibility.maxMinor, 4);
        assert.equal(compatibility.minPatch, 1);
        assert.equal(compatibility.minBuild, 90080);
    }
});

test("exact model compatibility is independent from a matching processor family", () => {
    const armv7Current = resolve({
        model: "DS419slim",
        osVersion: "dsm-7.2",
        dsmVersion: "7.2.2-72806",
        reportedArch: "armv7l",
    });
    const armv7Retired = resolve({
        model: "DS114",
        osVersion: "dsm-7.2",
        dsmVersion: "7.2.2-72806",
        reportedArch: "armv7l",
    });
    const x86Current = resolve({
        model: "DS923+",
        osVersion: "dsm-7.2",
        dsmVersion: "7.2.2-72806",
        reportedArch: "x86_64",
    });
    const x86Retired = resolve({
        model: "DS415+",
        osVersion: "dsm-7.2",
        dsmVersion: "7.2.2-72806",
        reportedArch: "x86_64",
    });

    assert.equal(armv7Current.ok, true);
    assert.equal(armv7Retired.code, "model_dsm_conflict");
    assert.equal(x86Current.ok, true);
    assert.equal(x86Retired.code, "model_dsm_conflict");
    assert.equal(selector.lookupModel("DS419slim").cpuArch, selector.lookupModel("DS114").cpuArch);
    assert.equal(selector.lookupModel("DS923+").cpuArch, selector.lookupModel("DS415+").cpuArch);
    assert.match(armv7Retired.details, /checked separately from CPU Package Arch/);
    assert.match(x86Retired.details, /checked separately from CPU Package Arch/);
});

test("PAS7700 remains factual DSM Enterprise data but never receives an ordinary SPK", () => {
    assert.deepEqual(selector.lookupModel("PAS7700"), {
        model: "PAS7700",
        cpuArch: "x86_64",
        packageArch: "epyc7003",
        assetArch: null,
    });

    assert.deepEqual(selector.modelCompatibility("PAS7700"), {
        productLine: "DSM Enterprise",
        status: "unsupported-product-line",
        minMinor: null,
        maxMinor: null,
        minPatch: null,
        minBuild: null,
        lastVersion: "1.0",
    });

    const enterprise = resolve({
        model: "PAS7700",
        productLine: "dsm-enterprise",
        osVersion: "dsm-enterprise-1.0",
        dsmVersion: "",
        reportedArch: "x86_64",
    });
    const wrongLine = resolve({
        model: "PAS7700",
        productLine: "dsm",
        osVersion: "dsm-7.4",
        dsmVersion: "7.4-90075",
        reportedArch: "x86_64",
    });

    assert.equal(enterprise.code, "unsupported_product_line");
    assert.match(enterprise.message, /DSM Enterprise 1\.0/);
    assert.match(enterprise.details, /support\/download\/PAS7700/);
    assert.equal(Object.hasOwn(enterprise, "assetTemplate"), false);
    assert.equal(selector.materializeRecommendation(enterprise, null), enterprise);
    assert.equal(Object.hasOwn(enterprise, "downloadUrl"), false);
    assert.equal(wrongLine.code, "product_line_conflict");
});

test("Evansport resolves to i686 only on its official DSM 7 branches", () => {
    const supported = resolve({
        model: "DS214play",
        dsmVersion: "7.1.1-42962",
        reportedArch: "i686",
    });
    const impossible = resolve({
        model: "DS214play",
        dsmVersion: "7.2-64570",
        reportedArch: "i686",
    });

    assert.equal(supported.ok, true);
    assert.equal(supported.assetTemplate, "synology-drive-sync-{tag}-i686.spk");
    assert.equal(impossible.ok, false);
    assert.equal(impossible.code, "model_dsm_conflict");
});

test("legacy ARMv7 platform branches are not guessed onto DSM 7.2", () => {
    const result = resolve({
        model: "DS114",
        dsmVersion: "7.2-64570",
        reportedArch: "armv7l",
    });

    assert.equal(result.ok, false);
    assert.equal(result.code, "model_dsm_conflict");
});

test("DSM minimum, legacy CPUs, unknown models, and conflicts fail closed", () => {
    assert.equal(resolve({ dsmVersion: "7.0-40758" }).code, "dsm_too_old");
    assert.equal(resolve({ dsmVersion: "7.1-1" }).ok, true);
    assert.equal(resolve({ dsmVersion: "prefix 7.2.2-72806 suffix" }).code, "invalid_dsm_version");
    assert.equal(resolve({ osVersion: "dsm-8.0", dsmVersion: "8.0-80000" }).code, "invalid_os_version_selection");
    assert.equal(resolve({ osVersion: "dsm-7.5", dsmVersion: "7.5-80000" }).code, "invalid_os_version_selection");
    assert.equal(
        resolve({ model: "DS212j", dsmVersion: "7.0-41890", reportedArch: "armv5" })
            .code,
        "model_dsm_conflict",
    );
    assert.equal(
        resolve({ model: "DS213+", dsmVersion: "7.0-41890", reportedArch: "ppc" })
            .code,
        "model_dsm_conflict",
    );
    assert.equal(resolve({ model: "DS99999" }).code, "unknown_model");
    assert.equal(resolve({ reportedArch: "aarch64" }).code, "architecture_conflict");
    assert.equal(resolve({ reportedArch: "aarch64 armv7l" }).code, "unknown_reported_arch");
    assert.equal(resolve({ reportedArch: "x86_64 i686" }).code, "unknown_reported_arch");
    assert.equal(resolve({ reportedArch: "junkarmv7fake" }).code, "unknown_reported_arch");
});

test("unknown models stay blocked while known offline selections use a manual release pattern", () => {
    const unknown = resolve({ model: "DS99999" });
    const unknownMaterialized = selector.materializeRecommendation(unknown, {
        tag_name: "26.11",
        draft: false,
        prerelease: false,
        assets: [
            {
                name: "synology-drive-sync-26.11-armv7.spk",
                state: "uploaded",
                size: 42,
                browser_download_url:
                    "https://github.com/supermarsx/synology-drive-sync/releases/download/26.11/synology-drive-sync-26.11-armv7.spk",
            },
        ],
    });
    const knownOffline = selector.materializeRecommendation(resolve({}), null);

    assert.equal(unknownMaterialized, unknown);
    assert.equal(unknown.code, "unknown_model");
    assert.match(unknown.details, /will not guess/);
    for (const unsafeField of ["assetTemplate", "name", "tag", "downloadUrl"]) {
        assert.equal(Object.hasOwn(unknownMaterialized, unsafeField), false, unsafeField);
    }

    assert.equal(knownOffline.ok, true);
    assert.equal(knownOffline.exact, false);
    assert.equal(knownOffline.name, "synology-drive-sync-YY.N-armv7.spk");
    assert.equal(knownOffline.tag, "YY.N");
    assert.equal(
        knownOffline.downloadUrl,
        "https://github.com/supermarsx/synology-drive-sync/releases/latest",
    );
    assert.doesNotMatch(knownOffline.downloadUrl, /\/releases\/download\//);
    assert.equal(selector.lookupModel("VirtualDSM"), null);
    assert.equal(resolve({ model: "VirtualDSM", reportedArch: "x86_64" }).code, "unknown_model");
});

test("exact DSM 7.4 builds honor the SPK manifest upper bound", () => {
    const missing = resolve({
        model: "FS6420",
        osVersion: "dsm-7.4",
        dsmVersion: "",
        reportedArch: "x86_64",
    });
    const maximum = resolve({
        model: "FS6420",
        osVersion: "dsm-7.4",
        dsmVersion: "7.4-99999",
        reportedArch: "x86_64",
    });
    const aboveMaximum = resolve({
        model: "FS6420",
        osVersion: "dsm-7.4",
        dsmVersion: "7.4-100000",
        reportedArch: "x86_64",
    });

    assert.equal(missing.ok, false);
    assert.equal(missing.code, "dsm_build_required");
    assert.match(missing.message, /DSM 7\.4 requires the exact installed build/);
    assert.match(missing.details, /7\.4-99999/);
    assert.equal(maximum.ok, true);
    assert.equal(maximum.assetTemplate, "synology-drive-sync-{tag}-x86_64.spk");
    assert.equal(aboveMaximum.ok, false);
    assert.equal(aboveMaximum.code, "dsm_too_new");
    assert.match(aboveMaximum.message, /7\.4-99999/);
});

test("OS controls require boundary builds, allow optional middle builds, and reject conflicts", () => {
    const noBuildOn71 = resolve({ dsmVersion: "", osVersion: "dsm-7.1" });
    const noBuildOn72 = resolve({ dsmVersion: "", osVersion: "dsm-7.2" });
    const noBuildOn73 = resolve({ dsmVersion: "", osVersion: "dsm-7.3" });
    const noBuildOn70 = resolve({ dsmVersion: "", osVersion: "dsm-7.0" });
    const minimum70 = resolve({ dsmVersion: "7.0-40759", osVersion: "dsm-7.0" });
    const branchConflict = resolve({
        osVersion: "dsm-7.1",
        dsmVersion: "7.2.2-72806",
    });
    const productConflict = resolve({
        productLine: "dsm-enterprise",
        osVersion: "dsm-7.4",
    });
    const legacy = resolve({
        model: "DS212j",
        productLine: "dsm",
        osVersion: "dsm-6.x-or-earlier",
        dsmVersion: "",
        reportedArch: "armv5",
    });

    for (const middleBranch of [noBuildOn71, noBuildOn72, noBuildOn73]) {
        assert.equal(middleBranch.ok, true);
        assert.match(middleBranch.detected, /exact build not supplied/);
    }
    assert.equal(noBuildOn70.code, "dsm_build_required");
    assert.equal(minimum70.ok, true);
    assert.equal(branchConflict.code, "os_version_conflict");
    assert.equal(productConflict.code, "os_selection_conflict");
    assert.equal(legacy.code, "unsupported_dsm_line");
    assert.match(legacy.details, /desktop CLI or (?:a )?container/i);
});

test("one supported model resolves on every reviewed DSM 7 branch", () => {
    const branches = [
        ["dsm-7.0", "7.0.1-42218"],
        ["dsm-7.1", "7.1.1-42962"],
        ["dsm-7.2", "7.2.2-72806"],
        ["dsm-7.3", "7.3-81180"],
        ["dsm-7.4", "7.4-90075"],
    ];

    for (const [osVersion, dsmVersion] of branches) {
        const result = resolve({ osVersion, dsmVersion });

        assert.equal(result.ok, true, osVersion);
        assert.equal(result.assetTemplate, "synology-drive-sync-{tag}-armv7.spk", osVersion);
        assert.match(result.detected, new RegExp(`DSM ${dsmVersion.replaceAll(".", "\\.")}`));
    }
});

test("desktop CLI and C ABI choose exact OS and CPU filename forms", () => {
    const cli = selector.resolveSelection({
        purpose: "desktop-cli",
        desktopOs: "windows",
        desktopCpu: "arm64",
    });
    const ffi = selector.resolveSelection({
        purpose: "c-abi",
        desktopOs: "macos",
        desktopCpu: "x86_64",
    });

    assert.equal(cli.assetTemplate, "synology-drive-sync-{tag}-windows-aarch64.zip");
    assert.equal(
        ffi.assetTemplate,
        "synology-drive-sync-{tag}-c-sdk-macos-x86_64.tar.gz",
    );
    assert.equal(
        selector.resolveSelection({
            purpose: "desktop-cli",
            desktopOs: "linux",
            desktopCpu: "i686",
        }).code,
        "unsupported_desktop_cpu",
    );
});

test("desktop CLI and C ABI cover every published OS and CPU pair", () => {
    for (const purpose of ["desktop-cli", "c-abi"]) {
        for (const desktopOs of ["linux", "macos", "windows"]) {
            for (const desktopCpu of ["x86_64", "aarch64"]) {
                const result = selector.resolveSelection({
                    purpose,
                    desktopOs,
                    desktopCpu,
                });
                const prefix = purpose === "c-abi" ? "c-sdk-" : "";
                const extension = desktopOs === "windows" ? "zip" : "tar.gz";

                assert.equal(result.ok, true, `${purpose}/${desktopOs}/${desktopCpu}`);
                assert.equal(
                    result.assetTemplate,
                    `synology-drive-sync-{tag}-${prefix}${desktopOs}-${desktopCpu}.${extension}`,
                );
                assert.equal(result.detected, `${desktopOs} · ${desktopCpu}`);
            }
        }
    }
});

test("purpose selection isolates DSM, desktop, SDK, and container inputs", () => {
    const dsm = resolve({ desktopOs: "unsupported-os", desktopCpu: "armv5" });
    const desktop = selector.resolveSelection({
        purpose: "desktop-cli",
        model: "not-a-model",
        productLine: "not-a-product",
        osVersion: "dsm-99.0",
        dsmVersion: "not-a-version",
        reportedArch: "not-an-architecture",
        desktopOs: "linux",
        desktopCpu: "x86_64",
    });
    const rust = selector.resolveSelection({
        purpose: "rust-sdk",
        desktopOs: "unsupported-os",
        desktopCpu: "unsupported-cpu",
    });
    const container = selector.resolveSelection({
        purpose: "container",
        desktopOs: "windows",
        desktopCpu: "arm64",
    });
    const unknown = selector.resolveSelection({
        purpose: "unknown",
        desktopOs: "linux",
        desktopCpu: "x86_64",
    });

    assert.equal(dsm.ok, true);
    assert.equal(dsm.artifactType, "dsm-spk");
    assert.equal(desktop.assetTemplate, "synology-drive-sync-{tag}-linux-x86_64.tar.gz");
    assert.equal(rust.assetTemplate, "synology-drive-sync-{tag}-rust-sdk.tar.gz");
    assert.equal(container.assetTemplate, "ghcr.io/supermarsx/synology-drive-sync:{tag}");
    assert.match(container.detected, /linux\/arm64/);
    assert.equal(unknown.code, "unknown_purpose");
});

test("platform-neutral Rust SDK does not reject an unsupported native host", () => {
    const result = selector.resolveSelection({
        purpose: "rust-sdk",
        desktopOs: "unsupported-os",
        desktopCpu: "armv7",
    });

    assert.equal(result.ok, true);
    assert.equal(result.assetTemplate, "synology-drive-sync-{tag}-rust-sdk.tar.gz");
    assert.equal(result.detected, "Platform-neutral Rust source bundle");
});

test("container selector correlates a release tag without claiming GHCR verification", () => {
    const base = selector.resolveSelection({
        purpose: "container",
        desktopOs: "linux",
        desktopCpu: "aarch64",
    });
    const result = selector.materializeRecommendation(base, {
        tag_name: "26.4",
        draft: false,
        prerelease: false,
        assets: [],
    });

    assert.equal(result.exact, false);
    assert.equal(result.releaseCorrelated, true);
    assert.equal(result.name, "ghcr.io/supermarsx/synology-drive-sync:26.4");
    assert.match(result.detected, /linux\/arm64/);
});

test("live GitHub asset must exist and be uploaded before an exact link is emitted", () => {
    const base = resolve({});
    const exact = selector.materializeRecommendation(base, {
        tag_name: "26.4",
        draft: false,
        prerelease: false,
        assets: [
            {
                name: "synology-drive-sync-26.4-armv7.spk",
                state: "uploaded",
                size: 42,
                browser_download_url:
                    "https://github.com/supermarsx/synology-drive-sync/releases/download/26.4/synology-drive-sync-26.4-armv7.spk",
            },
        ],
    });
    const absent = selector.materializeRecommendation(base, {
        tag_name: "26.4",
        draft: false,
        prerelease: false,
        assets: [],
    });
    const redirected = selector.materializeRecommendation(base, {
        tag_name: "26.4",
        draft: false,
        prerelease: false,
        assets: [
            {
                name: "synology-drive-sync-26.4-armv7.spk",
                state: "uploaded",
                size: 42,
                browser_download_url: "https://example.com/synology-drive-sync-26.4-armv7.spk",
            },
        ],
    });

    assert.equal(exact.exact, true);
    assert.equal(exact.name, "synology-drive-sync-26.4-armv7.spk");
    assert.equal(absent.exact, false);
    assert.equal(absent.name, "synology-drive-sync-YY.N-armv7.spk");
    assert.equal(redirected.exact, false);
});

test("known-invalid DSM SPK releases are never recommended", () => {
    const base = resolve({});
    const expectedReasons = new Map([
        ["26.5", /identity-changing\/set-ID privilege metadata/],
        ["26.6", /Synology-only sysnotify resource worker/],
        ["26.20", /system:system \(UID:GID 1:1\) canonical authentication helper/],
    ]);

    for (const [tag, expectedReason] of expectedReasons) {
        const name = `synology-drive-sync-${tag}-armv7.spk`;
        const result = selector.materializeRecommendation(base, {
            tag_name: tag,
            draft: false,
            prerelease: false,
            assets: [
                {
                    name,
                    state: "uploaded",
                    size: 42,
                    browser_download_url: `https://github.com/supermarsx/synology-drive-sync/releases/download/${tag}/${name}`,
                },
            ],
        });

        assert.equal(result.ok, false, tag);
        assert.equal(result.code, "known_invalid_dsm_spk_release", tag);
        assert.equal(result.exact, false, tag);
        assert.equal(result.downloadUrl, "https://github.com/supermarsx/synology-drive-sync/releases", tag);
        assert.match(result.message, new RegExp(`release ${tag.replace(".", "\\.")}`), tag);
        assert.match(result.details, expectedReason, tag);
        assert.match(result.details, /later fixed release/, tag);
    }
});

test("26.7 DSM SPK is accepted only through its exact canonical asset URL", () => {
    const base = resolve({});
    const name = "synology-drive-sync-26.7-armv7.spk";
    const result = selector.materializeRecommendation(base, {
        tag_name: "26.7",
        draft: false,
        prerelease: false,
        assets: [
            {
                name,
                state: "uploaded",
                size: 42,
                browser_download_url: `https://github.com/supermarsx/synology-drive-sync/releases/download/26.7/${name}`,
            },
        ],
    });

    assert.equal(result.ok, true);
    assert.equal(result.exact, true);
    assert.equal(result.name, name);
    assert.equal(
        result.downloadUrl,
        `https://github.com/supermarsx/synology-drive-sync/releases/download/26.7/${name}`,
    );
});

test("known-invalid DSM safety holds are scoped to SPKs", () => {
    const selections = [
        selector.resolveSelection({
            purpose: "desktop-cli",
            desktopOs: "linux",
            desktopCpu: "x86_64",
        }),
        selector.resolveSelection({ purpose: "rust-sdk" }),
        selector.resolveSelection({
            purpose: "c-abi",
            desktopOs: "macos",
            desktopCpu: "aarch64",
        }),
    ];

    for (const tag of ["26.5", "26.6", "26.20"]) {
        const releaseAssets = [
            `synology-drive-sync-${tag}-linux-x86_64.tar.gz`,
            `synology-drive-sync-${tag}-rust-sdk.tar.gz`,
            `synology-drive-sync-${tag}-c-sdk-macos-aarch64.tar.gz`,
        ].map(function asset(name) {
            return {
                name,
                state: "uploaded",
                size: 42,
                browser_download_url: `https://github.com/supermarsx/synology-drive-sync/releases/download/${tag}/${name}`,
            };
        });
        const release = {
            tag_name: tag,
            draft: false,
            prerelease: false,
            assets: releaseAssets,
        };

        for (const selection of selections) {
            const result = selector.materializeRecommendation(selection, release);
            assert.equal(result.ok, true, `${tag} ${selection.purpose}`);
            assert.equal(result.exact, true, `${tag} ${selection.purpose}`);
            assert.equal(result.tag, tag, `${tag} ${selection.purpose}`);
        }

        const container = selector.materializeRecommendation(
            selector.resolveSelection({
                purpose: "container",
                desktopOs: "linux",
                desktopCpu: "aarch64",
            }),
            release,
        );
        assert.equal(container.ok, true, tag);
        assert.equal(container.releaseCorrelated, true, tag);
        assert.equal(container.tag, tag);
    }
});

test("live release tags and GitHub asset URLs must be canonical", () => {
    const base = resolve({});

    for (const tag of ["26.0", "26.00", "26.01"]) {
        const result = selector.materializeRecommendation(base, {
            tag_name: tag,
            draft: false,
            prerelease: false,
            assets: [],
        });

        assert.equal(result.exact, false, tag);
        assert.equal(result.name, "synology-drive-sync-YY.N-armv7.spk", tag);
    }

    const name = "synology-drive-sync-26.4-armv7.spk";
    const path = `/supermarsx/synology-drive-sync/releases/download/26.4/${name}`;

    for (const url of [
        `https://github.com:444${path}`,
        `https://user@github.com${path}`,
        `https://user:password@github.com${path}`,
    ]) {
        const result = selector.materializeRecommendation(base, {
            tag_name: "26.4",
            draft: false,
            prerelease: false,
            assets: [
                {
                    name,
                    state: "uploaded",
                    size: 42,
                    browser_download_url: url,
                },
            ],
        });

        assert.equal(result.exact, false, url);
        assert.equal(result.name, "synology-drive-sync-YY.N-armv7.spk", url);
    }
});

function resolveAuto(overrides) {
    return selector.resolveSelection({
        purpose: "dsm-spk",
        model: "DS416",
        productLine: "auto",
        osVersion: "auto",
        dsmVersion: "",
        reportedArch: "armv7",
        ...overrides,
    });
}

test("model search ignores case, spaces, and plus, ranks exact names first, and covers the catalog", () => {
    const names = (query) => selector.searchModels(query).map((record) => record.model);

    assert.deepEqual(names("ds416"), ["DS416", "DS416j", "DS416play", "DS416slim"]);
    assert.deepEqual(names("DS 416"), names("ds416"));
    assert.deepEqual(names("  d s-4_1.6 "), names("ds416"));
    assert.deepEqual(names("rs11626"), ["RS11626xs+"]);
    assert.deepEqual(names("RS 11626 XS"), ["RS11626xs+"]);
    assert.deepEqual(names("rs11626xs+"), ["RS11626xs+"]);
    // "+" is ignored when filtering, but an exact name ranks first.
    assert.deepEqual(names("ds216"), ["DS216", "DS216+", "DS216+II", "DS216j", "DS216play", "DS216se"]);
    assert.deepEqual(names("ds216+").slice(0, 2), ["DS216+", "DS216"]);
    assert.deepEqual(names("DS216+II")[0], "DS216+II");
    assert.deepEqual(names("alpine4k"), ["DS215+", "DS416"], "Package Arch is searchable");
    assert.deepEqual(names("zzzz"), []);
    assert.equal(selector.searchModels("").length, 233);
    assert.equal(selector.searchModels("   ").length, 233);
    assert.equal(selector.searchModels("ds", 10).length, 10);

    // Every one of the 233 models is found by its own name and by a spaced, lower-case form.
    for (const record of selector.listModels()) {
        assert.equal(names(record.model)[0], record.model, record.model);
        assert.ok(names(record.model.toLowerCase().replace(/(\d)/, " $1")).includes(record.model), record.model);
    }
});

test("typed text never resolves to a model unless it is exactly one catalog name", () => {
    assert.equal(selector.exactModelForText("ds416").model, "DS416");
    assert.equal(selector.exactModelForText(" DS 416 ").model, "DS416");
    assert.equal(selector.exactModelForText("ds216+").model, "DS216+");
    assert.equal(selector.exactModelForText("rs11626xs+").model, "RS11626xs+");
    for (const text of ["", "ds41", "ds", "DS416x", "rs11626", "synology"]) {
        assert.equal(selector.exactModelForText(text), null, text);
    }
    assert.equal(selector.exactModelForText("ds41"), null);
    assert.equal(selector.exactModelForText("rs11626"), null);
    assert.equal(selector.exactModelForText("DS99999"), null);
    // Resolution itself has no nearest-model rule either.
    assert.equal(resolveAuto({ model: "ds41" }).code, "unknown_model");
    assert.equal(resolveAuto({ model: "DS 4160" }).code, "unknown_model");
    assert.equal(resolveAuto({ model: "" }).code, "unknown_model");
    assert.match(resolveAuto({ model: "" }).message, /typed text alone never selects/);
});

test("architecture auto-fill follows the catalog and the explicit asset mapping", () => {
    const ds416 = selector.autofillArchitecture("DS416");

    assert.equal(ds416.value, "armv7");
    assert.equal(ds416.packageArch, "alpine4k");
    assert.equal(ds416.assetName, "synology-drive-sync-YY.N-armv7.spk");
    assert.equal(ds416.source, "Synology's CPU table, captured 2026-08-26");
    assert.equal(selector.autofillArchitecture("DS923+").value, "x86_64");
    assert.equal(selector.autofillArchitecture("DS418").value, "armv8");
    assert.equal(selector.autofillArchitecture("DS214play").value, "i686");
    assert.equal(selector.autofillArchitecture("DS419slim").packageArch, "armada38x");

    // Legacy, Enterprise, unknown and unmapped models fill nothing.
    for (const model of ["DS1511+", "DS3611xs", "DS110+", "DS213+", "PAS7700", "DS99999", "", null]) {
        assert.equal(selector.autofillArchitecture(model), null, String(model));
    }
    for (const record of selector.listModels()) {
        const compatibility = selector.modelCompatibility(record);
        const fill = selector.autofillArchitecture(record);

        if (compatibility.status === "supported" && record.assetArch) {
            assert.equal(fill.value, record.assetArch, record.model);
            assert.equal(selector.normalizeCpuArch(fill.value), selector.normalizeCpuArch(record.cpuArch), record.model);
            // The filled value always satisfies the same check a user-supplied value would.
            const dsmVersion = compatibility.maxMinor === 4 ? "7.4-90075" : "";
            const resolved = resolveAuto({
                model: record.model,
                reportedArch: fill.value,
                dsmVersion: compatibility.minBuild === null ? dsmVersion : "7.4.1-90080",
            });
            assert.notEqual(resolved.code, "architecture_conflict", record.model);
            assert.notEqual(resolved.code, "unknown_reported_arch", record.model);
        } else {
            assert.equal(fill, null, record.model);
        }
    }
});

test("Automatic DSM resolves per model lifecycle within the SPK range", () => {
    const auto = selector.resolveAutomaticDsm;

    // DSM 7.0-7.1 models resolve to 7.1 and need no exact build.
    const old = auto("DS414", "");
    assert.equal(old.ok, true);
    assert.equal(old.osVersion.display, "DSM 7.1");
    assert.equal(old.requiresBuild, false);
    assert.equal(
        old.text,
        "Automatic: DSM 7.1 for this model (the newest it supports). No exact build is needed.",
    );
    assert.equal(resolveAuto({ model: "DS414", reportedArch: "armv7" }).ok, true);
    assert.match(resolveAuto({ model: "DS414", reportedArch: "armv7" }).detected, /DSM 7\.1 \(Automatic/);

    // DSM 7.x-7.4 models resolve to 7.4 with no build: every 7.4 build is within 7.4-99999 and the
    // model's newest DSM is above the 7.0-40759 minimum.
    const newest = auto("DS923+", "");
    assert.equal(newest.osVersion.display, "DSM 7.4");
    assert.equal(newest.requiresBuild, false);
    assert.match(newest.text, /^Automatic: DSM 7\.4 for this model.*No exact build is needed\.$/);
    const resolved = resolveAuto({ model: "DS923+", reportedArch: "x86_64" });
    assert.equal(resolved.ok, true);
    assert.equal(resolved.assetTemplate, "synology-drive-sync-{tag}-x86_64.spk");
    assert.equal(resolveAuto({ reportedArch: "armv7" }).assetTemplate, "synology-drive-sync-{tag}-armv7.spk");
    assert.equal(resolveAuto({ reportedArch: "armv7" }).ok, true, "DS416 + Automatic needs no build");
    // Models first published in 7.4.1 resolve too: they cannot run an older image.
    for (const model of ["RS11626xs+", "DS1525neo+", "DS925neo+", "DVA7400"]) {
        assert.equal(resolveAuto({ model, reportedArch: "x86_64" }).ok, true, model);
    }
    // Manual picks keep today's rule: DSM 7.0 and 7.4 need the exact build.
    assert.equal(
        resolveAuto({ model: "DS923+", reportedArch: "x86_64", osVersion: "dsm-7.4" }).code,
        "dsm_build_required",
    );
    assert.equal(
        resolveAuto({ reportedArch: "armv7", osVersion: "dsm-7.0" }).code,
        "dsm_build_required",
    );
    assert.equal(resolveAuto({ reportedArch: "armv7", osVersion: "dsm-7.2" }).ok, true);

    // An exact build is honoured and sets the branch, including the 7.0 and 7.4 boundaries.
    assert.equal(resolveAuto({ dsmVersion: "7.2.2-72806" }).ok, true);
    assert.equal(auto("DS416", "7.2.2-72806").osVersion.display, "DSM 7.2");
    assert.equal(resolveAuto({ dsmVersion: "7.4.1-90080" }).ok, true);
    assert.equal(resolveAuto({ dsmVersion: "7.4-99999" }).ok, true);
    assert.equal(resolveAuto({ dsmVersion: "7.0.1-42218" }).ok, true);
    assert.equal(resolveAuto({ dsmVersion: "7.0-40758" }).code, "dsm_too_old");
    assert.equal(resolveAuto({ dsmVersion: "7.4-100000" }).code, "dsm_too_new");

    // DSM 7.5+ and anything older than 7.0 stop with fail-closed messages.
    assert.equal(resolveAuto({ dsmVersion: "7.5-100000" }).code, "unsupported_dsm_line");
    assert.match(resolveAuto({ dsmVersion: "7.5-100000" }).message, /outside the reviewed DSM 7\.0–7\.4 range/);
    assert.equal(resolveAuto({ dsmVersion: "8.0-1000" }).code, "unsupported_dsm_line");
    assert.equal(resolveAuto({ dsmVersion: "6.2.4-25556" }).code, "unsupported_dsm_line");
    assert.equal(resolveAuto({ dsmVersion: "seven" }).code, "invalid_dsm_version");

    // A build outside the model's own interval still conflicts, as with an explicit pick.
    assert.equal(
        resolveAuto({ model: "DS414", reportedArch: "armv7", dsmVersion: "7.2.2-72806" }).code,
        "model_dsm_conflict",
    );
    assert.equal(
        resolveAuto({ model: "DS923+", reportedArch: "x86_64", dsmVersion: "7.0.1-42218" }).code,
        "model_dsm_conflict",
    );

    // Models first published in 7.4.1 keep their build floor under Automatic.
    assert.equal(resolveAuto({ model: "RS11626xs+", reportedArch: "x86_64", dsmVersion: "7.4.1-90080" }).ok, true);
    assert.equal(
        resolveAuto({ model: "RS11626xs+", reportedArch: "x86_64", dsmVersion: "7.4-90075" }).code,
        "model_build_too_old",
    );

    // Last supported DSM below 7.0 resolves to the existing fail-closed stop.
    for (const model of ["DS1511+", "DS3611xs", "DS110+"]) {
        const legacy = auto(model, "");

        assert.equal(legacy.ok, false, model);
        assert.equal(legacy.code, "model_dsm_conflict", model);
        assert.match(legacy.message, /last upgradable DSM as (5\.2|6\.2)/, model);
        assert.equal(resolveAuto({ model, reportedArch: "x86_64" }).code, "model_dsm_conflict", model);
    }

    // DSM Enterprise keeps its own stop, with Automatic or an explicit product line.
    assert.equal(auto("PAS7700", "").code, "unsupported_product_line");
    assert.equal(resolveAuto({ model: "PAS7700", reportedArch: "x86_64" }).code, "unsupported_product_line");
    assert.equal(
        resolveAuto({ model: "DS416", productLine: "dsm-enterprise" }).code,
        "product_line_conflict",
    );
    assert.equal(resolveAuto({ productLine: "bogus" }).code, "invalid_product_line");

    // An unknown model never resolves.
    assert.equal(auto("DS99999", "").code, "unknown_model");
});

test("a model whose newest DSM exceeds the SPK maximum stops under Automatic", () => {
    const data = mutableSelectorData();
    const group = data.compatibilityGroups.find((candidate) => candidate.models.includes("DS416"));

    group.maxMinor = 5;
    const patched = loadSelectorWithData(data);

    assert.equal(patched.resolveAutomaticDsm("DS416", "").code, "dsm_too_new");
    assert.match(patched.resolveAutomaticDsm("DS416", "").message, /7\.4-99999/);
    // An explicit, in-range pick on the same data is unaffected.
    assert.equal(
        patched.resolveSelection({
            purpose: "dsm-spk",
            model: "DS416",
            productLine: "dsm",
            osVersion: "dsm-7.2",
            dsmVersion: "7.2.2-72806",
            reportedArch: "armv7",
        }).ok,
        true,
    );
});

test("Automatic needs the exact build only where the build decides: a model whose newest DSM is 7.0", () => {
    const data = mutableSelectorData();
    const group = data.compatibilityGroups.find((candidate) => candidate.models.includes("DS416"));

    group.maxMinor = 0;
    const patched = loadSelectorWithData(data);
    const input = {
        purpose: "dsm-spk",
        model: "DS416",
        productLine: "auto",
        osVersion: "auto",
        dsmVersion: "",
        reportedArch: "armv7",
    };
    const auto = patched.resolveAutomaticDsm("DS416", "");

    assert.equal(auto.ok, true);
    assert.equal(auto.requiresBuild, true);
    assert.match(auto.text, /DSM 7\.0 needs the exact installed build/);

    const stop = patched.resolveSelection(input);

    assert.equal(stop.code, "dsm_build_required");
    assert.match(stop.message, /Automatic resolved DSM 7\.0 for DS416/);
    assert.match(stop.details, /7\.0-40759/);
    assert.equal(patched.resolveSelection({ ...input, dsmVersion: "7.0.1-42218" }).ok, true);
    assert.equal(patched.resolveSelection({ ...input, dsmVersion: "7.0-40758" }).code, "dsm_too_old");
});

test("DS416 resolves end to end to the armv7 SPK, and the release rules still apply", () => {
    const fill = selector.autofillArchitecture("DS416");
    const resolved = resolveAuto({ reportedArch: fill.value, dsmVersion: "7.2.2-72806" });
    const release = (tag) => ({
        tag_name: tag,
        draft: false,
        prerelease: false,
        assets: [
            {
                name: `synology-drive-sync-${tag}-armv7.spk`,
                state: "uploaded",
                size: 42,
                browser_download_url: `https://github.com/supermarsx/synology-drive-sync/releases/download/${tag}/synology-drive-sync-${tag}-armv7.spk`,
            },
        ],
    });

    assert.equal(resolved.ok, true);
    assert.equal(resolved.assetTemplate, "synology-drive-sync-{tag}-armv7.spk");
    assert.match(resolved.detected, /DS416 · Product line DSM · Processor armv7 · Package Arch alpine4k · Release asset armv7/);

    const exact = selector.materializeRecommendation(resolved, release("26.30"));

    assert.equal(exact.exact, true);
    assert.equal(
        exact.downloadUrl,
        "https://github.com/supermarsx/synology-drive-sync/releases/download/26.30/synology-drive-sync-26.30-armv7.spk",
    );
    assert.equal(selector.materializeRecommendation(resolved, null).exact, false);
    assert.doesNotMatch(selector.materializeRecommendation(resolved, null).downloadUrl, /\/releases\/download\//);
    for (const tag of ["26.5", "26.6", "26.20"]) {
        const held = selector.materializeRecommendation(resolved, release(tag));

        assert.equal(held.ok, false, tag);
        assert.equal(held.code, "known_invalid_dsm_spk_release", tag);
    }

    // Overriding the architecture so it disagrees with the catalog stops the selector.
    const mismatch = resolveAuto({ reportedArch: "x86_64", dsmVersion: "7.2.2-72806" });

    assert.equal(mismatch.ok, false);
    assert.equal(mismatch.code, "architecture_conflict");
    assert.equal(resolveAuto({ reportedArch: "", dsmVersion: "7.2.2-72806" }).code, "unknown_reported_arch");
    assert.equal(resolveAuto({ reportedArch: "aarch64", dsmVersion: "7.2.2-72806" }).code, "architecture_conflict");
});
