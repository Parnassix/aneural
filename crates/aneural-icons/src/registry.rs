//! The curated icon table. Every entry references a real static from an
//! `icondata_*` set crate (or [`crate::drawn`]), so a misspelled name fails to
//! compile.

use crate::drawn;
use icondata_core::Icon;
use std::collections::HashMap;
use std::sync::OnceLock;

macro_rules! icon_table {
    ($($krate:ident :: [$($name:ident),* $(,)?]),* $(,)?) => {
        /// All registered icons as `(name, icon)` pairs.
        pub static ICONS: &[(&str, Icon)] = &[
            $( $( (stringify!($name), $krate::$name), )* )*
        ];
    };
}

icon_table! {
    icondata_bs::[
        // file types: one per extension family, drawn as a labelled page
        BsFiletypeTsx, BsFiletypeJs, BsFiletypeJsx, BsFiletypePy, BsFiletypeJava,
        BsFiletypePhp, BsFiletypeRb, BsFiletypeCs, BsFiletypeMd, BsFiletypeMdx,
        BsFiletypeJson, BsFiletypeYml, BsFiletypeXml, BsFiletypeHtml,
        BsFiletypeCss, BsFiletypeScss, BsFiletypeSass, BsFiletypeSh,
        BsFiletypeSql, BsFiletypeCsv, BsFiletypeTxt, BsFiletypePdf,
        BsFiletypePng, BsFiletypeJpg, BsFiletypeGif, BsFiletypeSvg,
        BsFiletypeBmp, BsFiletypeHeic, BsFiletypeTiff, BsFiletypeRaw,
        BsFiletypeAi, BsFiletypePsd, BsFiletypeExe, BsFiletypeKey,
        BsFiletypeTtf, BsFiletypeOtf, BsFiletypeWoff,
        BsFiletypeMp3, BsFiletypeWav, BsFiletypeAac, BsFiletypeM4p,
        BsFiletypeMp4, BsFiletypeMov,
        BsFiletypeDoc, BsFiletypeDocx, BsFiletypeXls, BsFiletypeXlsx,
        BsFiletypePpt, BsFiletypePptx,
    ],
    drawn::[
        // the file types Bootstrap has no page for, built from its letters
        AnFiletypeTs, AnFiletypeRs, AnFiletypeGo, AnFiletypeToml,
    ],
    icondata_lu::[
        // structure
        LuFolder, LuFolderOpen, LuFile, LuFileCode, LuFileText, LuFileJson,
        LuPackage, LuBox, LuLayers, LuGitBranch, LuNetwork, LuDatabase, LuGlobe,
        // spores / annotations
        LuMessageSquare, LuMap, LuLightbulb, LuStickyNote, LuBookOpen, LuBug,
        LuWrench, LuFlame, LuScale, LuTag, LuShield, LuHash, LuLink, LuImage,
        LuGauge, LuCalendarClock, LuHistory,
        // brand / ui
        LuLeaf, LuSprout, LuPuzzle, LuCircleDot, LuSearch, LuSettings, LuPin,
        LuEye, LuEyeOff, LuTerminal, LuCheck, LuX, LuFunnel, LuListFilter,
        LuSlidersHorizontal,
    ],
    icondata_si::[
        SiTypescript, SiJavascript, SiPython, SiRust, SiGo, SiOpenjdk, SiPhp,
        SiRuby, SiMarkdown, SiNpm, SiYaml, SiToml, SiJson, SiHtml5, SiCss,
        SiReact, SiDocker, SiGit, SiGnubash, SiSqlite, SiSvg, SiGradle,
        SiApachemaven, SiComposer, SiRubygems, SiPypi, SiGooglecloud,
    ],
    icondata_vs::[
        VsRepo, VsPackage, VsJson, VsSymbolMethod, VsSymbolClass, VsComment,
        VsChecklist, VsFolderLibrary, VsLock, VsSettingsGear, VsSourceControl,
    ],
}

fn index() -> &'static HashMap<&'static str, Icon> {
    static INDEX: OnceLock<HashMap<&'static str, Icon>> = OnceLock::new();
    INDEX.get_or_init(|| ICONS.iter().copied().collect())
}

/// Look an icon up by its icondata static name, e.g. `"LuFolder"`.
pub fn lookup(name: &str) -> Option<Icon> {
    index().get(name).copied()
}

pub fn is_valid(name: &str) -> bool {
    index().contains_key(name)
}

/// All registered icon names, in table order.
pub fn names() -> impl Iterator<Item = &'static str> {
    ICONS.iter().map(|(n, _)| *n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_name_resolves_and_is_unique() {
        let mut seen = std::collections::HashSet::new();
        for (name, icon) in ICONS {
            assert!(seen.insert(*name), "duplicate icon name {name}");
            assert!(std::ptr::eq(lookup(name).unwrap(), *icon), "{name}");
            assert!(!icon.data.is_empty(), "{name} has no path data");
        }
        assert_eq!(names().count(), ICONS.len());
        assert!(!is_valid("LuDoesNotExist"));
    }

    #[test]
    fn names_used_by_aneural_core_builtins_exist() {
        for name in [
            "LuFolder",
            "LuFile",
            "VsRepo",
            "VsPackage",
            "LuPackage",
            "VsSymbolMethod",
            "LuCircleDot",
            "LuMessageSquare",
            "LuMap",
            "LuLightbulb",
            "LuStickyNote",
            "LuPuzzle",
            "LuSprout",
            "VsSourceControl",
            "LuGauge",
            "LuTerminal",
            "LuCalendarClock",
            "LuHistory",
        ] {
            assert!(
                is_valid(name),
                "core builtin icon {name} missing from registry"
            );
        }
    }
}
