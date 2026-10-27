/*
 * Copyright 2026 Roberto Leinardi.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use super::*;

fn list(patterns: &[&str]) -> Option<Vec<String>> {
    (!patterns.is_empty()).then(|| patterns.iter().map(|&pattern| pattern.to_owned()).collect())
}

fn global(mode: Mode, allow: &[&str], deny: &[&str]) -> Global {
    Global {
        mode,
        device_allow: list(allow),
        device_deny: list(deny),
    }
}

fn container(enable: Option<bool>, allow: &[&str], deny: &[&str]) -> Container {
    Container {
        enable,
        device_allow: list(allow),
        device_deny: list(deny),
    }
}

fn labels(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|&(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|&value| value.to_owned()).collect()
}

/// The label keys are the prefix plus their names.
#[test]
fn label_keys_share_the_prefix() {
    for (label, name) in [
        (LABEL_ENABLE, "enable"),
        (LABEL_DEVICE_ALLOW, "device-allow"),
        (LABEL_DEVICE_DENY, "device-deny"),
    ] {
        assert_eq!(label, format!("{LABEL_PREFIX}{name}"));
    }
}

/// Go: `TestEnabled`. Go's empty and unknown modes, which enable nothing, cannot be made here:
/// `global_rejects_an_empty_or_unknown_mode` covers them.
#[test]
fn enabled() {
    let cases = [
        ("opt-in unlabelled skipped", Mode::OptIn, None, false),
        (
            "opt-in enable=true processed",
            Mode::OptIn,
            Some(true),
            true,
        ),
        (
            "opt-in enable=false skipped",
            Mode::OptIn,
            Some(false),
            false,
        ),
        ("all unlabelled processed", Mode::All, None, true),
        ("all enable=true processed", Mode::All, Some(true), true),
        ("all enable=false skipped", Mode::All, Some(false), false),
    ];
    for (name, mode, enable, want) in cases {
        let got = global(mode, &[], &[]).enabled(&container(enable, &[], &[]));
        assert_eq!(got, want, "{name}");
    }
}

/// The trust-boundary gap of Go's `TestEnabled` and `TestGlobalValidate`: an empty and an unknown
/// mode are refused, so no policy with either can enable a container.
#[test]
fn global_rejects_an_empty_or_unknown_mode() {
    for mode in ["", "invalid", "ALL", " all"] {
        assert_eq!(
            Global::new(mode, None, None),
            Err(PolicyError::UnknownMode {
                mode: mode.to_owned()
            }),
            "{mode:?}"
        );
    }
}

/// Go: `TestDeviceAllowed`
#[test]
fn device_allowed() {
    type Case<'case> = (
        &'case str,
        &'case [&'case str],
        &'case [&'case str],
        &'case [&'case str],
        &'case [&'case str],
        &'case str,
        bool,
    );
    let cases: [Case<'_>; 8] = [
        ("no restrictions", &[], &[], &[], &[], "/dev/nvidia0", true),
        (
            "global allow matches",
            &["/dev/nvidia*"],
            &[],
            &[],
            &[],
            "/dev/nvidia0",
            true,
        ),
        (
            "global allow no match",
            &["/dev/nvidia*"],
            &[],
            &[],
            &[],
            "/dev/sda",
            false,
        ),
        (
            "global deny beats container allow",
            &[],
            &["/dev/dri/*"],
            &["/dev/dri/*"],
            &[],
            "/dev/dri/renderD128",
            false,
        ),
        (
            "container deny beats global allow",
            &["/dev/dri/*"],
            &[],
            &[],
            &["/dev/dri/renderD129"],
            "/dev/dri/renderD129",
            false,
        ),
        (
            "container allow narrows global allow, path in both",
            &["/dev/dri/*"],
            &[],
            &["/dev/dri/card0"],
            &[],
            "/dev/dri/card0",
            true,
        ),
        (
            "container allow narrows global allow, path not in container allow",
            &["/dev/dri/*"],
            &[],
            &["/dev/dri/card0"],
            &[],
            "/dev/dri/renderD128",
            false,
        ),
        (
            "global deny wins over global allow",
            &["/dev/*"],
            &["/dev/null"],
            &[],
            &[],
            "/dev/null",
            false,
        ),
    ];
    for (name, global_allow, global_deny, container_allow, container_deny, path, want) in cases {
        let got = global(Mode::All, global_allow, global_deny)
            .device_allowed(&container(None, container_allow, container_deny), path);
        assert_eq!(got, want, "{name}");
    }
}

/// Go: `TestDeniedAuthorized`
#[test]
fn denied_authorized() {
    let policy = global(Mode::All, &["/dev/dri/*"], &["/dev/disk/by-id/*"]);
    let labelled = container(None, &[], &["/dev/dri/card1"]);
    for (path, denied, authorized) in [
        ("/dev/dri/card0", false, true),
        ("/dev/dri/card1", true, true),
        ("/dev/disk/by-id/usb-x", true, false),
        ("/dev/sda", false, false),
    ] {
        assert_eq!(policy.denied(&labelled, path), denied, "denied {path}");
        assert_eq!(
            policy.authorized(&labelled, path),
            authorized,
            "authorized {path}"
        );
    }
    assert!(
        global(Mode::All, &[], &[]).authorized(&Container::default(), "/dev/anything"),
        "empty allow lists must authorize everything"
    );
}

/// Go: `TestExplicitlyAllowed`
#[test]
fn explicitly_allowed() {
    type Case<'case> = (
        &'case str,
        &'case [&'case str],
        &'case [&'case str],
        &'case [&'case str],
        &'case [&'case str],
        &'case str,
        bool,
    );
    let cases: [Case<'_>; 9] = [
        ("no allow globs", &[], &[], &[], &[], "/dev/log", false),
        (
            "no allow globs with deny",
            &[],
            &["/dev/sda"],
            &[],
            &[],
            "/dev/log",
            false,
        ),
        (
            "global allow matches",
            &["/dev/dri/*"],
            &[],
            &[],
            &[],
            "/dev/dri/card0",
            true,
        ),
        (
            "global allow no match",
            &["/dev/dri/*"],
            &[],
            &[],
            &[],
            "/dev/log",
            false,
        ),
        (
            "container allow matches",
            &[],
            &[],
            &["/dev/ttyUSB*"],
            &[],
            "/dev/ttyUSB0",
            true,
        ),
        (
            "container allow no match",
            &[],
            &[],
            &["/dev/ttyUSB*"],
            &[],
            "/dev/log",
            false,
        ),
        (
            "global deny beats allow",
            &["/dev/*"],
            &["/dev/log"],
            &[],
            &[],
            "/dev/log",
            false,
        ),
        (
            "container deny beats allow",
            &["/dev/*"],
            &[],
            &[],
            &["/dev/log"],
            "/dev/log",
            false,
        ),
        (
            "container allow narrows global allow",
            &["/dev/dri/*"],
            &[],
            &["/dev/dri/card0"],
            &[],
            "/dev/dri/renderD128",
            false,
        ),
    ];
    for (name, global_allow, global_deny, container_allow, container_deny, path, want) in cases {
        let got = global(Mode::All, global_allow, global_deny)
            .explicitly_allowed(&container(None, container_allow, container_deny), path);
        assert_eq!(got, want, "{name}");
    }
}

/// Go: `TestParseContainer`
#[test]
fn parse_container() {
    type Case<'case> = (&'case str, Vec<(&'case str, &'case str)>, Option<Container>);
    let cases: Vec<Case<'_>> = vec![
        ("nil labels", vec![], Some(container(None, &[], &[]))),
        (
            "no policy labels",
            vec![("other", "value")],
            Some(container(None, &[], &[])),
        ),
        (
            "enable=true",
            vec![(LABEL_ENABLE, "true")],
            Some(container(Some(true), &[], &[])),
        ),
        (
            "enable=false",
            vec![(LABEL_ENABLE, "false")],
            Some(container(Some(false), &[], &[])),
        ),
        (
            "enable=1 (truthy)",
            vec![(LABEL_ENABLE, "1")],
            Some(container(Some(true), &[], &[])),
        ),
        (
            "enable=invalid fails closed",
            vec![(LABEL_ENABLE, "yes")],
            None,
        ),
        (
            "device-allow single",
            vec![(LABEL_DEVICE_ALLOW, "/dev/nvidia*")],
            Some(container(None, &["/dev/nvidia*"], &[])),
        ),
        (
            "device-allow comma-separated",
            vec![(LABEL_DEVICE_ALLOW, "/dev/dri/*, /dev/null")],
            Some(container(None, &["/dev/dri/*", "/dev/null"], &[])),
        ),
        (
            "device-deny set",
            vec![(LABEL_DEVICE_DENY, "/dev/dri/renderD129")],
            Some(container(None, &[], &["/dev/dri/renderD129"])),
        ),
        (
            "invalid glob in device-allow fails closed",
            vec![(LABEL_DEVICE_ALLOW, "/dev/nvidia[")],
            None,
        ),
        (
            "invalid glob in device-deny fails closed",
            vec![(LABEL_DEVICE_DENY, "/dev/bad[")],
            None,
        ),
        (
            "relative glob in device-deny fails closed",
            vec![(LABEL_DEVICE_DENY, "/dev/null, sda")],
            None,
        ),
        (
            "uncleaned glob in device-allow fails closed",
            vec![(LABEL_DEVICE_ALLOW, "/dev/dri/../sda")],
            None,
        ),
    ];
    for (name, pairs, want) in cases {
        let got = Container::parse(&labels(&pairs));
        match want {
            Some(want) => assert_eq!(got, Ok(want), "{name}"),
            None => assert!(got.is_err(), "{name}: {got:?}"),
        }
    }
}

/// Go: `TestValidateGlobs`
#[test]
fn validate_globs_cases() {
    let cases: [(&[&str], bool); 15] = [
        (&[], false),
        (&["/dev/nvidia*", "/dev/dri/*"], false),
        (&["/dev/nvidia["], true),
        (&["/dev/valid", "/dev/bad["], true),
        (&["/dev/bus/usb/*/*"], false),
        // Relative, uncleaned and outside-/dev patterns never match a device path; for a deny
        // that would fail open.
        (&["sda"], true),
        (&["dev/sda"], true),
        (&["*"], true),
        (&["/dev//sda"], true),
        (&["/dev/dri/"], true),
        (&["/dev/./sda"], true),
        (&["/dev/../etc/*"], true),
        (&["/etc/*"], true),
        (&["/dev"], true),
        (&["/devices/*"], true),
    ];
    for (patterns, want_error) in cases {
        let got = validate_globs(&strings(patterns));
        assert_eq!(got.is_err(), want_error, "{patterns:?}: {got:?}");
    }
}

