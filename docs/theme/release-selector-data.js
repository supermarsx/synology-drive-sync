(function exposeReleaseSelectorData(root, factory) {
    const data = factory();

    if (typeof module === "object" && module.exports) {
        module.exports = data;
    }

    root.SDSYNC_RELEASE_SELECTOR_DATA = data;
})(typeof globalThis !== "undefined" ? globalThis : this, function createReleaseSelectorData() {
    "use strict";

    return Object.freeze({
        snapshotCapturedDate: "2026-08-26",
        sourceUrl:
            "https://kb.synology.com/en-us/DSM/tutorial/What_kind_of_CPU_does_my_NAS_have",
        sourceTitle: "Synology CPU and Package Arch table",
        lifecycleSourceUrl:
            "https://kb.synology.com/en-us/DSM/tutorial/What_is_the_last_upgradable_software_version_for_my_Synology_product",
        lifecycleSourceTitle: "Synology last upgradable software version table",
        supplementalModelSources: Object.freeze([
            Object.freeze({
                title: "Synology DSM 7.4.1-90080 PAT index",
                url: "https://archive.synology.com/download/Os/DSM/7.4.1-90080",
                purpose: "Exact physical model and first-build presence",
            }),
            Object.freeze({
                title: "Synology SynoOnlinePack_v2 1071 package index",
                url: "https://archive.synology.com/download/Package/SynoOnlinePack_v2/1071",
                purpose: "Exact model-to-Package-Arch correspondence",
            }),
        ]),
        archiveSources: Object.freeze([
            Object.freeze({ minor: 0, build: "7.0.1-42218", url: "https://archive.synology.com/download/Os/DSM/7.0.1-42218" }),
            Object.freeze({ minor: 1, build: "7.1-42661-1", url: "https://archive.synology.com/download/Os/DSM/7.1-42661-1" }),
            Object.freeze({ minor: 2, build: "7.2.2-72806", url: "https://archive.synology.com/download/Os/DSM/7.2.2-72806" }),
            Object.freeze({ minor: 3, build: "7.3-81180", url: "https://archive.synology.com/download/Os/DSM/7.3-81180" }),
            Object.freeze({ minor: 4, build: "7.4.1-90080", url: "https://archive.synology.com/download/Os/DSM/7.4.1-90080" }),
        ]),
        enterpriseSourceUrl: "https://www.synology.com/en-us/support/download/PAS7700",
        modelCount: 233,
        modelGroups: [
            { cpuArch: "armv5", packageArch: "88f628x", assetArch: null, models: ["DS110j", "DS111", "DS112", "DS112+", "DS112j", "DS210j", "DS211", "DS211+", "DS211j", "DS212", "DS212+", "DS212j", "DS213", "DS213air", "DS410j", "DS411", "DS411j", "DS411slim", "DS413j", "RS212", "RS411", "RS812"] },
            { cpuArch: "armv7", packageArch: "alpine", assetArch: "armv7", models: ["DS1515", "DS1517", "DS1817", "DS2015xs", "DS715"] },
            { cpuArch: "armv7", packageArch: "alpine4k", assetArch: "armv7", models: ["DS215+", "DS416"] },
            { cpuArch: "armv7", packageArch: "armada370", assetArch: "armv7", models: ["DS114", "DS115j", "DS213j", "DS214se", "DS216se", "DS414slim", "RS214"] },
            { cpuArch: "armv7", packageArch: "armada375", assetArch: "armv7", models: ["DS115", "DS215j"] },
            { cpuArch: "armv7", packageArch: "armada38x", assetArch: "armv7", models: ["DS116", "DS216", "DS216j", "DS218j", "DS416j", "DS416slim", "DS419slim", "RS217", "RS816"] },
            { cpuArch: "armv7", packageArch: "armadaxp", assetArch: "armv7", models: ["DS214", "DS214+", "DS414", "RS814", "RS815"] },
            { cpuArch: "armv7", packageArch: "comcerto2k", assetArch: "armv7", models: ["DS414j"] },
            { cpuArch: "armv7", packageArch: "monaco", assetArch: "armv7", models: ["DS216play"] },
            { cpuArch: "armv8", packageArch: "armada37xx", assetArch: "armv8", models: ["DS119j", "DS120j"] },
            { cpuArch: "armv8", packageArch: "rtd1296", assetArch: "armv8", models: ["DS118", "DS218", "DS218play", "DS220j", "DS418", "DS418j", "DS420j", "RS819"] },
            { cpuArch: "armv8", packageArch: "rtd1619b", assetArch: "armv8", models: ["DS124", "DS223", "DS223j", "DS423"] },
            { cpuArch: "i686", packageArch: "evansport", assetArch: "i686", models: ["DS214play", "DS415play"] },
            { cpuArch: "powerpc", packageArch: "ppc853x", assetArch: null, models: ["DS110+", "DS210+", "DS410"] },
            { cpuArch: "ppc", packageArch: "qoriq", assetArch: null, models: ["DS213+", "DS413"] },
            { cpuArch: "x86_64", packageArch: "apollolake", assetArch: "x86_64", models: ["DS1019+", "DS218+", "DS418play", "DS620slim", "DS718+", "DS918+"] },
            { cpuArch: "x86_64", packageArch: "avoton", assetArch: "x86_64", models: ["DS1515+", "DS1517+", "DS1815+", "DS1817+", "DS2415+", "DS415+", "RS1219+", "RS2416+", "RS2416RP+", "RS815+", "RS815RP+", "RS818+", "RS818RP+"] },
            { cpuArch: "x86_64", packageArch: "braswell", assetArch: "x86_64", models: ["DS216+", "DS216+II", "DS416play", "DS716+", "DS716+II", "DS916+"] },
            { cpuArch: "x86_64", packageArch: "broadwell", assetArch: "x86_64", models: ["DS3617xs", "DS3617xsII", "FS2017", "FS3400", "RS18017xs+", "RS3617RPxs", "RS3617xs+", "RS3618xs", "RS4017xs+"] },
            { cpuArch: "x86_64", packageArch: "broadwellnk", assetArch: "x86_64", models: ["DS1621xs+", "DS3018xs", "DS3622xs+", "FS1018", "FS3600", "RS1619xs+", "RS3621RPxs", "RS3621xs+", "RS4021xs+", "SA3400", "SA3600"] },
            { cpuArch: "x86_64", packageArch: "broadwellnkv2", assetArch: "x86_64", models: ["FS3410", "SA3410", "SA3610"] },
            { cpuArch: "x86_64", packageArch: "broadwellntbap", assetArch: "x86_64", models: ["SA3200D", "SA3400D"] },
            { cpuArch: "x86_64", packageArch: "bromolow", assetArch: "x86_64", models: ["DS3611xs", "DS3612xs", "DS3615xs", "RC18015xs+", "RS10613xs+", "RS18016xs+", "RS3411RPxs", "RS3411xs", "RS3412RPxs", "RS3412xs", "RS3413xs+", "RS3614RPxs", "RS3614xs", "RS3614xs+", "RS3617xs"] },
            { cpuArch: "x86_64", packageArch: "cedarview", assetArch: "x86_64", models: ["DS1512+", "DS1513+", "DS1812+", "DS1813+", "DS2413+", "DS412+", "DS713+", "RS2212+", "RS2212RP+", "RS2414+", "RS2414RP+", "RS812+", "RS812RP+", "RS814+", "RS814RP+"] },
            { cpuArch: "x86_64", packageArch: "denverton", assetArch: "x86_64", models: ["DS1618+", "DS1819+", "DS2419+", "DS2419+II", "DVA3219", "DVA3221", "RS2418+", "RS2418RP+", "RS2818RP+", "RS820+", "RS820RP+"] },
            { cpuArch: "x86_64", packageArch: "epyc7002", assetArch: "x86_64", models: ["SA6400"] },
            { cpuArch: "x86_64", packageArch: "epyc7003", assetArch: "x86_64", models: ["FS6420", "RS11626xs+"] },
            { cpuArch: "x86_64", packageArch: "epyc7003", assetArch: null, models: ["PAS7700"] },
            { cpuArch: "x86_64", packageArch: "geminilake", assetArch: "x86_64", models: ["DS1520+", "DS220+", "DS224+", "DS420+", "DS423+", "DS720+", "DS920+", "DVA1622"] },
            { cpuArch: "x86_64", packageArch: "geminilakenk", assetArch: "x86_64", models: ["DS225+", "DS425+", "FS200T"] },
            { cpuArch: "x86_64", packageArch: "grantley", assetArch: "x86_64", models: ["FS3017"] },
            { cpuArch: "x86_64", packageArch: "icelaked", assetArch: "x86_64", models: ["FS3420", "RS1626xs+", "RS3626xs", "RS4826xs+", "RS6426xs+"] },
            { cpuArch: "x86_64", packageArch: "purley", assetArch: "x86_64", models: ["FS6400", "HD6500"] },
            { cpuArch: "x86_64", packageArch: "r1000", assetArch: "x86_64", models: ["DS1522+", "DS723+", "DS923+", "RS422+"] },
            { cpuArch: "x86_64", packageArch: "r1000nk", assetArch: "x86_64", models: ["DS725+", "DS725neo+"] },
            { cpuArch: "x86_64", packageArch: "v1000", assetArch: "x86_64", models: ["DS1621+", "DS1821+", "DS1823xs+", "DS2422+", "FS2500", "RS1221+", "RS1221RP+", "RS2421+", "RS2421RP+", "RS2423+", "RS2423RP+", "RS2821RP+", "RS822+", "RS822RP+", "RS826+", "RS826RP+"] },
            { cpuArch: "x86_64", packageArch: "v1000nk", assetArch: "x86_64", models: ["DS1525+", "DS1525neo+", "DS1825+", "DS1825neo+", "DS925+", "DS925neo+", "DVA7400", "RS1226+", "RS1226RP+", "RS2423RP+II", "RS2825RP+"] },
            { cpuArch: "x86_64", packageArch: "x86", assetArch: null, models: ["DS1010+", "DS1511+", "DS2411+", "DS411+", "DS411+II", "DS710+", "DS712+", "RS2211+", "RS2211RP+", "RS810+", "RS810RP+"] },
        ],
        compatibilityGroups: [
            {
                productLine: "DSM",
                status: "legacy",
                lastVersion: "5.2",
                models: ["DS1010+", "DS110+", "DS110j", "DS210+", "DS210j", "DS410", "DS410j", "DS710+", "RS810+", "RS810RP+"],
            },
            {
                productLine: "DSM",
                status: "legacy",
                lastVersion: "6.2",
                models: ["DS111", "DS112", "DS112+", "DS112j", "DS1511+", "DS1512+", "DS1812+", "DS211", "DS211+", "DS211j", "DS212", "DS212+", "DS212j", "DS213", "DS213+", "DS213air", "DS2411+", "DS3611xs", "DS3612xs", "DS411", "DS411+", "DS411+II", "DS411j", "DS411slim", "DS412+", "DS413", "DS413j", "DS712+", "RS212", "RS2211+", "RS2211RP+", "RS2212+", "RS2212RP+", "RS3411RPxs", "RS3411xs", "RS3412RPxs", "RS3412xs", "RS411", "RS812", "RS812+", "RS812RP+"],
            },
            {
                productLine: "DSM",
                status: "supported",
                minMinor: 0,
                maxMinor: 1,
                models: ["DS114", "DS115", "DS115j", "DS1513+", "DS1515", "DS1515+", "DS1813+", "DS1815+", "DS2015xs", "DS213j", "DS214", "DS214+", "DS214play", "DS214se", "DS215+", "DS215j", "DS216se", "DS2413+", "DS2415+", "DS3615xs", "DS414", "DS414j", "DS414slim", "DS415+", "DS415play", "DS713+", "DS715", "RC18015xs+", "RS10613xs+", "RS214", "RS2414+", "RS2414RP+", "RS3413xs+", "RS3614RPxs", "RS3614xs", "RS3614xs+", "RS814", "RS814+", "RS814RP+", "RS815", "RS815+", "RS815RP+"],
            },
            {
                productLine: "DSM",
                status: "supported",
                minMinor: 0,
                maxMinor: 4,
                models: ["DS1019+", "DS116", "DS118", "DS119j", "DS120j", "DS1517", "DS1517+", "DS1520+", "DS1618+", "DS1621+", "DS1621xs+", "DS1817", "DS1817+", "DS1819+", "DS1821+", "DS216", "DS216+", "DS216+II", "DS216j", "DS216play", "DS218", "DS218+", "DS218j", "DS218play", "DS220+", "DS220j", "DS2419+", "DS2419+II", "DS2422+", "DS3018xs", "DS3617xs", "DS3617xsII", "DS3622xs+", "DS416", "DS416j", "DS416play", "DS416slim", "DS418", "DS418j", "DS418play", "DS419slim", "DS420+", "DS420j", "DS620slim", "DS716+", "DS716+II", "DS718+", "DS720+", "DS916+", "DS918+", "DS920+", "DVA3219", "DVA3221", "FS1018", "FS2017", "FS2500", "FS3017", "FS3400", "FS3600", "FS6400", "RS1219+", "RS1221+", "RS1221RP+", "RS1619xs+", "RS18016xs+", "RS18017xs+", "RS217", "RS2416+", "RS2416RP+", "RS2418+", "RS2418RP+", "RS2421+", "RS2421RP+", "RS2818RP+", "RS2821RP+", "RS3617RPxs", "RS3617xs", "RS3617xs+", "RS3618xs", "RS3621RPxs", "RS3621xs+", "RS4017xs+", "RS4021xs+", "RS816", "RS818+", "RS818RP+", "RS819", "RS820+", "RS820RP+", "SA3200D", "SA3400", "SA3600"],
            },
            {
                productLine: "DSM",
                status: "supported",
                minMinor: 1,
                maxMinor: 4,
                models: ["DS1522+", "DVA1622", "FS3410", "HD6500", "RS422+", "RS822+", "RS822RP+"],
            },
            {
                productLine: "DSM",
                status: "supported",
                minMinor: 2,
                maxMinor: 4,
                models: ["DS124", "DS1525+", "DS1823xs+", "DS1825+", "DS223", "DS223j", "DS224+", "DS225+", "DS423", "DS423+", "DS425+", "DS723+", "DS725+", "DS923+", "DS925+", "RS2423+", "RS2423RP+", "RS2825RP+", "SA3400D", "SA3410", "SA3610", "SA6400"],
            },
            {
                productLine: "DSM",
                status: "supported",
                minMinor: 4,
                maxMinor: 4,
                models: ["FS200T", "FS3420", "FS6420", "RS1226+", "RS1226RP+", "RS1626xs+", "RS2423RP+II", "RS3626xs", "RS4826xs+", "RS6426xs+", "RS826+", "RS826RP+"],
            },
            {
                productLine: "DSM",
                status: "supported",
                minMinor: 4,
                maxMinor: 4,
                minPatch: 1,
                minBuild: 90080,
                models: ["DS1525neo+", "DS1825neo+", "DS725neo+", "DS925neo+", "DVA7400", "RS11626xs+"],
            },
            {
                productLine: "DSM Enterprise",
                status: "unsupported-product-line",
                lastVersion: "1.0",
                models: ["PAS7700"],
            },
        ],
    });
});
