(function exposeReleaseSelector(root, factory) {
    let selectorData = root.SDSYNC_RELEASE_SELECTOR_DATA;

    if (typeof module === "object" && module.exports) {
        selectorData = require("./release-selector-data.js");
    }

    const api = factory(selectorData);

    if (typeof module === "object" && module.exports) {
        module.exports = api;
    }

    root.SDSyncReleaseSelector = api;

    if (root.document) {
        const start = function startReleaseSelectors() {
            api.initSelectors(root.document, root.fetch ? root.fetch.bind(root) : null);
        };

        if (root.document.readyState === "loading") {
            root.document.addEventListener("DOMContentLoaded", start, { once: true });
        } else {
            start();
        }
    }
})(typeof globalThis !== "undefined" ? globalThis : this, function createReleaseSelector(data) {
    "use strict";

    if (!data || !Array.isArray(data.modelGroups)) {
        throw new Error("release selector model data is unavailable");
    }

    const REPOSITORY = "supermarsx/synology-drive-sync";
    const RELEASES_URL = `https://github.com/${REPOSITORY}/releases/latest`;
    const RELEASE_API_URL = `https://api.github.com/repos/${REPOSITORY}/releases/latest`;
    const PACKAGE_URL = `https://github.com/${REPOSITORY}/pkgs/container/synology-drive-sync`;
    const FALLBACK_TAG = "YY.N";
    const MAX_VERIFIED_DSM_MINOR = 4;

    const ARMV7_PACKAGES = new Set([
        "alpine",
        "alpine4k",
        "armada370",
        "armada375",
        "armada38x",
        "armadaxp",
        "comcerto2k",
        "monaco",
    ]);
    const ARMV8_PACKAGES = new Set(["armada37xx", "rtd1296", "rtd1619b"]);
    const X86_64_PACKAGES = new Set([
        "apollolake",
        "avoton",
        "braswell",
        "broadwell",
        "broadwellnk",
        "broadwellnkv2",
        "broadwellntbap",
        "bromolow",
        "cedarview",
        "denverton",
        "epyc7002",
        "epyc7003",
        "geminilake",
        "geminilakenk",
        "grantley",
        "icelaked",
        "purley",
        "r1000",
        "r1000nk",
        "v1000",
        "v1000nk",
    ]);
    // Exact contiguous intervals derived from AllPlatformOptionNames in SynologyOpenSource's
    // official DSM7.0 through DSM7.4 pkgscripts-ng branches. Model-less toolkit platforms such as
    // kvmx64, kvmcloud, and epyc7003ntb are intentionally absent from this model-driven selector.
    const DSM_PLATFORM_MINOR_BOUNDS = Object.freeze({
        alpine: Object.freeze({ min: 0, max: 4 }),
        alpine4k: Object.freeze({ min: 0, max: 4 }),
        apollolake: Object.freeze({ min: 0, max: 4 }),
        armada370: Object.freeze({ min: 0, max: 1 }),
        armada375: Object.freeze({ min: 0, max: 1 }),
        armada37xx: Object.freeze({ min: 0, max: 4 }),
        armada38x: Object.freeze({ min: 0, max: 4 }),
        armadaxp: Object.freeze({ min: 0, max: 1 }),
        avoton: Object.freeze({ min: 0, max: 4 }),
        braswell: Object.freeze({ min: 0, max: 4 }),
        broadwell: Object.freeze({ min: 0, max: 4 }),
        broadwellnk: Object.freeze({ min: 0, max: 4 }),
        broadwellnkv2: Object.freeze({ min: 1, max: 4 }),
        broadwellntbap: Object.freeze({ min: 2, max: 4 }),
        bromolow: Object.freeze({ min: 0, max: 4 }),
        cedarview: Object.freeze({ min: 0, max: 1 }),
        comcerto2k: Object.freeze({ min: 0, max: 1 }),
        denverton: Object.freeze({ min: 0, max: 4 }),
        epyc7002: Object.freeze({ min: 1, max: 4 }),
        epyc7003: Object.freeze({ min: 3, max: 4 }),
        evansport: Object.freeze({ min: 0, max: 1 }),
        geminilake: Object.freeze({ min: 0, max: 4 }),
        geminilakenk: Object.freeze({ min: 2, max: 4 }),
        grantley: Object.freeze({ min: 0, max: 4 }),
        icelaked: Object.freeze({ min: 3, max: 4 }),
        monaco: Object.freeze({ min: 0, max: 4 }),
        purley: Object.freeze({ min: 0, max: 4 }),
        r1000: Object.freeze({ min: 1, max: 4 }),
        r1000nk: Object.freeze({ min: 2, max: 4 }),
        rtd1296: Object.freeze({ min: 0, max: 4 }),
        rtd1619b: Object.freeze({ min: 1, max: 4 }),
        v1000: Object.freeze({ min: 0, max: 4 }),
        v1000nk: Object.freeze({ min: 2, max: 4 }),
    });
    const DESKTOP_OSES = new Set(["linux", "macos", "windows"]);
    const DESKTOP_CPUS = new Set(["aarch64", "x86_64"]);
    const NON_DSM_MODELS = Object.freeze({
        PAS7700: Object.freeze({
            operatingSystem: "DSM Enterprise 1.0",
            sourceUrl: "https://www.synology.com/en-us/support/download/PAS7700",
        }),
    });

    function normalizedModel(value) {
        return String(value || "")
            .trim()
            .toUpperCase()
            .replace(/\s+/g, "");
    }

    function buildModelIndex(modelGroups) {
        const index = new Map();

        for (const group of modelGroups) {
            for (const model of group.models) {
                const key = normalizedModel(model);
                const record = Object.freeze({
                    model,
                    cpuArch: String(group.cpuArch).toLowerCase(),
                    packageArch: String(group.packageArch).toLowerCase(),
                });
                const existing = index.get(key);

                if (
                    existing &&
                    (existing.cpuArch !== record.cpuArch ||
                        existing.packageArch !== record.packageArch)
                ) {
                    throw new Error(`conflicting Synology model snapshot entry: ${model}`);
                }

                index.set(key, record);
            }
        }

        return index;
    }

    const MODEL_INDEX = buildModelIndex(data.modelGroups);

    if (!Number.isInteger(data.modelCount) || data.modelCount !== MODEL_INDEX.size) {
        throw new Error("release selector model snapshot count does not match its model records");
    }

    function listModels() {
        return Array.from(MODEL_INDEX.values()).sort(function sortModels(left, right) {
            return left.model.localeCompare(right.model, undefined, {
                numeric: true,
                sensitivity: "base",
            });
        });
    }

    function lookupModel(model) {
        return MODEL_INDEX.get(normalizedModel(model)) || null;
    }

    function nonDsmModelMetadata(model) {
        return model ? NON_DSM_MODELS[normalizedModel(model.model)] || null : null;
    }

    function normalizeCpuArch(value) {
        const raw = String(value || "").trim().toLowerCase();
        const families = new Set();

        if (!raw) {
            return null;
        }

        const patterns = [
            ["armv7", /\b(?:armv7(?:-a|l)?|v7l)\b/],
            ["armv8", /\b(?:aarch64|arm64|armv8(?:-a)?)\b/],
            ["x86_64", /\b(?:x86[_-]?64|amd64|x64)\b/],
            ["i686", /\b(?:i[3-6]86|ia32)\b|\bx86\b(?![_-]64)/],
            ["armv5", /\barmv5(?:te|l)?\b/],
            ["ppc", /\b(?:powerpc(?:spe)?|ppc(?:64(?:le)?|spe)?|qoriq)\b/],
        ];

        for (const [family, pattern] of patterns) {
            if (pattern.test(raw)) {
                families.add(family);
            }
        }

        return families.size === 1 ? families.values().next().value : null;
    }

    function normalizeDesktopCpu(value) {
        const normalized = normalizeCpuArch(value);

        if (normalized === "armv8") {
            return "aarch64";
        }

        return normalized;
    }

    function parseDsmVersion(value) {
        const match = String(value || "").match(
            /^\s*(?:DSM\s*)?(\d+)\.(\d+)(?:\.(\d+))?\s*-\s*(\d+)(?:\s+(?:Update|u)\s*(\d+))?\s*$/i,
        );

        if (!match) {
            return null;
        }

        const numbers = match.slice(1, 6).filter(Boolean).map(Number);

        if (
            numbers.some(function invalidVersionPart(part) {
                return !Number.isSafeInteger(part) || part < 0;
            }) ||
            Number(match[1]) === 0 ||
            Number(match[4]) === 0
        ) {
            return null;
        }

        return Object.freeze({
            major: Number(match[1]),
            minor: Number(match[2]),
            patch: Number(match[3] || 0),
            build: Number(match[4]),
            update: Number(match[5] || 0),
            display: `${match[1]}.${match[2]}${match[3] ? `.${match[3]}` : ""}-${match[4]}${match[5] ? ` Update ${match[5]}` : ""}`,
        });
    }

    function failure(code, message, details) {
        return Object.freeze({
            ok: false,
            code,
            message,
            details: details || "No artifact is recommended until these inputs agree.",
        });
    }

    function packageAssetArch(packageArch) {
        if (ARMV7_PACKAGES.has(packageArch)) {
            return "armv7";
        }
        if (ARMV8_PACKAGES.has(packageArch)) {
            return "armv8";
        }
        if (X86_64_PACKAGES.has(packageArch)) {
            return "x86_64";
        }
        if (packageArch === "evansport") {
            return "i686";
        }

        return null;
    }

    function dsmMinorBounds(packageArch) {
        return DSM_PLATFORM_MINOR_BOUNDS[String(packageArch || "").toLowerCase()] || null;
    }

    function dsmMinorRangeLabel(bounds) {
        return bounds.min === bounds.max
            ? `DSM 7.${bounds.min}`
            : `DSM 7.${bounds.min}–7.${bounds.max}`;
    }

    const DSM_ASSET_PACKAGES = new Set([
        ...ARMV7_PACKAGES,
        ...ARMV8_PACKAGES,
        ...X86_64_PACKAGES,
        "evansport",
    ]);

    if (
        Object.keys(DSM_PLATFORM_MINOR_BOUNDS).length !== DSM_ASSET_PACKAGES.size ||
        [...DSM_ASSET_PACKAGES].some(function missingBounds(packageArch) {
            return !dsmMinorBounds(packageArch);
        }) ||
        Object.keys(DSM_PLATFORM_MINOR_BOUNDS).some(function missingAsset(packageArch) {
            return !DSM_ASSET_PACKAGES.has(packageArch);
        })
    ) {
        throw new Error("DSM asset families and official toolkit intervals are out of sync");
    }

    function resolveDsmSelection(input) {
        const catalogModel = lookupModel(input.model);
        const nonDsmModel = nonDsmModelMetadata(catalogModel);

        if (nonDsmModel) {
            return failure(
                "non_dsm_model",
                `${catalogModel.model} runs ${nonDsmModel.operatingSystem}, not DSM 7.`,
                `The factual CPU snapshot retains this model, but DSM 7 SPKs are not compatible with its operating-system and package lifecycle. Confirm the product OS in Synology's Download Center: ${nonDsmModel.sourceUrl}`,
            );
        }

        const version = parseDsmVersion(input.dsmVersion);

        if (!version) {
            return failure(
                "invalid_dsm_version",
                "Enter the complete DSM version and build, for example 7.2.2-72806.",
            );
        }
        if (
            version.major < 7 ||
            (version.major === 7 && version.build < 40759)
        ) {
            return failure(
                "dsm_too_old",
                "This package requires DSM 7.0-40759 or newer.",
                "DSM 6 and earlier require a separately designed package; changing the INFO label would not make this DSM 7 package compatible.",
            );
        }
        if (version.major > 7) {
            return failure(
                "dsm_unverified_major",
                `DSM ${version.major} is newer than the verified DSM 7 package contract.`,
                "Check the current release notes before installing; the selector does not infer forward compatibility.",
            );
        }
        if (version.minor > MAX_VERIFIED_DSM_MINOR) {
            return failure(
                "dsm_unverified_minor",
                `DSM 7.${version.minor} is newer than the DSM 7.4 toolkit matrix captured by this selector.`,
                "Check the current release notes and official Synology platform table; the selector does not infer compatibility for a newer DSM branch.",
            );
        }

        const model = catalogModel;

        if (!model) {
            return failure(
                "unknown_model",
                "That model is not in the captured Synology CPU-table snapshot.",
                `Check Synology's live Package Arch table (snapshot captured ${data.snapshotCapturedDate}) and report the exact model, DSM build, and Package Arch. The selector will not guess from a model year or marketing name.`,
            );
        }

        const expectedCpu = normalizeCpuArch(model.cpuArch);
        const reportedCpu = normalizeCpuArch(input.reportedArch);

        if (!reportedCpu) {
            return failure(
                "unknown_reported_arch",
                "The reported processor architecture is missing, unrecognized, or contradictory.",
                "Use one exact output family: x86_64, aarch64/armv8, armv7l/armv7-a, or i686. Multiple family tokens are rejected.",
            );
        }
        if (expectedCpu === "armv5" || expectedCpu === "ppc") {
            return failure(
                "legacy_cpu",
                `${model.model} uses unsupported ${model.cpuArch}/${model.packageArch} package architecture.`,
                "Official DSM 7 toolkits do not provide ARMv5 or PowerPC targets, and this package depends on DSM 7 paths and lifecycle behavior.",
            );
        }
        if (expectedCpu !== reportedCpu) {
            return failure(
                "architecture_conflict",
                `The official ${model.model} entry says ${model.cpuArch}/${model.packageArch}, but the reported architecture resolves to ${reportedCpu}.`,
                "Recheck the model and runtime output. A package is not recommended when those two independent inputs conflict.",
            );
        }
        const assetArch = packageAssetArch(model.packageArch);

        if (!assetArch) {
            return failure(
                "unsupported_package_arch",
                `No verified DSM artifact maps to Package Arch ${model.packageArch}.`,
                "Use the exact official Package Arch to request a new verified build; do not install a relabeled binary.",
            );
        }

        const bounds = dsmMinorBounds(model.packageArch);

        if (!bounds) {
            return failure(
                "unsupported_package_arch",
                `No verified DSM toolkit interval maps to Package Arch ${model.packageArch}.`,
                "Update the captured official platform matrix and its regression tests before recommending an artifact.",
            );
        }
        if (version.minor < bounds.min || version.minor > bounds.max) {
            return failure(
                "platform_dsm_conflict",
                `${model.model}/${model.packageArch} is present in the official ${dsmMinorRangeLabel(bounds)} toolkit interval, not DSM 7.${version.minor}.`,
                "Recheck the DSM version and model. The selector does not infer support before a platform was introduced or after Synology removed it from pkgscripts-ng.",
            );
        }

        return Object.freeze({
            ok: true,
            kind: "release-asset",
            purpose: "DSM direct SPK",
            assetTemplate: `synology-drive-sync-{tag}-${assetArch}.spk`,
            detected: `${model.model} · ${model.cpuArch} · Package Arch ${model.packageArch} · DSM ${version.display} · Toolkit ${dsmMinorRangeLabel(bounds)}`,
            rationale:
                assetArch === "armv7"
                    ? "The ARMv7 SPK carries one little-endian EABI5 hard-float binary and explicitly lists every verified DSM 7 ARMv7 family in INFO."
                    : `The ${assetArch} SPK matches the official Synology package-family mapping.`,
        });
    }

    function desktopPlatform(input) {
        const os = String(input.desktopOs || "").trim().toLowerCase();
        const cpu = normalizeDesktopCpu(input.desktopCpu);

        if (!DESKTOP_OSES.has(os)) {
            return failure(
                "unsupported_desktop_os",
                "Choose Linux, macOS, or Windows.",
            );
        }
        if (!DESKTOP_CPUS.has(cpu)) {
            return failure(
                "unsupported_desktop_cpu",
                "Published desktop artifacts support x86-64 and ARM64 only.",
                "Use a source build for another target only after validating its complete dependency and runtime contract.",
            );
        }

        return Object.freeze({ ok: true, os, cpu });
    }

    function resolveDesktopSelection(input) {
        if (input.purpose === "rust-sdk") {
            return Object.freeze({
                ok: true,
                kind: "release-asset",
                purpose: "Rust SDK source",
                assetTemplate: "synology-drive-sync-{tag}-rust-sdk.tar.gz",
                detected: "Platform-neutral Rust source bundle",
                rationale: "The Rust SDK is source, so resolving this asset does not depend on the build host's OS or processor architecture.",
            });
        }

        const platform = desktopPlatform(input);

        if (!platform.ok) {
            return platform;
        }

        const suffix = platform.os === "windows" ? "zip" : "tar.gz";
        const detected = `${platform.os} · ${platform.cpu}`;

        switch (input.purpose) {
            case "desktop-cli":
                return Object.freeze({
                    ok: true,
                    kind: "release-asset",
                    purpose: "Desktop CLI",
                    assetTemplate: `synology-drive-sync-{tag}-${platform.os}-${platform.cpu}.${suffix}`,
                    detected,
                    rationale: "This archive contains the native CLI, completions, manual pages, license, and dependency notices.",
                });
            case "c-abi":
                return Object.freeze({
                    ok: true,
                    kind: "release-asset",
                    purpose: "C ABI SDK",
                    assetTemplate: `synology-drive-sync-{tag}-c-sdk-${platform.os}-${platform.cpu}.${suffix}`,
                    detected,
                    rationale:
                        platform.os === "windows"
                            ? "The SDK contains sdsync.dll, its import library, the C header, example, and notices."
                            : `The SDK contains the native ${platform.os === "macos" ? "dylib" : "shared object"}, C header, example, and notices.`,
                });
            case "container":
                return Object.freeze({
                    ok: true,
                    kind: "container",
                    purpose: "Container image",
                    assetTemplate: "ghcr.io/supermarsx/synology-drive-sync:{tag}",
                    detected: `${detected} · linux/${platform.cpu === "x86_64" ? "amd64" : "arm64"}`,
                    rationale: "The multi-architecture OCI image runs as a Linux container through Docker or another compatible runtime.",
                });
            default:
                return failure("unknown_purpose", "Choose a supported release purpose.");
        }
    }

    function resolveSelection(input) {
        if (input && input.purpose === "dsm-spk") {
            return resolveDsmSelection(input);
        }

        return resolveDesktopSelection(input || {});
    }

    function applyTag(template, tag) {
        return String(template).replace(/\{tag\}/g, tag);
    }

    function validLatestRelease(payload) {
        if (
            !payload ||
            payload.draft !== false ||
            payload.prerelease !== false ||
            !/^\d{2}\.\d+$/.test(String(payload.tag_name || ""))
        ) {
            return null;
        }

        return payload;
    }

    function isOfficialAssetUrl(value, tag, name) {
        try {
            const parsed = new URL(value);

            return (
                parsed.protocol === "https:" &&
                parsed.hostname === "github.com" &&
                parsed.pathname === `/${REPOSITORY}/releases/download/${tag}/${name}` &&
                !parsed.search &&
                !parsed.hash
            );
        } catch (_error) {
            return false;
        }
    }

    function materializeRecommendation(recommendation, payload) {
        if (!recommendation.ok) {
            return recommendation;
        }

        const release = validLatestRelease(payload);
        const tag = release ? release.tag_name : FALLBACK_TAG;
        const name = applyTag(recommendation.assetTemplate, tag);

        if (recommendation.kind === "container") {
            return Object.freeze({
                ...recommendation,
                name,
                tag,
                exact: false,
                releaseCorrelated: Boolean(release),
                downloadUrl: PACKAGE_URL,
                releaseNote: release
                    ? `The live latest GitHub Release is ${tag}, but this page has not independently verified that the GHCR tag exists. Confirm the tag in the container package and deploy its digest.`
                    : "Live release lookup was unavailable. YY.N shows the versioned form; confirm an actual tag in the container package and deploy its digest.",
            });
        }

        const asset = release
            ? (Array.isArray(release.assets) ? release.assets : []).find(function findAsset(candidate) {
                  return (
                      candidate &&
                      candidate.name === name &&
                       candidate.state === "uploaded" &&
                       Number(candidate.size) > 0 &&
                       typeof candidate.browser_download_url === "string" &&
                       isOfficialAssetUrl(candidate.browser_download_url, tag, name)
                  );
              })
            : null;

        if (asset) {
            return Object.freeze({
                ...recommendation,
                name,
                tag,
                exact: true,
                downloadUrl: asset.browser_download_url,
                releaseNote: `Verified against the live latest GitHub Release ${tag}.`,
            });
        }

        return Object.freeze({
            ...recommendation,
            name: applyTag(recommendation.assetTemplate, FALLBACK_TAG),
            tag: FALLBACK_TAG,
            exact: false,
            downloadUrl: RELEASES_URL,
            releaseNote: release
                ? `The live latest release is ${tag}, but it does not publish this exact asset. The versioned filename pattern is shown without inventing a download.`
                : "Live release lookup was unavailable. The deterministic YY.N filename pattern is shown; choose the matching asset on Releases.",
        });
    }

    async function fetchLatestRelease(fetchImpl) {
        if (typeof fetchImpl !== "function") {
            return null;
        }

        const response = await fetchImpl(RELEASE_API_URL, {
            headers: {
                Accept: "application/vnd.github+json",
                "X-GitHub-Api-Version": "2022-11-28",
            },
            cache: "no-store",
        });

        if (!response.ok) {
            throw new Error(`GitHub latest release lookup failed with HTTP ${response.status}`);
        }

        return response.json();
    }

    function setPurposeFields(container, purpose) {
        const dsmFields = container.querySelector("[data-dsm-fields]");
        const desktopFields = container.querySelector("[data-desktop-fields]");
        const wantsDsm = purpose === "dsm-spk";
        const wantsDesktop = !wantsDsm && purpose !== "rust-sdk";

        if (dsmFields) {
            dsmFields.hidden = !wantsDsm;
            for (const control of dsmFields.querySelectorAll("input, select")) {
                control.disabled = !wantsDsm;
            }
        }
        if (desktopFields) {
            desktopFields.hidden = !wantsDesktop;
            for (const control of desktopFields.querySelectorAll("input, select")) {
                control.disabled = !wantsDesktop;
            }
        }
    }

    function appendFact(documentRef, list, label, value) {
        const wrapper = documentRef.createElement("div");
        const term = documentRef.createElement("dt");
        const description = documentRef.createElement("dd");

        term.textContent = label;
        description.textContent = value;
        wrapper.append(term, description);
        list.append(wrapper);
    }

    function renderFailure(container, recommendation) {
        const result = container.querySelector("[data-selector-result]");
        const documentRef = container.ownerDocument;
        const heading = documentRef.createElement("h2");
        const summary = documentRef.createElement("p");
        const detail = documentRef.createElement("p");

        heading.textContent = "No safe recommendation";
        summary.className = "selector-result-summary";
        summary.textContent = recommendation.message;
        detail.textContent = recommendation.details;
        result.replaceChildren(heading, summary, detail);
        result.dataset.state = "blocked";
        result.hidden = false;
        result.setAttribute("tabindex", "-1");
        result.focus();
    }

    function renderSuccess(container, recommendation) {
        const result = container.querySelector("[data-selector-result]");
        const documentRef = container.ownerDocument;
        const heading = documentRef.createElement("h2");
        const summary = documentRef.createElement("p");
        const artifact = documentRef.createElement("code");
        const facts = documentRef.createElement("dl");
        const rationale = documentRef.createElement("p");
        const releaseNote = documentRef.createElement("p");
        const link = documentRef.createElement("a");

        heading.textContent = "Recommended release";
        summary.className = "selector-result-summary";
        summary.textContent = `${recommendation.purpose}: `;
        artifact.textContent = recommendation.name;
        summary.append(artifact);
        facts.className = "selector-result-facts";
        appendFact(documentRef, facts, "Matched inputs", recommendation.detected);
        appendFact(
            documentRef,
            facts,
            "Release lookup",
            recommendation.exact
                ? `Live ${recommendation.tag}`
                : recommendation.releaseCorrelated
                  ? `Release ${recommendation.tag}; GHCR unverified`
                  : "Offline-safe pattern",
        );
        rationale.textContent = recommendation.rationale;
        releaseNote.textContent = recommendation.releaseNote;
        link.className = "selector-download";
        link.href = recommendation.downloadUrl;
        link.textContent =
            recommendation.kind === "container"
                ? "Open the container package"
                : recommendation.exact
                  ? "Download this exact asset"
                  : "Open the latest GitHub Release";
        if (recommendation.exact && recommendation.kind === "release-asset") {
            link.setAttribute("download", "");
        }
        result.replaceChildren(heading, summary, facts, rationale, releaseNote, link);
        result.dataset.state = recommendation.exact ? "exact" : "fallback";
        result.hidden = false;
        result.setAttribute("tabindex", "-1");
        result.focus();
    }

    function formInput(form) {
        const values = new form.ownerDocument.defaultView.FormData(form);

        return {
            purpose: values.get("purpose"),
            model: values.get("model"),
            dsmVersion: values.get("dsmVersion"),
            reportedArch: values.get("reportedArch"),
            desktopOs: values.get("desktopOs"),
            desktopCpu: values.get("desktopCpu"),
        };
    }

    function initSelector(container, fetchImpl) {
        if (container.dataset.selectorReady === "true") {
            return;
        }
        container.dataset.selectorReady = "true";

        const form = container.querySelector("form");
        const purpose = form.querySelector("[name=purpose]");
        const modelInput = form.querySelector("[name=model]");
        const modelList = container.querySelector("[data-model-list]");
        const modelFact = container.querySelector("[data-model-fact]");
        const status = container.querySelector("[data-selector-status]");
        const documentRef = container.ownerDocument;
        const releasePromise = fetchLatestRelease(fetchImpl).catch(function unavailable() {
            return null;
        });

        if (modelList) {
            const fragment = documentRef.createDocumentFragment();

            for (const model of listModels()) {
                const option = documentRef.createElement("option");
                const nonDsmModel = nonDsmModelMetadata(model);
                option.value = model.model;
                option.label = nonDsmModel
                    ? `${model.cpuArch} · ${model.packageArch} · ${nonDsmModel.operatingSystem}, no DSM SPK`
                    : `${model.cpuArch} · ${model.packageArch}`;
                fragment.append(option);
            }

            modelList.append(fragment);
        }

        function updateModelFact() {
            const model = lookupModel(modelInput.value);
            const nonDsmModel = nonDsmModelMetadata(model);
            modelFact.textContent = nonDsmModel
                ? `Official table snapshot: ${model.cpuArch} · Package Arch ${model.packageArch} · ${nonDsmModel.operatingSystem}, DSM SPK unsupported`
                : model
                  ? `Official table snapshot: ${model.cpuArch} · Package Arch ${model.packageArch}`
                  : "Choose an exact model from the captured official table.";
        }

        purpose.addEventListener("change", function purposeChanged() {
            setPurposeFields(container, purpose.value);
        });
        modelInput.addEventListener("input", updateModelFact);
        setPurposeFields(container, purpose.value);
        updateModelFact();

        form.addEventListener("submit", async function submitted(event) {
            event.preventDefault();
            status.textContent = "Checking compatibility and the current GitHub Release…";

            const resolved = resolveSelection(formInput(form));

            if (!resolved.ok) {
                renderFailure(container, resolved);
                status.textContent = "Compatibility check stopped without recommending an artifact.";
                return;
            }

            const latestRelease = await releasePromise;
            const recommendation = materializeRecommendation(resolved, latestRelease);
            renderSuccess(container, recommendation);
            status.textContent = recommendation.exact
                ? "Exact current artifact found."
                : recommendation.kind === "container" && recommendation.releaseCorrelated
                  ? "Compatibility resolved; the release tag is current, but its GHCR publication still needs verification."
                  : recommendation.kind === "container"
                    ? "Compatibility resolved; showing a versioned image pattern because GHCR could not be verified."
                    : "Compatibility resolved; showing a fail-safe filename pattern because no exact live asset was found.";
        });
    }

    function initSelectors(documentRef, fetchImpl) {
        for (const container of documentRef.querySelectorAll("[data-release-selector]")) {
            initSelector(container, fetchImpl);
        }
    }

    return Object.freeze({
        FALLBACK_TAG,
        RELEASE_API_URL,
        dsmMinorBounds,
        initSelectors,
        listModels,
        lookupModel,
        materializeRecommendation,
        normalizeCpuArch,
        parseDsmVersion,
        resolveSelection,
    });
});
