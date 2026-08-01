use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, anyhow, bail};
use plist::{Dictionary, Value};
use rust_ios_protocol::{XCTestEnvironmentValue, XCTestTargetSpec};

pub fn parse_xctestrun(path: &Path) -> anyhow::Result<Vec<XCTestTargetSpec>> {
    let root = Value::from_file(path)
        .with_context(|| format!("failed to read .xctestrun {}", path.display()))?;
    parse_root(&root).with_context(|| format!("invalid .xctestrun {}", path.display()))
}

fn parse_root(root: &Value) -> anyhow::Result<Vec<XCTestTargetSpec>> {
    let root = root
        .as_dictionary()
        .ok_or_else(|| anyhow!("root must be a plist dictionary"))?;
    let version = root
        .get("__xctestrun_metadata__")
        .and_then(Value::as_dictionary)
        .and_then(|metadata| metadata.get("FormatVersion"))
        .and_then(|value| {
            value.as_unsigned_integer().or_else(|| {
                value
                    .as_signed_integer()
                    .and_then(|value| value.try_into().ok())
            })
        })
        .ok_or_else(|| anyhow!("__xctestrun_metadata__.FormatVersion is missing or invalid"))?;

    let targets = match version {
        1 => parse_v1(root)?,
        2 => parse_v2(root)?,
        version => bail!(".xctestrun format version {version} is not supported"),
    };
    if targets.is_empty() {
        bail!(".xctestrun does not contain any test targets");
    }
    Ok(targets)
}

fn parse_v1(root: &Dictionary) -> anyhow::Result<Vec<XCTestTargetSpec>> {
    let mut targets = Vec::new();
    for (name, value) in root {
        if name == "__xctestrun_metadata__" {
            continue;
        }
        let Some(target) = value.as_dictionary() else {
            continue;
        };
        targets.push(parse_target(target, name, 0)?);
    }
    Ok(targets)
}

fn parse_v2(root: &Dictionary) -> anyhow::Result<Vec<XCTestTargetSpec>> {
    let configurations = root
        .get("TestConfigurations")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("TestConfigurations is missing or is not an array"))?;
    let mut targets = Vec::new();
    for (configuration_index, configuration) in configurations.iter().enumerate() {
        let configuration = configuration.as_dictionary().ok_or_else(|| {
            anyhow!("TestConfigurations[{configuration_index}] must be a dictionary")
        })?;
        let configuration_name = optional_string(configuration, "Name")
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| format!("configuration-{}", configuration_index + 1));
        let test_targets = configuration
            .get("TestTargets")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                anyhow!("TestConfigurations[{configuration_index}].TestTargets must be an array")
            })?;
        for (target_index, target) in test_targets.iter().enumerate() {
            let target = target.as_dictionary().ok_or_else(|| {
                anyhow!(
                    "TestConfigurations[{configuration_index}].TestTargets[{target_index}] must be a dictionary"
                )
            })?;
            targets.push(parse_target(target, &configuration_name, target_index)?);
        }
    }
    Ok(targets)
}

fn parse_target(
    target: &Dictionary,
    configuration_name: &str,
    target_index: usize,
) -> anyhow::Result<XCTestTargetSpec> {
    let runner_bundle_id = required_string(target, "TestHostBundleIdentifier")?;
    let xctest_config = optional_string(target, "TestBundlePath")
        .and_then(|path| file_name(&path))
        .filter(|name| !name.is_empty());
    let display_name = xctest_config
        .as_deref()
        .and_then(|name| name.strip_suffix(".xctest"))
        .filter(|name| !name.is_empty())
        .unwrap_or(&runner_bundle_id);
    let name = if configuration_name == display_name {
        display_name.to_owned()
    } else {
        format!("{configuration_name}/{display_name}")
    };
    let is_ui_test = target
        .get("IsUITestBundle")
        .and_then(Value::as_boolean)
        .unwrap_or(false);
    let target_app_name = optional_string(target, "UITargetAppPath")
        .and_then(|path| file_name(&path))
        .and_then(|name| name.strip_suffix(".app").map(str::to_owned))
        .filter(|name| !name.is_empty());
    let environment = if is_ui_test {
        merge_environments(
            target,
            &[
                "EnvironmentVariables",
                "TestingEnvironmentVariables",
                "UITargetAppEnvironmentVariables",
            ],
        )?
    } else {
        BTreeMap::new()
    };
    let arguments = string_array(target, "CommandLineArguments")?;
    let tests_to_run = string_array(target, "OnlyTestIdentifiers")?;
    let tests_to_skip = string_array(target, "SkipTestIdentifiers")?;
    let name = if name.trim().is_empty() {
        format!("target-{}", target_index + 1)
    } else {
        name
    };

    Ok(XCTestTargetSpec {
        name,
        runner_bundle_id,
        target_bundle_id: None,
        target_app_name,
        xctest_config,
        arguments,
        environment,
        tests_to_run,
        tests_to_skip,
        is_ui_test,
    })
}

