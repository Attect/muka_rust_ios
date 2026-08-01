use rust_ios_protocol::CommandInvocation;

use crate::{CoreError, CoreResult};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WallpaperScreen {
    Lock,
    Home,
    Both,
}

impl WallpaperScreen {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Lock => "lock",
            Self::Home => "home",
            Self::Both => "both",
        }
    }

    pub(crate) fn where_value(self) -> u64 {
        match self {
            Self::Lock => 1,
            Self::Home => 2,
            Self::Both => 3,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SetWallpaperInvocation {
    pub(crate) image_path: String,
    pub(crate) screen: WallpaperScreen,
    pub(crate) p12_path: String,
    pub(crate) password: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PairInvocation {
    Standard {
        host_name: Option<String>,
    },
    Supervised {
        p12_path: String,
        password: Option<String>,
    },
}

pub(crate) fn parse_pair_invocation(invocation: &CommandInvocation) -> CoreResult<PairInvocation> {
    if invocation.argv.first().map(String::as_str) != Some("pair") {
        return Err(CoreError::InvalidArgument(
            "pair parser received a different command".into(),
        ));
    }
    let mut host_name = None;
    let mut p12_path = None;
    let mut password = None;
    let mut password_seen = false;
    let mut index = 1;
    while index < invocation.argv.len() {
        let argument = &invocation.argv[index];
        if !argument.starts_with("--") {
            return Err(CoreError::InvalidArgument(format!(
                "pair does not accept positional argument {argument:?}"
            )));
        }
        let (name, inline_value) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(name, value)| {
                (name, Some(value))
            });
        let value = if let Some(value) = inline_value {
            value.to_owned()
        } else {
            index += 1;
            let value = invocation
                .argv
                .get(index)
                .ok_or_else(|| CoreError::InvalidArgument(format!("{name} requires a value")))?;
            if value.starts_with("--") {
                return Err(CoreError::InvalidArgument(format!(
                    "{name} requires a value, got option {value:?}"
                )));
            }
            value.clone()
        };
        match name {
            "--name" => {
                if host_name.is_some() {
                    return Err(CoreError::InvalidArgument(
                        "pair accepts --name only once".into(),
                    ));
                }
                if value.is_empty() {
                    return Err(CoreError::InvalidArgument(
                        "pair --name cannot be empty".into(),
                    ));
                }
                host_name = Some(value);
            }
            "--p12file" => {
                if p12_path.is_some() {
                    return Err(CoreError::InvalidArgument(
                        "pair accepts --p12file only once".into(),
                    ));
                }
                if value.is_empty() {
                    return Err(CoreError::InvalidArgument(
                        "pair --p12file cannot be empty".into(),
                    ));
                }
                p12_path = Some(value);
            }
            "--password" => {
                if password_seen {
                    return Err(CoreError::InvalidArgument(
                        "pair accepts --password only once".into(),
                    ));
                }
                password_seen = true;
                password = (!value.is_empty()).then_some(value);
            }
            _ => {
                return Err(CoreError::InvalidArgument(format!(
                    "pair does not accept option {name}"
                )));
            }
        }
        index += 1;
    }

    match p12_path {
        Some(p12_path) if host_name.is_none() => {
            Ok(PairInvocation::Supervised { p12_path, password })
        }
        Some(_) => Err(CoreError::InvalidArgument(
            "pair --name cannot be combined with supervised --p12file pairing".into(),
        )),
        None if password_seen => Err(CoreError::InvalidArgument(
            "pair --password requires --p12file".into(),
        )),
        None => Ok(PairInvocation::Standard { host_name }),
    }
}

pub(crate) fn parse_set_wallpaper_invocation(
    invocation: &CommandInvocation,
) -> CoreResult<SetWallpaperInvocation> {
    if invocation.argv.first().map(String::as_str) != Some("set-wallpaper") {
        return Err(CoreError::InvalidArgument(
            "set-wallpaper parser received a different command".into(),
        ));
    }

    let mut image_path = None;
    let mut screen = None;
    let mut p12_path = None;
    let mut password = None;
    let mut password_seen = false;
    let mut index = 1;
    while index < invocation.argv.len() {
        let argument = &invocation.argv[index];
        if argument.starts_with("--") {
            let (name, inline_value) = argument
                .split_once('=')
                .map_or((argument.as_str(), None), |(name, value)| {
                    (name, Some(value))
                });
            let value = if let Some(value) = inline_value {
                value.to_owned()
            } else {
                index += 1;
                let value = invocation.argv.get(index).ok_or_else(|| {
                    CoreError::InvalidArgument(format!("{name} requires a value"))
                })?;
                if value.starts_with("--") {
                    return Err(CoreError::InvalidArgument(format!(
                        "{name} requires a value, got option {value:?}"
                    )));
                }
                value.clone()
            };
            match name {
                "--screen" => {
                    if screen.is_some() {
                        return Err(CoreError::InvalidArgument(
                            "set-wallpaper accepts --screen only once".into(),
                        ));
                    }
                    screen = Some(match value.as_str() {
                        "lock" => WallpaperScreen::Lock,
                        "home" => WallpaperScreen::Home,
                        "both" => WallpaperScreen::Both,
                        _ => {
                            return Err(CoreError::InvalidArgument(format!(
                                "set-wallpaper --screen must be lock, home or both, got {value:?}"
                            )));
                        }
                    });
                }
                "--p12file" => {
                    if p12_path.is_some() {
                        return Err(CoreError::InvalidArgument(
                            "set-wallpaper accepts --p12file only once".into(),
                        ));
                    }
                    if value.is_empty() {
                        return Err(CoreError::InvalidArgument(
                            "set-wallpaper --p12file cannot be empty".into(),
                        ));
                    }
                    p12_path = Some(value);
                }
                "--password" => {
                    if password_seen {
                        return Err(CoreError::InvalidArgument(
                            "set-wallpaper accepts --password only once".into(),
                        ));
                    }
                    password_seen = true;
                    password = (!value.is_empty()).then_some(value);
                }
                _ => {
                    return Err(CoreError::InvalidArgument(format!(
                        "set-wallpaper does not accept option {name}"
                    )));
                }
            }
        } else if image_path.replace(argument.clone()).is_some() {
            return Err(CoreError::InvalidArgument(
                "set-wallpaper requires exactly one image path".into(),
            ));
        }
        index += 1;
    }

    Ok(SetWallpaperInvocation {
        image_path: image_path.ok_or_else(|| {
            CoreError::InvalidArgument("set-wallpaper requires a JPEG or PNG image path".into())
        })?,
        screen: screen.unwrap_or(WallpaperScreen::Home),
        p12_path: p12_path.ok_or_else(|| {
            CoreError::InvalidArgument(
                "set-wallpaper requires --p12file with a supervision identity".into(),
            )
        })?,
        password,
    })
}

pub(crate) fn validate_erase_invocation(invocation: &CommandInvocation) -> CoreResult<&str> {
    let udid = invocation
        .udid
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            CoreError::InvalidArgument(
                "erase requires an explicit --udid; implicit device selection is disabled".into(),
            )
        })?;
    if !invocation.argv.iter().any(|value| value == "--force") {
        return Err(CoreError::InvalidArgument(
            "erase requires --force and will irreversibly remove all apps and settings".into(),
        ));
    }
    if invocation.argv.as_slice() != ["erase", "--force"] {
        return Err(CoreError::InvalidArgument(
            "erase accepts only one --force command-specific argument".into(),
        ));
    }
    Ok(udid)
}

