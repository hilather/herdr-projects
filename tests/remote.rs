#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods)] // Test-only spawns outside the library may skip the spawn gate.
//! Which SSH target a saved machine resolves to, through the compiled CLI:
//! `doctor` resolves every machine an open remote thread uses, exactly as the
//! copies and report polls do. herdr is a fake whose `machine list --json`
//! answers from `machines.json` (and fails when it is missing).
use serde_json::{json, Value};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command};

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

const FAKE_HERDR: &str = "#!/bin/sh\ncase \"$*\" in\n--version) echo 'herdr 0.9.1';;\n'machine list --json') cat \"$HOME/machines.json\" || exit 1;;\n*) echo '{\"result\":{}}';;\nesac\n";

struct Lab { home: tempfile::TempDir }

impl Lab {
    /// Project `demo` with one open remote thread on each machine in `machines`.
    fn new(machines: &[&str]) -> Self {
        let lab = Lab { home: tempfile::tempdir().unwrap() };
        fs::write(lab.path("herdr"), FAKE_HERDR).unwrap();
        fs::set_permissions(lab.path("herdr"), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(lab.command().args(["new", "demo"]).output().unwrap().status.success());
        for (n, machine) in machines.iter().enumerate() {
            let id = format!("t-{:04}", n + 1);
            let record = json!({"id": id, "title": machine, "status": "open", "kind": "adopted", "created": jiff::Timestamp::now().to_string(),
                "machine": machine, "agent": "claude", "workspace_id": "w", "tab_id": "w:t", "pane_id": format!("p{n}"), "cwd": "/remote/work"});
            fs::write(lab.path("root/demo/threads").join(format!("{id}.toml")), toml::to_string(&record).unwrap()).unwrap();
        }
        lab
    }
    fn path(&self, name: &str) -> PathBuf { self.home.path().join(name) }
    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command.env_clear().env("HOME", self.home.path()).env("PATH", "/usr/bin:/bin").env("HERDR_BIN_PATH", self.path("herdr")).arg("--root").arg(self.path("root"));
        command
    }
    fn listing(&self, machines: Option<Value>) {
        match machines { Some(list) => fs::write(self.path("machines.json"), list.to_string()).unwrap(), None => { let _ = fs::remove_file(self.path("machines.json")); } }
    }
    fn config(&self, text: &str) {
        let dir = self.path(".config/herdr-projects");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), text).unwrap();
    }
    /// `doctor`'s verdict on each machine: `ok TARGET` or `FAIL ERROR`.
    fn machines(&self) -> std::collections::BTreeMap<String, String> {
        let out = self.command().arg("doctor").output().unwrap();
        String::from_utf8(out.stdout).unwrap().lines().filter_map(|line| {
            let (mark, rest) = line.strip_prefix('[')?.split_once("] machine ")?;
            let (machine, detail) = rest.split_once(": ")?;
            Some((machine.to_string(), format!("{} {}", mark.trim(), detail.strip_prefix("ssh target ").unwrap_or(detail))))
        }).collect()
    }
}

fn expect(lab: &Lab, cases: &[(&str, &str)]) {
    let found = lab.machines();
    for (machine, verdict) in cases {
        let got = found.get(*machine).unwrap_or_else(|| panic!("no verdict for {machine}: {found:?}"));
        assert!(got.starts_with(verdict), "{machine}: expected {verdict}, got {got}");
    }
}

/// Replaces `target_comes_from_herdr_then_from_config` and
/// `saved_route_selection_matches_id_precedence_and_refuses_ambiguous_or_disabled_profiles`.
///
/// herdr's saved machines are matched by profile id first, then by label; a
/// machine herdr does not list falls back to `[machines.NAME] ssh` in
/// `config.toml`, also when the listing itself fails. An ambiguous label, a
/// disabled profile or a listing with duplicate ids is refused and never
/// falls back to the config file.
#[test]
fn saved_machines_resolve_by_id_then_label_then_config_and_refuse_ambiguity() {
    let lab = Lab::new(&["m1", "abc", "chosen", "box", "nope", "same", "off"]);
    lab.config("[machines.box]\nssh = \"me@box.local\"\n[machines.same]\nssh = \"config-same\"\n[machines.off]\nssh = \"config-off\"\n[machines.chosen]\nssh = \"config-chosen\"\n");
    lab.listing(Some(json!([
        {"id": "abc", "label": "m1", "target": "m1.local", "session": "default"},
        {"id": "chosen", "label": "first", "target": "correct.local"},
        {"id": "other", "label": "chosen", "target": "wrong.local"},
        {"id": "s1", "label": "same", "target": "a.local"},
        {"id": "s2", "label": "same", "target": "b.local"},
        {"id": "o1", "label": "off", "target": "off.local", "enabled": false},
    ])));
    expect(&lab, &[
        ("m1", "ok m1.local"), ("abc", "ok m1.local"), ("chosen", "ok correct.local"), ("box", "ok me@box.local"),
        ("nope", "FAIL machine `nope` has no SSH target: it is not in `herdr machine list`, and config.toml has no [machines.nope] ssh"),
        ("same", "FAIL ambiguous machine label; use its profile ID"), ("off", "FAIL saved machine is disabled"),
    ]);

    // herdr cannot list its machines: only the config file answers.
    lab.listing(None);
    expect(&lab, &[("box", "ok me@box.local"), ("same", "ok config-same"), ("chosen", "ok config-chosen"), ("m1", "FAIL machine `m1` has no SSH target")]);

    // Duplicate profile ids make the whole listing untrustworthy.
    lab.listing(Some(json!([{"id": "a", "label": "m1", "target": "a.local"}, {"id": "a", "label": "other", "target": "b.local"}])));
    expect(&lab, &[("m1", "FAIL duplicate saved machine ID"), ("box", "FAIL duplicate saved machine ID")]);
}
