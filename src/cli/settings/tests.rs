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

//! The tests of `config_test.go`: the merge, and the reload against a real store.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use super::*;
use crate::config::{Store, new_store};
use crate::logger::{Level, MemorySink};
use crate::policy::Mode;
use crate::test_support::capture_logs;

/// The flag defaults, as [`Settings::from_flags`] returns them without any flag.
fn default_settings() -> Settings {
    Settings {
        log_format: "text".to_owned(),
        log_level: "info".to_owned(),
        docker_socket: "/var/run/docker.sock".to_owned(),
        policy_mode: "opt-in".to_owned(),
        ..Settings::default()
    }
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

/// Go: `TestMergeSettings_Precedence`.
#[test]
fn merge_settings_precedence() {
    let file = FileSchema {
        policy_mode: "all".to_owned(),
        log_level: "debug".to_owned(),
        device_deny: Some(strings(&["/dev/sda"])),
        ..FileSchema::default()
    };
    let mut cli = default_settings();
    cli.log_level = "warn".to_owned();
    let merged = Settings::merge(&cli, &set(&["log-level"]), &file);
    assert_eq!(merged.log_level, "warn", "the command line's");
    assert_eq!(merged.policy_mode, "all");
    assert_eq!(merged.device_deny, Some(strings(&["/dev/sda"])));
    assert_eq!(merged.log_format, "text");
    assert_eq!(merged.docker_socket, "/var/run/docker.sock");
}

/// Go: `TestMergeSettings_DoesNotAlias`. Rust's ownership makes the aliasing impossible; what is
/// left to check is that the inputs are unchanged.
#[test]
fn merge_settings_does_not_alias() {
    let mut defaults = default_settings();
    defaults.device_allow = Some(strings(&["/dev/null"]));
    let file = FileSchema {
        device_deny: Some(strings(&["/dev/sda"])),
        ..FileSchema::default()
    };
    let merged = Settings::merge(&defaults, &BTreeSet::new(), &file);
    assert_eq!(defaults.device_allow, Some(strings(&["/dev/null"])));
    assert_eq!(merged.device_deny, Some(strings(&["/dev/sda"])));
    assert_eq!(file.device_deny, Some(strings(&["/dev/sda"])));
}

/// An explicit empty list in the file overrides the flag; an absent one does not.
#[test]
fn an_explicit_empty_list_overrides() {
    let mut cli = default_settings();
    cli.device_allow = Some(strings(&["/dev/null"]));
    let empty = FileSchema {
        device_allow: Some(Vec::new()),
        ..FileSchema::default()
    };
    assert_eq!(
        Settings::merge(&cli, &BTreeSet::new(), &empty).device_allow,
        Some(Vec::new())
    );
    assert_eq!(
        Settings::merge(&cli, &BTreeSet::new(), &FileSchema::default()).device_allow,
        Some(strings(&["/dev/null"]))
    );
}

/// Every failed check is reported, joined as `errors.Join` joins them, Q2's refusal included.
#[test]
fn validate_joins_every_failure() {
    let mut settings = default_settings();
    settings.log_format = "xml".to_owned();
    settings.policy_mode = "bogus".to_owned();
    settings.debug_addr = ":6060".to_owned();
    assert_eq!(
        settings.validate().unwrap_err().to_string(),
        "setting \"log-format\": unknown value \"xml\", must be one of [text json plain]\n\
         unknown policy mode \"bogus\": must be \"opt-in\" or \"all\"\n\
         debug-addr \":6060\": the pprof debug server was removed in 2.0; leave debug-addr empty"
    );
    let runtime = default_settings().validate().unwrap();
    assert!(!runtime.dry_run);
    assert_eq!(runtime.policy.mode(), Mode::OptIn);
}

/// A reloader wired to a real store, with the logger configuration recorded instead of applied
/// and its lines captured: Go's `reloadEnv`.
struct ReloadEnv {
    reloader: Reloader,
    store: Arc<Store>,
    path: PathBuf,
    logger_config: Arc<Mutex<Vec<String>>>,
    logs: MemorySink,
}

impl ReloadEnv {
    fn new(flags: &Settings, cli_set: &[&str], initial: &str) -> Self {
        static COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sda-reload-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        std::fs::write(&path, initial).unwrap();
        let file = config::load_file(path.to_str().unwrap()).unwrap();
        let cli_set = set(cli_set);
        let current = Settings::merge(flags, &cli_set, &file);
        let (store, publisher) = new_store(current.validate().unwrap());
        let publisher = Arc::new(publisher);
        let logger_config = Arc::new(Mutex::new(Vec::new()));
        let (log, logs) = capture_logs(Level::INFO);
        let recorded = Arc::clone(&logger_config);
        let reloader = Reloader {
            path: path.to_str().unwrap().to_owned(),
            flags: flags.clone(),
            cli_set,
            current,
            configure_logger: Box::new(move |format, level, _| {
                recorded
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(format!("{format}/{level}"));
            }),
            publish_and_reconcile: Box::new(move |runtime| {
                let generation = publisher.publish(runtime);
                Box::pin(async move { generation })
            }),
            log,
        };
        Self {
            reloader,
            store,
            path,
            logger_config,
            logs,
        }
    }

    async fn reload(&mut self, content: &str) -> Result<(), ReloadError> {
        std::fs::write(&self.path, content).unwrap();
        self.reloader.reload().await
    }

    fn logger_config(&self) -> Vec<String> {
        self.logger_config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Go: `TestReload_RemovedKeyFallsBackToFlagOrDefault`.
#[tokio::test]
async fn reload_removed_key_falls_back_to_flag_or_default() {
    let mut cli = default_settings();
    cli.device_deny = Some(strings(&["/dev/sdb"]));
    let mut env = ReloadEnv::new(&cli, &["device-deny"], "policy-mode: all\n");
    assert_eq!(env.store.load().policy.mode(), Mode::All);
    env.reload("log-level: info\n").await.unwrap();
    let policy = env.store.load().policy.clone();
    assert_eq!(policy.mode(), Mode::OptIn);
    assert_eq!(policy.device_deny(), Some(&["/dev/sdb".to_owned()][..]));
}

/// Go: `TestReload_InvalidKeepsStoreAndLogger`, with Q2's `debug-addr` added.
#[tokio::test]
async fn reload_invalid_keeps_store_and_logger() {
    let mut env = ReloadEnv::new(&default_settings(), &[], "policy-mode: all\n");
    let (before, before_generation) = env.store.snapshot();
    for (name, content) in [
        ("unparsable file", "policy-mode: [\n"),
        (
            "invalid glob",
            "log-level: debug\ndevice-allow: [\"/dev/[\"]\n",
        ),
        ("unknown value", "log-level: loud\n"),
        ("debug server (Q2)", "debug-addr: \":6060\"\n"),
    ] {
        assert!(env.reload(content).await.is_err(), "{name}: accepted");
        let (after, after_generation) = env.store.snapshot();
        assert_eq!(after_generation, before_generation, "{name}");
        assert_eq!(after, before, "{name}");
    }
    assert!(env.logger_config().is_empty());
}

/// The rejection texts are Go's.
#[tokio::test]
async fn reload_errors_read_as_go_s() {
    let mut env = ReloadEnv::new(&default_settings(), &[], "dry-run: false\n");
    let path = env.path.display().to_string();
    assert_eq!(
        env.reload("log-level: loud\n")
            .await
            .unwrap_err()
            .to_string(),
        format!(
            "config reload failed: config file \"{path}\": key \"log-level\": unknown value \
             \"loud\", must be one of [debug info warn error]"
        )
    );
    assert_eq!(
        env.reload("device-allow: [relative]\n")
            .await
            .unwrap_err()
            .to_string(),
        "config reload: invalid setting: device-allow: glob pattern must start with /dev/: \
         \"relative\""
    );
    assert_eq!(
        env.reload("dry-run: true\n").await.unwrap_err().to_string(),
        format!("config reload: {DRY_RUN_ENABLE}")
    );
}

/// Go: `TestReload_ColdFieldChangeIgnored`.
#[tokio::test]
async fn reload_cold_field_change_ignored() {
    let mut env = ReloadEnv::new(
        &default_settings(),
        &[],
        "metrics-addr: \"127.0.0.1:9090\"\n",
    );
    env.reload("metrics-addr: \"127.0.0.1:9999\"\npolicy-mode: all\n")
        .await
        .unwrap();
    assert_eq!(env.reloader.current.metrics_addr, "127.0.0.1:9090");
    assert_eq!(env.store.load().policy.mode(), Mode::All);
    assert_eq!(
        env.logs.contents(),
        "level=INFO config reload: setting requires a restart; ignored key=metrics-addr\n\
         level=INFO config reloaded generation=2 dry_run=false policy_mode=all device_allow=[] \
         device_deny=[]\n"
    );
    assert_eq!(env.logger_config(), ["text/info"]);
}

/// Go: `TestReload_DryRunTransitions`, true to false is applied.
#[tokio::test]
async fn reload_dry_run_true_to_false_is_applied() {
    let mut env = ReloadEnv::new(&default_settings(), &[], "dry-run: true\n");
    env.reload("dry-run: false\n").await.unwrap();
    assert!(!env.store.load().dry_run);
}

/// Go: `TestReload_DryRunTransitions`, false to true is rejected and nothing changes.
#[tokio::test]
async fn reload_dry_run_false_to_true_is_rejected() {
    let mut env = ReloadEnv::new(
        &default_settings(),
        &[],
        "dry-run: false\npolicy-mode: all\n",
    );
    let (before, before_generation) = env.store.snapshot();
    let error = env
        .reload("dry-run: true\npolicy-mode: opt-in\n")
        .await
        .unwrap_err();
    assert!(matches!(error, ReloadError::DryRunEnable), "{error}");
    let (after, after_generation) = env.store.snapshot();
    assert_eq!((after, after_generation), (before, before_generation));
    assert!(env.logger_config().is_empty());
}

/// Go's `watchSIGHUP`: each signal logs and reloads, a rejection is logged, and the watch ends
/// with its context.
#[tokio::test]
async fn watch_sighup_reloads_on_each_signal() {
    let env = ReloadEnv::new(&default_settings(), &[], "policy-mode: all\n");
    std::fs::write(&env.path, "log-level: loud\n").unwrap();
    let path = env.path.display().to_string();
    let cancel = CancellationToken::new();
    let (sender, receiver) = mpsc::channel(1);
    let watch = tokio::spawn(watch_sighup(cancel.clone(), receiver, env.reloader));
    sender.send(()).await.unwrap();
    crate::test_support::eventually("the rejection", std::time::Duration::from_secs(5), || {
        env.logs.contents().contains("rejected")
    })
    .await;
    cancel.cancel();
    watch.await.unwrap();
    assert_eq!(
        env.logs.contents(),
        format!(
            "level=INFO SIGHUP received; reloading config config_file={path}\n\
             level=ERROR config reload rejected; keeping previous config err=config reload \
             failed: config file \"{path}\": key \"log-level\": unknown value \"loud\", must \
             be one of [debug info warn error]\n"
        )
    );
}