pub(crate) const SETUP_SKIP_KEYS: &[&str] = &[
    "Accessibility",
    "AccessibilityAppearance",
    "ActionButton",
    "AgeAssurance",
    "AgeBasedSafetySettings",
    "Android",
    "Appearance",
    "AppleID",
    "AppStore",
    "Avatar",
    "Biometric",
    "CameraButton",
    "CloudStorage",
    "DeviceProtection",
    "DeviceToDeviceMigration",
    "Diagnostics",
    "Display",
    "EnableLockdownMode",
    "ExpressLanguage",
    "FileVault",
    "iCloudDiagnostics",
    "iCloudStorage",
    "iMessageAndFaceTime",
    "IntendedUser",
    "Intelligence",
    "Keyboard",
    "Language",
    "LanguageAndLocale",
    "Location",
    "LockdownMode",
    "MessagingActivationUsingPhoneNumber",
    "Multitasking",
    "OSShowCase",
    "Passcode",
    "Payment",
    "PreferredLanguage",
    "Privacy",
    "Region",
    "Registration",
    "Restore",
    "RestoreCompleted",
    "Safety",
    "SafetyAndHandling",
    "ScreenSaver",
    "ScreenTime",
    "SIMSetup",
    "Siri",
    "SoftwareUpdate",
    "SpokenLanguage",
    "TapToSetup",
    "TermsOfAddress",
    "Tips",
    "Tone",
    "TOS",
    "TouchID",
    "TrueToneDisplay",
    "TVHomeScreenSync",
    "TVProviderSignIn",
    "TVRoom",
    "UnlockWithWatch",
    "UpdateCompleted",
    "Wallpaper",
    "WatchMigration",
    "WebContentFiltering",
    "Welcome",
    "WiFi",
    "DisplayTone",
    "HomeButtonSensitivity",
    "OnBoarding",
    "Zoom",
];

