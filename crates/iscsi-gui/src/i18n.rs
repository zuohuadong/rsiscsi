use std::{env, process::Command, sync::OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

static LANG: OnceLock<Lang> = OnceLock::new();

pub fn init() {
    let _ = lang();
}

pub fn lang() -> Lang {
    *LANG.get_or_init(|| detect_with(|key| env::var(key).ok(), apple_languages))
}

pub fn pick(zh: &'static str, en: &'static str) -> &'static str {
    match lang() {
        Lang::Zh => zh,
        Lang::En => en,
    }
}

pub fn locale() -> &'static str {
    pick("zh-CN", "en-US")
}

fn lang_from_tag(tag: &str) -> Lang {
    if tag
        .trim()
        .split(['-', '_', '.', '@'])
        .next()
        .is_some_and(|language| language.eq_ignore_ascii_case("zh"))
    {
        Lang::Zh
    } else {
        Lang::En
    }
}

fn apple_languages() -> Option<String> {
    let output = Command::new("/usr/bin/defaults")
        .args(["read", "-g", "AppleLanguages"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())
        .flatten()
}

fn first_apple_language(languages: &str) -> Option<Lang> {
    let tag = languages
        .trim()
        .strip_prefix('(')?
        .strip_suffix(')')?
        .split(',')
        .next()?
        .trim()
        .trim_matches('"');
    (!tag.is_empty()).then(|| lang_from_tag(tag))
}

fn detect_with(
    mut environment: impl FnMut(&str) -> Option<String>,
    apple: impl FnOnce() -> Option<String>,
) -> Lang {
    environment("RSISCSI_LANG")
        .and_then(|tag| match tag.trim().to_ascii_lowercase().as_str() {
            "zh" => Some(Lang::Zh),
            "en" => Some(Lang::En),
            _ => None,
        })
        .or_else(|| apple().and_then(|languages| first_apple_language(&languages)))
        .or_else(|| {
            ["LANG", "LC_ALL"].into_iter().find_map(|key| {
                environment(key)
                    .filter(|tag| !tag.trim().is_empty())
                    .map(|tag| lang_from_tag(&tag))
            })
        })
        .unwrap_or(Lang::En)
}

macro_rules! tr {
    ($zh:literal, $en:literal $(,)?) => {
        $crate::i18n::pick($zh, $en)
    };
    ($zh:literal, $en:literal, $($arguments:tt)+) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::Zh => format!($zh, $($arguments)+),
            $crate::i18n::Lang::En => format!($en, $($arguments)+),
        }
    };
}

pub(crate) use tr;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_tags() {
        for (tag, expected) in [
            ("zh-Hans-CN", Lang::Zh),
            ("en-US", Lang::En),
            ("fr-FR", Lang::En),
            ("", Lang::En),
            ("zh_TW", Lang::Zh),
            ("zh", Lang::Zh),
            ("zh-Hans", Lang::Zh),
            ("ZH-hant-TW", Lang::Zh),
            (" zh_CN.UTF-8 ", Lang::Zh),
            ("zhang", Lang::En),
        ] {
            assert_eq!(lang_from_tag(tag), expected, "{tag:?}");
        }
    }

    #[test]
    fn apple_languages_uses_only_the_first_tag() {
        for (languages, expected) in [
            ("(\n    \"zh-Hans-CN\",\n    \"en-US\"\n)", Some(Lang::Zh)),
            ("(\"en-US\", \"zh-Hans-CN\")", Some(Lang::En)),
            ("(fr-FR, zh_TW)", Some(Lang::En)),
            ("(zh_TW)", Some(Lang::Zh)),
            ("()", None),
            ("( \n )", None),
            ("", None),
            ("not an array", None),
        ] {
            assert_eq!(first_apple_language(languages), expected, "{languages:?}");
        }
    }

    fn detected(environment: &[(&str, &str)], apple: Option<&str>) -> Lang {
        detect_with(
            |key| {
                environment
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, tag)| (*tag).to_owned())
            },
            || apple.map(str::to_owned),
        )
    }

    #[test]
    fn detection_priority_and_fallbacks() {
        assert_eq!(
            detected(&[("RSISCSI_LANG", "ZH")], Some("(en-US)")),
            Lang::Zh
        );
        assert_eq!(
            detected(&[("RSISCSI_LANG", "eN")], Some("(zh_TW)")),
            Lang::En
        );
        assert_eq!(
            detected(
                &[("RSISCSI_LANG", "fr"), ("LANG", "en-US")],
                Some("(zh_TW)")
            ),
            Lang::Zh
        );
        assert_eq!(
            detected(&[("LANG", "zh_CN")], Some("(fr-FR, zh_TW)")),
            Lang::En
        );
        assert_eq!(detected(&[("LANG", "zh_CN.UTF-8")], None), Lang::Zh);
        assert_eq!(
            detected(&[("LANG", "fr_FR"), ("LC_ALL", "zh_TW")], None),
            Lang::En
        );
        assert_eq!(
            detected(&[("LANG", ""), ("LC_ALL", "zh_TW")], Some("()")),
            Lang::Zh
        );
        assert_eq!(detected(&[("LC_ALL", "zh_TW")], Some("invalid")), Lang::Zh);
        assert_eq!(detected(&[], None), Lang::En);
    }

    #[test]
    fn override_does_not_probe_system_languages() {
        assert_eq!(
            detect_with(
                |key| (key == "RSISCSI_LANG").then(|| "en".to_owned()),
                || panic!("system languages must not be probed with a valid override"),
            ),
            Lang::En
        );
    }

    #[test]
    fn initialization_and_translation_are_consistent() {
        init();
        let selected = lang();
        init();
        assert_eq!(lang(), selected);
        assert_eq!(
            locale(),
            if selected == Lang::Zh {
                "zh-CN"
            } else {
                "en-US"
            }
        );
        let expected = format!("{} 64 MiB", tr!("容量", "Capacity"));
        assert_eq!(tr!("容量 {} MiB", "Capacity {} MiB", 64), expected);
        assert_eq!(
            tr!("容量 {size} MiB", "Capacity {size} MiB", size = 64),
            expected
        );
        assert_eq!(
            pick("zh", "en"),
            if selected == Lang::Zh { "zh" } else { "en" }
        );
    }
}
