"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");

const selectorData = require("../docs/theme/release-selector-data.js");
const selector = require("../docs/theme/release-selector.js");

function resolve(overrides) {
    return selector.resolveSelection({
        purpose: "dsm-spk",
        model: "DS419slim",
        dsmVersion: "7.2.2-72806",
        reportedArch: "ARMv7 Processor rev 1 (v7l)",
        ...overrides,
    });
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
        'name="dsmVersion"',
        'name="reportedArch"',
        'name="desktopOs"',
        'name="desktopCpu"',
        "data-selector-result",
    ]) {
        assert.match(block[1], new RegExp(control));
    }
});

test("captured official table has unique normalized model mappings", () => {
    const models = selector.listModels();
    const normalized = new Set(models.map((record) => record.model.toUpperCase()));

    assert.equal(selectorData.snapshotCapturedDate, "2026-08-24");
    assert.equal(
        selectorData.sourceUrl,
        "https://kb.synology.com/en-us/DSM/tutorial/What_kind_of_CPU_does_my_NAS_have",
    );
    assert.equal(models.length, 231);
    assert.equal(normalized.size, models.length);
    assert.deepEqual(selector.lookupModel(" ds419SLIM "), {
        model: "DS419slim",
        cpuArch: "armv7",
        packageArch: "armada38x",
    });
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

test("DSM platform introductions fail before 7.1, 7.2, and 7.3", () => {
    const cases = [
        ["SA6400", "epyc7002", 1],
        ["DS225+", "geminilakenk", 2],
        ["FS6420", "epyc7003", 3],
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

        assert.equal(before.code, "platform_dsm_conflict", `${packageArch} before DSM 7.${firstMinor}`);
        assert.equal(introduced.ok, true, `${packageArch} on DSM 7.${firstMinor}`);
        assert.match(introduced.detected, new RegExp(`Toolkit DSM 7\\.${firstMinor}–7\\.4`));
    }
});

test("PAS7700 remains factual data but never receives a DSM 7 SPK", () => {
    assert.deepEqual(selector.lookupModel("PAS7700"), {
        model: "PAS7700",
        cpuArch: "x86_64",
        packageArch: "epyc7003",
    });

    for (const dsmVersion of ["7.3-99999", "7.4-99999", "not-a-dsm-version"]) {
        const result = resolve({
            model: "PAS7700",
            dsmVersion,
            reportedArch: "x86_64",
        });

        assert.equal(result.ok, false);
        assert.equal(result.code, "non_dsm_model");
        assert.match(result.message, /DSM Enterprise 1\.0/);
        assert.match(result.details, /support\/download\/PAS7700/);
    }
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
    assert.equal(impossible.code, "platform_dsm_conflict");
});

test("legacy ARMv7 platform branches are not guessed onto DSM 7.2", () => {
    const result = resolve({
        model: "DS114",
        dsmVersion: "7.2-64570",
        reportedArch: "armv7l",
    });

    assert.equal(result.ok, false);
    assert.equal(result.code, "platform_dsm_conflict");
});

test("DSM minimum, legacy CPUs, unknown models, and conflicts fail closed", () => {
    assert.equal(resolve({ dsmVersion: "7.0-40758" }).code, "dsm_too_old");
    assert.equal(resolve({ dsmVersion: "7.1-1" }).code, "dsm_too_old");
    assert.equal(resolve({ dsmVersion: "prefix 7.2.2-72806 suffix" }).code, "invalid_dsm_version");
    assert.equal(resolve({ dsmVersion: "8.0-80000" }).code, "dsm_unverified_major");
    assert.equal(resolve({ dsmVersion: "7.5-80000" }).code, "dsm_unverified_minor");
    assert.equal(
        resolve({ model: "DS212j", dsmVersion: "7.0-41890", reportedArch: "armv5" })
            .code,
        "legacy_cpu",
    );
    assert.equal(
        resolve({ model: "DS213+", dsmVersion: "7.0-41890", reportedArch: "ppc" })
            .code,
        "legacy_cpu",
    );
    assert.equal(resolve({ model: "DS99999" }).code, "unknown_model");
    assert.equal(resolve({ reportedArch: "aarch64" }).code, "architecture_conflict");
    assert.equal(resolve({ reportedArch: "aarch64 armv7l" }).code, "unknown_reported_arch");
    assert.equal(resolve({ reportedArch: "x86_64 i686" }).code, "unknown_reported_arch");
    assert.equal(resolve({ reportedArch: "junkarmv7fake" }).code, "unknown_reported_arch");
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