#[cfg(test)]
mod tests {
    use rust_ios_protocol::OutputOptions;

    use super::*;

    fn invocation(argv: &[&str], udid: Option<&str>) -> CommandInvocation {
        CommandInvocation {
            argv: argv.iter().map(|value| (*value).to_owned()).collect(),
            cwd: None,
            udid: udid.map(str::to_owned),
            output: OutputOptions::default(),
        }
    }

    #[test]
    fn erase_requires_force_explicit_udid_and_no_extra_arguments() {
        assert_eq!(
            validate_erase_invocation(&invocation(&["erase", "--force"], Some("test-phone")))
                .unwrap(),
            "test-phone"
        );
        for invalid in [
            invocation(&["erase"], Some("test-phone")),
            invocation(&["erase", "--force"], None),
            invocation(&["erase", "--force", "--force"], Some("test-phone")),
            invocation(&["erase", "--force", "unexpected"], Some("test-phone")),
        ] {
            assert!(matches!(
                validate_erase_invocation(&invalid),
                Err(CoreError::InvalidArgument(_))
            ));
        }
    }

    #[test]
    fn set_wallpaper_parser_defaults_home_and_accepts_all_screens() {
        let parsed = parse_set_wallpaper_invocation(&invocation(
            &[
                "set-wallpaper",
                "wallpaper.png",
                "--p12file",
                "identity.p12",
            ],
            None,
        ))
        .unwrap();
        assert_eq!(parsed.image_path, "wallpaper.png");
        assert_eq!(parsed.screen, WallpaperScreen::Home);
        assert_eq!(parsed.p12_path, "identity.p12");
        assert_eq!(parsed.password, None);

        for (name, expected, where_value) in [
            ("lock", WallpaperScreen::Lock, 1),
            ("home", WallpaperScreen::Home, 2),
            ("both", WallpaperScreen::Both, 3),
        ] {
            let parsed = parse_set_wallpaper_invocation(&invocation(
                &[
                    "set-wallpaper",
                    "--screen",
                    name,
                    "wallpaper.jpg",
                    "--p12file=identity.p12",
                    "--password=secret",
                ],
                None,
            ))
            .unwrap();
            assert_eq!(parsed.screen, expected);
            assert_eq!(parsed.screen.as_str(), name);
            assert_eq!(parsed.screen.where_value(), where_value);
            assert_eq!(parsed.password.as_deref(), Some("secret"));
        }
    }

    #[test]
    fn set_wallpaper_parser_rejects_missing_repeated_and_unknown_arguments() {
        for argv in [
            vec!["set-wallpaper", "wallpaper.png"],
            vec!["set-wallpaper", "--p12file=identity.p12"],
            vec![
                "set-wallpaper",
                "one.png",
                "two.png",
                "--p12file=identity.p12",
            ],
            vec![
                "set-wallpaper",
                "wallpaper.png",
                "--p12file=one.p12",
                "--p12file=two.p12",
            ],
            vec![
                "set-wallpaper",
                "wallpaper.png",
                "--p12file=identity.p12",
                "--screen=display",
            ],
            vec![
                "set-wallpaper",
                "wallpaper.png",
                "--p12file=identity.p12",
                "--unknown=value",
            ],
        ] {
            assert!(matches!(
                parse_set_wallpaper_invocation(&invocation(&argv, None)),
                Err(CoreError::InvalidArgument(_))
            ));
        }
    }

    #[test]
    fn pair_parser_separates_standard_and_supervised_modes() {
        assert_eq!(
            parse_pair_invocation(&invocation(&["pair"], None)).unwrap(),
            PairInvocation::Standard { host_name: None }
        );
        assert_eq!(
            parse_pair_invocation(&invocation(&["pair", "--name", "lab-host"], None)).unwrap(),
            PairInvocation::Standard {
                host_name: Some("lab-host".into())
            }
        );
        assert_eq!(
            parse_pair_invocation(&invocation(
                &["pair", "--p12file=organization.p12", "--password=secret"],
                None,
            ))
            .unwrap(),
            PairInvocation::Supervised {
                p12_path: "organization.p12".into(),
                password: Some("secret".into())
            }
        );
    }

    #[test]
    fn pair_parser_rejects_ambiguous_and_extra_arguments() {
        for argv in [
            vec!["pair", "unexpected"],
            vec!["pair", "--password=secret"],
            vec!["pair", "--name=host", "--p12file=organization.p12"],
            vec!["pair", "--p12file=one.p12", "--p12file=two.p12"],
            vec!["pair", "--name=one", "--name=two"],
            vec!["pair", "--unknown=value"],
        ] {
            assert!(matches!(
                parse_pair_invocation(&invocation(&argv, None)),
                Err(CoreError::InvalidArgument(_))
            ));
        }
    }
}