fn merge_environments(
    target: &Dictionary,
    keys: &[&str],
) -> anyhow::Result<BTreeMap<String, XCTestEnvironmentValue>> {
    let mut environment = BTreeMap::new();
    for key in keys {
        let Some(value) = target.get(key) else {
            continue;
        };
        let dictionary = value
            .as_dictionary()
            .ok_or_else(|| anyhow!("{key} must be a dictionary"))?;
        for (name, value) in dictionary {
            environment.insert(
                name.clone(),
                environment_value(value).with_context(|| {
                    format!("environment variable {name:?} in {key} has unsupported value")
                })?,
            );
        }
    }
    Ok(environment)
}

fn environment_value(value: &Value) -> anyhow::Result<XCTestEnvironmentValue> {
    if let Some(value) = value.as_string() {
        Ok(XCTestEnvironmentValue::String(value.into()))
    } else if let Some(value) = value.as_boolean() {
        Ok(XCTestEnvironmentValue::Boolean(value))
    } else if let Some(value) = value.as_signed_integer() {
        Ok(XCTestEnvironmentValue::SignedInteger(value))
    } else if let Some(value) = value.as_unsigned_integer() {
        Ok(XCTestEnvironmentValue::UnsignedInteger(value))
    } else if let Some(value) = value.as_real() {
        Ok(XCTestEnvironmentValue::Real(value))
    } else {
        bail!("expected string, bool, integer or real")
    }
}

fn required_string(dictionary: &Dictionary, key: &str) -> anyhow::Result<String> {
    optional_string(dictionary, key)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("{key} is missing or empty"))
}

fn optional_string(dictionary: &Dictionary, key: &str) -> Option<String> {
    dictionary
        .get(key)
        .and_then(Value::as_string)
        .map(str::to_owned)
}

fn string_array(dictionary: &Dictionary, key: &str) -> anyhow::Result<Vec<String>> {
    let Some(value) = dictionary.get(key) else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .ok_or_else(|| anyhow!("{key} must be an array"))?;
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value
                .as_string()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("{key}[{index}] must be a string"))
        })
        .collect()
}

fn file_name(path: &str) -> Option<String> {
    path.rsplit(['/', '\\'])
        .find(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use plist::{Dictionary, Integer, Value};

    use super::*;

    fn dictionary<const N: usize>(entries: [(&str, Value); N]) -> Dictionary {
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect()
    }

    fn metadata(version: i64) -> Value {
        Value::Dictionary(dictionary([(
            "FormatVersion",
            Value::Integer(Integer::from(version)),
        )]))
    }

    fn ui_target(runner: &str, config: &str) -> Value {
        Value::Dictionary(dictionary([
            ("TestHostBundleIdentifier", Value::String(runner.into())),
            ("TestBundlePath", Value::String(config.into())),
            ("IsUITestBundle", Value::Boolean(true)),
            (
                "UITargetAppPath",
                Value::String("__TESTROOT__/Debug-iphoneos/Demo.app".into()),
            ),
            (
                "EnvironmentVariables",
                Value::Dictionary(dictionary([("STRING", Value::String("value".into()))])),
            ),
            (
                "TestingEnvironmentVariables",
                Value::Dictionary(dictionary([("BOOL", Value::Boolean(true))])),
            ),
            (
                "OnlyTestIdentifiers",
                Value::Array(vec![Value::String("DemoTests/testOne".into())]),
            ),
        ]))
    }

    #[test]
    fn parses_v1_dynamic_target() {
        let root = Value::Dictionary(dictionary([
            ("__xctestrun_metadata__", metadata(1)),
            (
                "DemoAction",
                ui_target(
                    "com.example.DemoUITests.xctrunner",
                    "__TESTHOST__/PlugIns/DemoUITests.xctest",
                ),
            ),
        ]));
        let targets = parse_root(&root).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].name, "DemoAction/DemoUITests");
        assert_eq!(targets[0].target_app_name.as_deref(), Some("Demo"));
        assert_eq!(targets[0].tests_to_run, ["DemoTests/testOne"]);
        assert_eq!(
            targets[0].environment["BOOL"],
            XCTestEnvironmentValue::Boolean(true)
        );
    }

    #[test]
    fn parses_binary_v2_with_multiple_targets() {
        let root = Value::Dictionary(dictionary([
            ("__xctestrun_metadata__", metadata(2)),
            (
                "TestConfigurations",
                Value::Array(vec![Value::Dictionary(dictionary([
                    ("Name", Value::String("DemoScheme".into())),
                    (
                        "TestTargets",
                        Value::Array(vec![
                            ui_target(
                                "com.example.First.xctrunner",
                                "__TESTHOST__/PlugIns/First.xctest",
                            ),
                            ui_target(
                                "com.example.Second.xctrunner",
                                "__TESTHOST__/PlugIns/Second.xctest",
                            ),
                        ]),
                    ),
                ]))]),
            ),
        ]));
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("Demo.xctestrun");
        root.to_file_binary(&path).unwrap();
        let targets = parse_xctestrun(&path).unwrap();
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].name, "DemoScheme/First");
        assert_eq!(targets[1].name, "DemoScheme/Second");
    }

    #[test]
    fn rejects_unknown_format_and_complex_environment() {
        let root = Value::Dictionary(dictionary([("__xctestrun_metadata__", metadata(3))]));
        assert!(
            parse_root(&root)
                .unwrap_err()
                .to_string()
                .contains("version 3")
        );

        let error = environment_value(&Value::Array(Vec::new())).unwrap_err();
        assert!(error.to_string().contains("expected string"));
    }
}
