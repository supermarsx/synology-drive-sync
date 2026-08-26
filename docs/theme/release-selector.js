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

    if (
        !data ||
        !Array.isArray(data.modelGroups) ||
        !Array.isArray(data.compatibilityGroups) ||
        !Array.isArray(data.supplementalModelSources)
    ) {
        throw new Error("release selector model data is unavailable");
    }

    const REPOSITORY = "supermarsx/synology-drive-sync";
    const RELEASES_URL = `https://github.com/${REPOSITORY}/releases/latest`;
    const ALL_RELEASES_URL = `https://github.com/${REPOSITORY}/releases`;
    const RELEASE_API_URL = `https://api.github.com/repos/${REPOSITORY}/releases/latest`;
    const PACKAGE_URL = `https://github.com/${REPOSITORY}/pkgs/container/synology-drive-sync`;
    const FALLBACK_TAG = "YY.N";
    const KNOWN_INVALID_DSM_SPK_RELEASES = Object.freeze({
        "26.5":
            "Release 26.5 SPKs contain identity-changing/set-ID privilege metadata that DSM rejects for a third-party package.",
        "26.6":
            "Release 26.6 SPKs request the Synology-only sysnotify resource worker, which DSM rejects for a third-party package.",
    });
    const DSM_PACKAGE_MAXIMUM = Object.freeze({
        major: 7,
        minor: 4,
        build: 99999,
        display: "7.4-99999",
    });

    const DSM_ASSET_ARCHES = new Set(["armv7", "armv8", "i686", "x86_64"]);
    const DSM_ASSET_CPU_ARCH = Object.freeze({
        armv7: "armv7",
        armv8: "armv8",
        i686: "i686",
        x86_64: "x86_64",
    });
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
    const PRODUCT_LINES = new Set(["dsm", "dsm-enterprise"]);

    function normalizedModel(value) {
        return String(value || "")
            .trim()
            .toUpperCase()
            .replace(/\s+/g, "");
    }

    function buildModelIndex(modelGroups) {
        const index = new Map();

        for (const group of modelGroups) {
            if (
                !group ||
                typeof group !== "object" ||
                !Array.isArray(group.models) ||
                !Object.prototype.hasOwnProperty.call(group, "assetArch")
            ) {
                throw new Error("invalid Synology model snapshot group");
            }
            const cpuArch = String(group.cpuArch || "").toLowerCase();
            const packageArch = String(group.packageArch || "").toLowerCase();
            const assetArch = group.assetArch === null
                ? null
                : String(group.assetArch || "").toLowerCase();

            if (
                !cpuArch ||
                !packageArch ||
                !group.models.length ||
                (assetArch !== null && !DSM_ASSET_ARCHES.has(assetArch)) ||
                (assetArch !== null && normalizeCpuArch(cpuArch) !== DSM_ASSET_CPU_ARCH[assetArch])
            ) {
                throw new Error(`invalid Synology model snapshot mapping: ${packageArch || "unknown"}`);
            }

            for (const model of group.models) {
                const key = normalizedModel(model);
                const record = Object.freeze({
                    model,
                    cpuArch,
                    packageArch,
                    assetArch,
                });
                const existing = index.get(key);

                if (
                    existing &&
                    (existing.cpuArch !== record.cpuArch ||
                        existing.packageArch !== record.packageArch ||
                        existing.assetArch !== record.assetArch)
                ) {
                    throw new Error(`conflicting Synology model snapshot entry: ${model}`);
                }

                index.set(key, record);
            }
        }

        return index;
    }

    const MODEL_INDEX = buildModelIndex(data.modelGroups);

    function buildPackageAssetIndex(modelGroups) {
        const index = new Map();

        for (const group of modelGroups) {
            if (group.assetArch === null) {
                continue;
            }
            const packageArch = String(group.packageArch).toLowerCase();
            const assetArch = String(group.assetArch).toLowerCase();
            const existing = index.get(packageArch);

            if (existing && existing !== assetArch) {
                throw new Error(`conflicting DSM package asset mapping: ${packageArch}`);
            }
            index.set(packageArch, assetArch);
        }

        return index;
    }

    const PACKAGE_ASSET_INDEX = buildPackageAssetIndex(data.modelGroups);

    function buildCompatibilityIndex(groups) {
        const index = new Map();

        for (const group of groups) {
            const minPatch = Number.isInteger(group.minPatch) ? group.minPatch : null;
            const minBuild = Number.isInteger(group.minBuild) ? group.minBuild : null;

            if ((minPatch === null) !== (minBuild === null)) {
                throw new Error("incomplete Synology model minimum-build mapping");
            }

            for (const model of group.models) {
                const key = normalizedModel(model);

                if (index.has(key)) {
                    throw new Error(`duplicate Synology lifecycle entry: ${model}`);
                }

                index.set(
                    key,
                    Object.freeze({
                        productLine: group.productLine,
                        status: group.status,
                        minMinor: Number.isInteger(group.minMinor) ? group.minMinor : null,
                        maxMinor: Number.isInteger(group.maxMinor) ? group.maxMinor : null,
                        minPatch,
                        minBuild,
                        lastVersion: group.lastVersion || null,
                    }),
                );
            }
        }

        return index;
    }

    const MODEL_COMPATIBILITY_INDEX = buildCompatibilityIndex(data.compatibilityGroups);

    if (
        !Number.isInteger(data.modelCount) ||
        data.modelCount !== MODEL_INDEX.size ||
        MODEL_COMPATIBILITY_INDEX.size !== MODEL_INDEX.size ||
        [...MODEL_INDEX.keys()].some(function missingCompatibility(model) {
            return !MODEL_COMPATIBILITY_INDEX.has(model);
        }) ||
        [...MODEL_COMPATIBILITY_INDEX.keys()].some(function unknownCompatibility(model) {
            return !MODEL_INDEX.has(model);
        })
    ) {
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

    function modelCompatibility(model) {
        const key = typeof model === "object" && model ? model.model : model;

        return MODEL_COMPATIBILITY_INDEX.get(normalizedModel(key)) || null;
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

    function normalizeProductLine(value) {
        const normalized = String(value || "")
            .trim()
            .toLowerCase()
            .replace(/[_\s]+/g, "-");

        if (normalized === "dsm" || normalized === "diskstation-manager") {
            return "dsm";
        }
        if (normalized === "dsm-enterprise" || normalized === "dsmenterprise") {
            return "dsm-enterprise";
        }

        return null;
    }

    function parseOsVersionSelection(value) {
        const normalized = String(value || "").trim().toLowerCase();
        const dsm7 = /^dsm-7\.([0-4])$/.exec(normalized);

        if (dsm7) {
            return Object.freeze({
                key: normalized,
                productLine: "dsm",
                display: `DSM 7.${dsm7[1]}`,
                supported: true,
                major: 7,
                minor: Number(dsm7[1]),
            });
        }
        if (normalized === "dsm-6.x-or-earlier") {
            return Object.freeze({
                key: normalized,
                productLine: "dsm",
                display: "DSM 6.x or earlier",
                supported: false,
                major: 6,
                minor: null,
            });
        }
        if (normalized === "dsm-enterprise-1.0") {
            return Object.freeze({
                key: normalized,
                productLine: "dsm-enterprise",
                display: "DSM Enterprise 1.0",
                supported: false,
                major: 1,
                minor: 0,
            });
        }

        return null;
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
        return PACKAGE_ASSET_INDEX.get(String(packageArch || "").toLowerCase()) || null;
    }

    function dsmMinorBounds(packageArch) {
        return DSM_PLATFORM_MINOR_BOUNDS[String(packageArch || "").toLowerCase()] || null;
    }

    function dsmMinorRangeLabel(bounds) {
        return bounds.min === bounds.max
            ? `DSM 7.${bounds.min}`
            : `DSM 7.${bounds.min}–7.${bounds.max}`;
    }

    function compatibilityLabel(compatibility) {
        if (compatibility.productLine === "DSM Enterprise") {
            return `DSM Enterprise ${compatibility.lastVersion} · no published SPK`;
        }
        if (compatibility.status === "legacy") {
            return `last upgradable DSM ${compatibility.lastVersion} · no published SPK`;
        }
        if (compatibility.minBuild !== null) {
            return `DSM 7.${compatibility.minMinor}.${compatibility.minPatch}-${compatibility.minBuild} or newer on DSM 7.${compatibility.maxMinor}`;
        }

        return dsmMinorRangeLabel({
            min: compatibility.minMinor,
            max: compatibility.maxMinor,
        });
    }

    function modelAssetLabel(model, compatibility) {
        if (
            !model ||
            !compatibility ||
            compatibility.status !== "supported" ||
            !model.assetArch
        ) {
            return "manual review / no SPK";
        }

        return `synology-drive-sync-YY.N-${model.assetArch}.spk`;
    }

    const DSM_ASSET_PACKAGES = new Set(PACKAGE_ASSET_INDEX.keys());

    if (
        Object.keys(DSM_PLATFORM_MINOR_BOUNDS).length !== DSM_ASSET_PACKAGES.size ||
        [...DSM_ASSET_PACKAGES].some(function missingBounds(packageArch) {
            return !dsmMinorBounds(packageArch);
        }) ||
        Object.keys(DSM_PLATFORM_MINOR_BOUNDS).some(function missingAsset(packageArch) {
            return !DSM_ASSET_PACKAGES.has(packageArch);
        }) ||
        [...MODEL_INDEX.values()].some(function inconsistentModelAsset(model) {
            return model.assetArch !== null &&
                packageAssetArch(model.packageArch) !== model.assetArch;
        })
    ) {
        throw new Error("DSM asset families and official toolkit intervals are out of sync");
    }

    function resolveDsmSelection(input) {
        const catalogModel = lookupModel(input.model);
        if (!catalogModel) {
            return failure(
                "unknown_model",
                "That model is not in the captured physical Synology model catalog.",
                `Check Synology's live CPU/Package Arch table and model-specific archives (snapshot captured ${data.snapshotCapturedDate}), then report the exact model, DSM build, and Package Arch. The selector will not guess from a model year or marketing name.`,
            );
        }

        const compatibility = modelCompatibility(catalogModel);
        const productLine = normalizeProductLine(input.productLine);
        const osVersion = parseOsVersionSelection(input.osVersion);

        if (!productLine || !PRODUCT_LINES.has(productLine)) {
            return failure(
                "invalid_product_line",
                "Select the operating-system product line reported by the NAS.",
                "Choose DiskStation Manager (DSM) or DSM Enterprise; sharing a CPU family does not make their package lifecycles interchangeable.",
            );
        }
        if (!osVersion) {
            return failure(
                "invalid_os_version_selection",
                "Select one of the catalogued Synology OS versions.",
                `The selector covers DSM 7.0 through 7.4, informational DSM 6.x or earlier, and DSM Enterprise 1.0 as captured ${data.snapshotCapturedDate}. Newer or unknown lines fail closed until the catalog is refreshed.`,
            );
        }
        if (productLine !== osVersion.productLine) {
            return failure(
                "os_selection_conflict",
                "The selected product line and OS version belong to different Synology products.",
                "Choose a DSM version with DiskStation Manager, or DSM Enterprise 1.0 with DSM Enterprise.",
            );
        }

        const expectedProductLine =
            compatibility.productLine === "DSM Enterprise" ? "dsm-enterprise" : "dsm";

        if (productLine !== expectedProductLine) {
            return failure(
                "product_line_conflict",
                `${catalogModel.model} is catalogued for ${compatibility.productLine}, not ${productLine === "dsm" ? "DSM" : "DSM Enterprise"}.`,
                `Recheck the exact model and OS product. The selector does not transfer a package between product lines. Source: ${expectedProductLine === "dsm-enterprise" ? data.enterpriseSourceUrl : data.lifecycleSourceUrl}`,
            );
        }
        if (compatibility.status === "unsupported-product-line") {
            return failure(
                "unsupported_product_line",
                `${catalogModel.model} runs DSM Enterprise ${compatibility.lastVersion}; no ordinary DSM SPK is offered.`,
                `DSM Enterprise has a separate package lifecycle that this release does not publish or claim to support. Confirm the product in Synology's Download Center: ${data.enterpriseSourceUrl}`,
            );
        }
        if (!osVersion.supported) {
            const lastKnown = compatibility.lastVersion
                ? ` Synology lists ${catalogModel.model}'s last upgradable DSM as ${compatibility.lastVersion}.`
                : "";

            return failure(
                "unsupported_dsm_line",
                `${osVersion.display} is informational only; this release publishes DSM 7 SPKs.`,
                `${lastKnown} A safe older-DSM port needs its own toolchain, package metadata, lifecycle tests, and release assets. Desktop CLI or a container on a supported workstation is the safe alternative.`,
            );
        }
        if (compatibility.status !== "supported") {
            return failure(
                "model_dsm_conflict",
                `${catalogModel.model} cannot run the selected ${osVersion.display}; Synology lists its last upgradable DSM as ${compatibility.lastVersion}.`,
                `Use the official lifecycle table (${data.lifecycleSourceUrl}) and Download Center for that exact model. No incompatible DSM 7 asset is offered; use the desktop CLI or container on another supported system instead.`,
            );
        }
        if (
            osVersion.minor < compatibility.minMinor ||
            osVersion.minor > compatibility.maxMinor
        ) {
            const modelRange = dsmMinorRangeLabel({
                min: compatibility.minMinor,
                max: compatibility.maxMinor,
            });

            return failure(
                "model_dsm_conflict",
                `${catalogModel.model} is catalogued for ${modelRange}, not ${osVersion.display}.`,
                `Model-specific availability is checked separately from CPU Package Arch. Review Synology's lifecycle table and archived release directories captured ${data.snapshotCapturedDate}; the selector does not infer support from the model year.`,
            );
        }

        const exactVersionInput = String(input.dsmVersion || "").trim();
        const version = exactVersionInput ? parseDsmVersion(exactVersionInput) : null;

        if (exactVersionInput && !version) {
            return failure(
                "invalid_dsm_version",
                "The exact build, when supplied, must use a complete form such as 7.2.2-72806.",
            );
        }
        if ((osVersion.minor === 0 || osVersion.minor === 4) && !version) {
            const boundary = osVersion.minor === 0 ? "minimum" : "maximum";
            const manifestVersion =
                osVersion.minor === 0 ? "DSM 7.0-40759" : DSM_PACKAGE_MAXIMUM.display;

            return failure(
                "dsm_build_required",
                `${osVersion.display} requires the exact installed build before an SPK can be recommended.`,
                `The package ${boundary} is ${manifestVersion}. Enter the complete installed version/build so the selector can prove that compatibility boundary.`,
            );
        }
        if (
            version &&
            (version.major !== osVersion.major || version.minor !== osVersion.minor)
        ) {
            return failure(
                "os_version_conflict",
                `The exact build ${version.display} conflicts with the selected ${osVersion.display} branch.`,
                "Recheck Control Panel > Info Center. The branch selector and exact build, when supplied, must describe the same installed OS.",
            );
        }
        if (
            version &&
            version.major === 7 &&
            version.minor === 0 &&
            version.patch === 0 &&
            version.build < 40759
        ) {
            return failure(
                "dsm_too_old",
                "This package requires DSM 7.0-40759 or newer.",
                "An older build requires a separately designed package; changing the INFO label would not make this DSM 7 package compatible.",
            );
        }
        if (
            version &&
            compatibility.minBuild !== null &&
            (version.patch < compatibility.minPatch ||
                (version.patch === compatibility.minPatch &&
                    version.build < compatibility.minBuild))
        ) {
            return failure(
                "model_build_too_old",
                `${catalogModel.model} first appears in the captured DSM 7.${compatibility.minMinor}.${compatibility.minPatch}-${compatibility.minBuild} model index.`,
                "Choose the exact installed model and build. The selector will not backfill a newly introduced model onto an older DSM image.",
            );
        }
        if (
            version &&
            version.major === DSM_PACKAGE_MAXIMUM.major &&
            version.minor === DSM_PACKAGE_MAXIMUM.minor &&
            version.build > DSM_PACKAGE_MAXIMUM.build
        ) {
            return failure(
                "dsm_too_new",
                `This package declares a maximum supported DSM version/build of ${DSM_PACKAGE_MAXIMUM.display}.`,
                "A later DSM 7.4 build needs a package whose INFO compatibility ceiling has been reviewed and tested; the selector will not recommend this artifact beyond its declared maximum.",
            );
        }

        const model = catalogModel;

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
        const assetArch = model.assetArch;

        if (!assetArch) {
            return failure(
                "unsupported_package_arch",
                `No verified DSM artifact is assigned to ${model.model} (${model.cpuArch}/${model.packageArch}).`,
                "Use the exact official Package Arch to request a new verified build; do not install a relabeled binary.",
            );
        }
        if (packageAssetArch(model.packageArch) !== assetArch) {
            return failure(
                "asset_mapping_conflict",
                `The captured ${model.model} model-to-asset mapping is inconsistent.`,
                "Refresh and validate the canonical Synology model catalog before recommending an SPK.",
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
        const selectedMinor = osVersion.minor;

        if (selectedMinor < bounds.min || selectedMinor > bounds.max) {
            return failure(
                "platform_dsm_conflict",
                `${model.model}/${model.packageArch} is present in the official ${dsmMinorRangeLabel(bounds)} toolkit interval, not DSM 7.${selectedMinor}.`,
                "Recheck the DSM version and model. The selector does not infer support before a platform was introduced or after Synology removed it from pkgscripts-ng.",
            );
        }

        const modelBounds = Object.freeze({
            min: compatibility.minMinor,
            max: compatibility.maxMinor,
        });
        const versionDisplay = version
            ? version.display
            : `7.${selectedMinor} (exact build not supplied)`;

        return Object.freeze({
            ok: true,
            kind: "release-asset",
            artifactType: "dsm-spk",
            purpose: "DSM dashboard package (SPK)",
            assetTemplate: `synology-drive-sync-{tag}-${assetArch}.spk`,
            detected: `${model.model} · Product line DSM · Processor ${model.cpuArch} · Package Arch ${model.packageArch} · Release asset ${assetArch} · DSM ${versionDisplay} · Model ${compatibilityLabel(compatibility)} · Toolkit ${dsmMinorRangeLabel(bounds)}`,
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
            !/^\d{2}\.[1-9]\d*$/.test(String(payload.tag_name || ""))
        ) {
            return null;
        }

        return payload;
    }

    function isOfficialAssetUrl(value, tag, name) {
        try {
            const parsed = new URL(value);

            return (
                parsed.origin === "https://github.com" &&
                !parsed.username &&
                !parsed.password &&
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

        const invalidDsmSpkReason =
            release && recommendation.artifactType === "dsm-spk"
                ? KNOWN_INVALID_DSM_SPK_RELEASES[tag]
                : null;

        if (invalidDsmSpkReason) {
            return Object.freeze({
                ...recommendation,
                ok: false,
                code: "known_invalid_dsm_spk_release",
                message: `Do not download or install DSM SPKs from release ${tag}.`,
                details: `${invalidDsmSpkReason} Review GitHub Releases for a fixed release (26.7 or newer); the selector will verify its exact asset before offering a download.`,
                tag,
                exact: false,
                releaseCorrelated: true,
                downloadUrl: ALL_RELEASES_URL,
                actionLabel: "Review GitHub Releases",
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
        const children = [heading, summary, detail];

        heading.textContent = "No safe recommendation";
        summary.className = "selector-result-summary";
        summary.textContent = recommendation.message;
        detail.textContent = recommendation.details;
        if (recommendation.downloadUrl) {
            const link = documentRef.createElement("a");
            link.className = "selector-download";
            link.href = recommendation.downloadUrl;
            link.textContent = recommendation.actionLabel || "Review GitHub Releases";
            children.push(link);
        }
        result.replaceChildren(...children);
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
            productLine: values.get("productLine"),
            osVersion: values.get("osVersion"),
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
        const modelInput = form.querySelector("[data-model-select]");
        const modelCount = container.querySelector("[data-model-count]");
        const modelFact = container.querySelector("[data-model-fact]");
        const status = container.querySelector("[data-selector-status]");
        const documentRef = container.ownerDocument;
        const releasePromise = fetchLatestRelease(fetchImpl).catch(function unavailable() {
            return null;
        });

        if (modelInput) {
            const fragment = documentRef.createDocumentFragment();
            const models = listModels();

            for (const model of models) {
                const option = documentRef.createElement("option");
                const compatibility = modelCompatibility(model);
                option.value = model.model;
                option.textContent = `${model.model} — processor ${model.cpuArch} — Package Arch ${model.packageArch} — ${modelAssetLabel(model, compatibility)} — ${compatibilityLabel(compatibility)}`;
                fragment.append(option);
            }

            modelInput.append(fragment);
            if (modelCount) {
                modelCount.textContent = `${models.length} exact models loaded`;
            }
        }

        function updateModelFact() {
            const model = lookupModel(modelInput.value);
            const compatibility = modelCompatibility(model);
            if (model) {
                modelFact.textContent = `Official snapshot: processor ${model.cpuArch} · Package Arch ${model.packageArch} · Release ${modelAssetLabel(model, compatibility)} · Product line ${compatibility.productLine} · ${compatibilityLabel(compatibility)}`;
            } else if (modelInput.value === "__unknown__") {
                modelFact.textContent = `This model is outside the ${data.snapshotCapturedDate} physical catalog. No SPK is recommended; use Synology's live CPU table and model-specific archives for manual review.`;
            } else {
                modelFact.textContent = `Choose one of the ${data.modelCount} exact models in the captured physical catalog.`;
            }
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
            if (!recommendation.ok) {
                renderFailure(container, recommendation);
                status.textContent =
                    "The current DSM package release is blocked; no download was recommended.";
                return;
            }
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
        modelCompatibility,
        materializeRecommendation,
        normalizeCpuArch,
        parseDsmVersion,
        parseOsVersionSelection,
        resolveSelection,
    });
});