/// Go: `TestGlobalValidate`
#[test]
fn global_validate() {
    type Case<'case> = (
        &'case str,
        &'case str,
        &'case [&'case str],
        &'case [&'case str],
        bool,
    );
    let cases: [Case<'_>; 7] = [
        ("valid opt-in", "opt-in", &[], &[], false),
        ("valid all", "all", &[], &[], false),
        ("invalid mode", "invalid", &[], &[], true),
        ("bad allow glob", "all", &["/dev/bad["], &[], true),
        ("bad deny glob", "all", &[], &["/dev/bad["], true),
        ("relative deny glob", "all", &[], &["sd*"], true),
        ("allow glob outside /dev", "all", &["/sys/*"], &[], true),
    ];
    for (name, mode, allow, deny, want_error) in cases {
        let got = Global::new(mode, list(allow), list(deny));
        assert_eq!(got.is_err(), want_error, "{name}: {got:?}");
    }
}

/// Go: `TestUnknownLabels`; its nil-map case is the empty map here.
#[test]
fn unknown_labels_cases() {
    type Case<'case> = (&'case str, Vec<(&'case str, &'case str)>, Vec<&'case str>);
    let enabled = format!("{LABEL_PREFIX}enabled");
    let zzz = format!("{LABEL_PREFIX}zzz");
    let aaa = format!("{LABEL_PREFIX}aaa");
    let mmm = format!("{LABEL_PREFIX}mmm");
    let cases: [Case<'_>; 5] = [
        ("empty map", vec![], vec![]),
        (
            "no swarm-device-access keys",
            vec![("traefik.enable", "true")],
            vec![],
        ),
        (
            "only known keys",
            vec![
                (LABEL_ENABLE, "true"),
                (LABEL_DEVICE_ALLOW, "/dev/snd/*"),
                (LABEL_DEVICE_DENY, "/dev/sda"),
            ],
            vec![],
        ),
        (
            "mixed known and unknown",
            vec![(LABEL_ENABLE, "true"), (&enabled, "true"), (&zzz, "x")],
            vec![&enabled, &zzz],
        ),
        (
            "unknown sorted deterministically",
            vec![(&zzz, "1"), (&aaa, "2"), (&mmm, "3")],
            vec![&aaa, &mmm, &zzz],
        ),
    ];
    for (name, pairs, want) in cases {
        assert_eq!(unknown_labels(&labels(&pairs)), strings(&want), "{name}");
    }
}

/// The error texts are Go's, `%w` chains included.
#[test]
fn errors_read_as_go() {
    assert_eq!(
        Global::new("bogus", None, None).unwrap_err().to_string(),
        r#"unknown policy mode "bogus": must be "opt-in" or "all""#
    );
    assert_eq!(
        Global::new("all", Some(strings(&["/dev/["])), None)
            .unwrap_err()
            .to_string(),
        r#"device-allow: invalid glob pattern "/dev/[": syntax error in pattern"#
    );
    assert_eq!(
        Global::new("all", None, Some(strings(&["/dev/x/"])))
            .unwrap_err()
            .to_string(),
        r#"device-deny: glob pattern is not a clean path: "/dev/x/""#
    );
    assert_eq!(
        Container::parse(&labels(&[(LABEL_ENABLE, "maybe")]))
            .unwrap_err()
            .to_string(),
        r#"invalid value for label "swarm-device-access.enable": "maybe" is not a boolean"#
    );
    assert_eq!(
        Container::parse(&labels(&[(LABEL_DEVICE_DENY, "/etc")]))
            .unwrap_err()
            .to_string(),
        r#"invalid glob in label "swarm-device-access.device-deny": glob pattern must start with /dev/: "/etc""#
    );
}
