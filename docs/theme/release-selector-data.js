(function exposeReleaseSelectorData(root, factory) {
    const data = factory();

    if (typeof module === "object" && module.exports) {
        module.exports = data;
    }

    root.SDSYNC_RELEASE_SELECTOR_DATA = data;
})(typeof globalThis !== "undefined" ? globalThis : this, function createReleaseSelectorData() {
    "use strict";

    return Object.freeze({
        snapshotCapturedDate: "2026-08-24",
        sourceUrl:
            "https://kb.synology.com/en-us/DSM/tutorial/What_kind_of_CPU_does_my_NAS_have",
        sourceTitle: "Synology CPU and Package Arch table",
        modelCount: 231,
        modelGroups: [
            { cpuArch: "armv5", packageArch: "88f628x", models: ["DS110j", "DS111", "DS112", "DS112+", "DS112j", "DS210j", "DS211", "DS211+", "DS211j", "DS212", "DS212+", "DS212j", "DS213", "DS213air", "DS410j", "DS411", "DS411j", "DS411slim", "DS413j", "RS212", "RS411", "RS812"] },
            { cpuArch: "armv7", packageArch: "alpine", models: ["DS1515", "DS1517", "DS1817", "DS2015xs", "DS715"] },
            { cpuArch: "armv7", packageArch: "alpine4k", models: ["DS215+", "DS416"] },
            { cpuArch: "armv7", packageArch: "armada370", models: ["DS114", "DS115j", "DS213j", "DS214se", "DS216se", "DS414slim", "RS214"] },
            { cpuArch: "armv7", packageArch: "armada375", models: ["DS115", "DS215j"] },
            { cpuArch: "armv7", packageArch: "armada38x", models: ["DS116", "DS216", "DS216j", "DS218j", "DS416j", "DS416slim", "DS419slim", "RS217", "RS816"] },
            { cpuArch: "armv7", packageArch: "armadaxp", models: ["DS214", "DS214+", "DS414", "RS814", "RS815"] },
            { cpuArch: "armv7", packageArch: "comcerto2k", models: ["DS414j"] },
            { cpuArch: "armv7", packageArch: "monaco", models: ["DS216play"] },
            { cpuArch: "armv8", packageArch: "armada37xx", models: ["DS119j", "DS120j"] },
            { cpuArch: "armv8", packageArch: "rtd1296", models: ["DS118", "DS218", "DS218play", "DS220j", "DS418", "DS418j", "DS420j", "RS819"] },
            { cpuArch: "armv8", packageArch: "rtd1619b", models: ["DS124", "DS223", "DS223j", "DS423"] },
            { cpuArch: "i686", packageArch: "evansport", models: ["DS214play", "DS415play"] },
            { cpuArch: "powerpc", packageArch: "ppc853x", models: ["DS110+", "DS210+", "DS410"] },
            { cpuArch: "ppc", packageArch: "qoriq", models: ["DS213+", "DS413"] },
            { cpuArch: "x86_64", packageArch: "apollolake", models: ["DS1019+", "DS218+", "DS418play", "DS620slim", "DS718+", "DS918+"] },
            { cpuArch: "x86_64", packageArch: "avoton", models: ["DS1515+", "DS1517+", "DS1815+", "DS1817+", "DS2415+", "DS415+", "RS1219+", "RS2416+", "RS2416RP+", "RS815+", "RS815RP+", "RS818+", "RS818RP+"] },
            { cpuArch: "x86_64", packageArch: "braswell", models: ["DS216+", "DS216+II", "DS416play", "DS716+", "DS716+II", "DS916+"] },
            { cpuArch: "x86_64", packageArch: "broadwell", models: ["DS3617xs", "DS3617xsII", "FS2017", "FS3400", "RS18017xs+", "RS3617RPxs", "RS3617xs+", "RS3618xs", "RS4017xs+"] },
            { cpuArch: "x86_64", packageArch: "broadwellnk", models: ["DS1621xs+", "DS3018xs", "DS3622xs+", "FS1018", "FS3600", "RS1619xs+", "RS3621RPxs", "RS3621xs+", "RS4021xs+", "SA3400", "SA3600"] },
            { cpuArch: "x86_64", packageArch: "broadwellnkv2", models: ["FS3410", "SA3410", "SA3610"] },
            { cpuArch: "x86_64", packageArch: "broadwellntbap", models: ["SA3200D", "SA3400D"] },
            { cpuArch: "x86_64", packageArch: "bromolow", models: ["DS3611xs", "DS3612xs", "DS3615xs", "RC18015xs+", "RS10613xs+", "RS18016xs+", "RS3411RPxs", "RS3411xs", "RS3412RPxs", "RS3412xs", "RS3413xs+", "RS3614RPxs", "RS3614xs", "RS3614xs+", "RS3617xs"] },
            { cpuArch: "x86_64", packageArch: "cedarview", models: ["DS1512+", "DS1513+", "DS1812+", "DS1813+", "DS2413+", "DS412+", "DS713+", "RS2212+", "RS2212RP+", "RS2414+", "RS2414RP+", "RS812+", "RS812RP+", "RS814+", "RS814RP+"] },
            { cpuArch: "x86_64", packageArch: "denverton", models: ["DS1618+", "DS1819+", "DS2419+", "DS2419+II", "DVA3219", "DVA3221", "RS2418+", "RS2418RP+", "RS2818RP+", "RS820+", "RS820RP+"] },
            { cpuArch: "x86_64", packageArch: "epyc7002", models: ["SA6400"] },
            { cpuArch: "x86_64", packageArch: "epyc7003", models: ["FS6420", "PAS7700"] },
            { cpuArch: "x86_64", packageArch: "geminilake", models: ["DS1520+", "DS220+", "DS224+", "DS420+", "DS423+", "DS720+", "DS920+", "DVA1622"] },
            { cpuArch: "x86_64", packageArch: "geminilakenk", models: ["DS225+", "DS425+", "FS200T"] },
            { cpuArch: "x86_64", packageArch: "grantley", models: ["FS3017"] },
            { cpuArch: "x86_64", packageArch: "icelaked", models: ["FS3420", "RS1626xs+", "RS3626xs", "RS4826xs+", "RS6426xs+"] },
            { cpuArch: "x86_64", packageArch: "purley", models: ["FS6400", "HD6500"] },
            { cpuArch: "x86_64", packageArch: "r1000", models: ["DS1522+", "DS723+", "DS923+", "RS422+"] },
            { cpuArch: "x86_64", packageArch: "r1000nk", models: ["DS725+", "DS725neo+"] },
            { cpuArch: "x86_64", packageArch: "v1000", models: ["DS1621+", "DS1821+", "DS1823xs+", "DS2422+", "FS2500", "RS1221+", "RS1221RP+", "RS2421+", "RS2421RP+", "RS2423+", "RS2423RP+", "RS2821RP+", "RS822+", "RS822RP+", "RS826+", "RS826RP+"] },
            { cpuArch: "x86_64", packageArch: "v1000nk", models: ["DS1525+", "DS1525neo+", "DS1825+", "DS1825neo+", "DS925+", "DS925neo+", "RS1226+", "RS1226RP+", "RS2423RP+II", "RS2825RP+"] },
            { cpuArch: "x86_64", packageArch: "x86", models: ["DS1010+", "DS1511+", "DS2411+", "DS411+", "DS411+II", "DS710+", "DS712+", "RS2211+", "RS2211RP+", "RS810+", "RS810RP+"] },
        ],
    });
});
