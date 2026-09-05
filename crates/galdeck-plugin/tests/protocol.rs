//! The wire format is the contract with other people's programs, so it is
//! pinned here rather than left to whatever serde happens to do.

use std::collections::BTreeMap;

use galdeck_plugin::{decode, encode, FromPlugin, Manifest, ToPlugin};

#[test]
fn messages_are_one_json_object_per_line() {
    // A plugin author reads their side off `cat`, so the shape matters.
    let line = encode(&ToPlugin::Press { key: 3 }).unwrap();
    assert_eq!(line, "{\"type\":\"press\",\"key\":3}\n");
    assert!(line.ends_with('\n'));
    assert_eq!(line.matches('\n').count(), 1);
}

#[test]
fn every_message_round_trips() {
    let mut options = BTreeMap::new();
    options.insert("city".to_string(), "London".to_string());

    let to = [
        ToPlugin::Hello {
            protocol: 1,
            plugin: "counter".into(),
        },
        ToPlugin::Appear { key: 0, options },
        ToPlugin::Disappear { key: 11 },
        ToPlugin::Press { key: 5 },
        ToPlugin::Shutdown,
    ];
    for message in to {
        let line = encode(&message).unwrap();
        assert_eq!(decode::<ToPlugin>(&line).unwrap(), message);
    }

    let from = [
        FromPlugin::Ready {
            name: "Counter".into(),
        },
        FromPlugin::SetText {
            key: 2,
            text: "42".into(),
        },
        FromPlugin::SetColor {
            key: 2,
            color: "@accent".into(),
        },
        FromPlugin::Log {
            message: "hello".into(),
        },
    ];
    for message in from {
        let line = encode(&message).unwrap();
        assert_eq!(decode::<FromPlugin>(&line).unwrap(), message);
    }
}

#[test]
fn a_message_from_a_newer_daemon_is_a_clean_failure_not_a_wrong_one() {
    // The SDK ignores what it cannot read, so a daemon that grows a message
    // does not break every plugin that predates it. What matters is that this
    // is an error rather than something that silently deserializes wrong.
    assert!(decode::<ToPlugin>(r#"{"type":"teleport","key":1}"#).is_err());
}

#[test]
fn appear_carries_the_options_the_config_gave_it() {
    // How one plugin serves several keys that mean different things.
    let line = r#"{"type":"appear","key":6,"options":{"label":"presses"}}"#;
    let ToPlugin::Appear { key, options } = decode::<ToPlugin>(line).unwrap() else {
        panic!("expected appear");
    };
    assert_eq!(key, 6);
    assert_eq!(options.get("label").map(String::as_str), Some("presses"));
}

#[test]
fn a_manifest_is_a_name_and_a_command() {
    let manifest: Manifest = toml::from_str(
        "name = \"Counter\"\ndescription = \"counts\"\ncommand = \"python3 counter.py\"\n",
    )
    .unwrap();
    assert_eq!(manifest.name, "Counter");
    assert_eq!(manifest.command, "python3 counter.py");
}

#[test]
fn a_manifest_with_an_unknown_field_is_refused() {
    // Better a clear error than a setting that silently does nothing.
    assert!(toml::from_str::<Manifest>("name = \"x\"\ncommand = \"y\"\nsandbox = true\n").is_err());
}

#[test]
fn the_shipped_example_plugin_has_a_valid_manifest() {
    let text = include_str!("../../../config/v2/plugins/counter/plugin.toml");
    let manifest: Manifest = toml::from_str(text).expect("the shipped example must parse");
    assert_eq!(manifest.name, "Counter");
}
