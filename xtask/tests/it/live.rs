//! What `cargo xtask live` reads of the configuration that names the controller, and what it
//! refuses there before any run, and which lines of a run's progress tell its id and a lease
//! ready.

use std::path::Path;

use expect_test::expect;

use super::settings::{Provider, Settings};
use super::wait;

/// A configuration whose controller has two Modal classes, one of whose GPU two classes share.
const CONFIG: &str = r#"[controller]
name = "alice"
region = "us-west-1"

[controller.modal]
profile = "alice"
config = "modal.toml"
classes.a100 = { gpu = "A100-40GB", gpu_model = "NVIDIA A100-SXM4-40GB", cpu_millis = 8000, memory_bytes = 68719476736, startup_seconds = 1800 }
classes.a100_2 = { gpu = "A100-40GB:2", gpu_model = "NVIDIA A100-SXM4-40GB", cpu_millis = 8000, memory_bytes = 68719476736, startup_seconds = 1800 }
classes.h100 = { gpu = "H100!", gpu_model = "NVIDIA H100 80GB HBM3", cpu_millis = 8000, memory_bytes = 68719476736, startup_seconds = 1800 }
"#;

// Each case: what the settings are, or the refusal.
#[test]
fn settings() {
    let path = Path::new("/home/alice/.config/pumpkin/pumpkin.toml");
    let a100 = Some("NVIDIA A100-SXM4-40GB");
    let without_modal = CONFIG.split("[controller.modal]").next().unwrap();
    let cases = [
        ("ec2", CONFIG, Some(Provider::Ec2), None),
        ("modal", CONFIG, Some(Provider::Modal), a100),
        ("modal without a GPU", CONFIG, Some(Provider::Modal), None),
        (
            "modal with a GPU no class has",
            CONFIG,
            Some(Provider::Modal),
            Some("NVIDIA B200"),
        ),
        (
            "modal without [controller.modal]",
            without_modal,
            Some(Provider::Modal),
            a100,
        ),
        ("any provider", CONFIG, None, None),
        (
            "any provider without [controller.modal]",
            without_modal,
            None,
            None,
        ),
        (
            "no [controller]",
            "[providers.local.docker]\n",
            Some(Provider::Ec2),
            None,
        ),
    ];
    let lines: Vec<String> = cases
        .into_iter()
        .map(|(case, text, provider, gpu_model)| {
            let parsed = Settings::parse(text, path, provider, gpu_model);
            match parsed {
                Ok(settings) => format!("{case}: {settings:?}"),
                Err(error) => format!("{case}: {error}"),
            }
        })
        .collect();
    expect![[r#"
        ec2: Settings { name: "alice", region: "us-west-1", modal: None, gpu_model: None }
        modal: Settings { name: "alice", region: "us-west-1", modal: Some(Modal { profile: "alice", config: Some("/home/alice/.config/pumpkin/modal.toml") }), gpu_model: Some("NVIDIA A100-SXM4-40GB") }
        modal without a GPU: a live run on Modal needs the model of a GPU that a class of `[controller.modal]` has, the cheapest best: give one of `NVIDIA A100-SXM4-40GB`, `NVIDIA H100 80GB HBM3` with `--gpu-model`
        modal with a GPU no class has: no class of /home/alice/.config/pumpkin/pumpkin.toml's `[controller.modal]` has the GPU `NVIDIA B200`: give one of `NVIDIA A100-SXM4-40GB`, `NVIDIA H100 80GB HBM3`
        modal without [controller.modal]: /home/alice/.config/pumpkin/pumpkin.toml names no `[controller.modal]`, which a live run on Modal needs: name the workspace's profile and classes there, as the README says
        any provider: Settings { name: "alice", region: "us-west-1", modal: Some(Modal { profile: "alice", config: Some("/home/alice/.config/pumpkin/modal.toml") }), gpu_model: None }
        any provider without [controller.modal]: Settings { name: "alice", region: "us-west-1", modal: None, gpu_model: None }
        no [controller]: /home/alice/.config/pumpkin/pumpkin.toml names no `[controller]`, which a live run goes through: name the one `deploy/aws` made, as the README says"#]].assert_eq(&lines.join("\n"));
}

// Lines that `./pumpkin run` said of a run that it sent to a controller.
#[test]
fn run_started() {
    let lines = [
        "pumpkin: run 20261121-064723-9fa36f9e goes to controller `alice`",
        "pumpkin: run 20261121-064723-9fa36f9e is a job on controller `alice`, which builds it, \
         then runs it",
        "pumpkin: run 20261121-064723-9fa36f9e started",
    ];
    let shown: Vec<String> = (lines.iter())
        .map(|line| format!("{:?}: {line}", wait::run_started(line)))
        .collect();
    expect![[r#"
        Some("20261121-064723-9fa36f9e"): pumpkin: run 20261121-064723-9fa36f9e goes to controller `alice`
        None: pumpkin: run 20261121-064723-9fa36f9e is a job on controller `alice`, which builds it, then runs it
        None: pumpkin: run 20261121-064723-9fa36f9e started"#]].assert_eq(&shown.join("\n"));
}

// Lines that `./pumpkin wait` says, as its own tests pin them.
#[test]
fn lease_ready() {
    let lines = [
        "pumpkin:    0:38 cpu lease 0 of agent 0 ready",
        "pumpkin:    2:06 gpu lease 1 of agent 0 ready with NVIDIA A100-SXM4-80GB",
        "pumpkin:    8:25 gpu lease 3 of agent 0 ready with NVIDIA A100 80GB PCIe (its role needs NVIDIA A100-SXM4-80GB)",
        "pumpkin:    0:55 box lease 0 of job 0 ready",
        "pumpkin:    0:39 cpu lease 0 ready",
        "pumpkin:    0:19 cpu lease 0 of agent 0 asked on aws/judge",
        "pumpkin:    1:14 gpu lease 1 of agent 0 ended: closed by its holder",
        "     0:41 gpu lease 1 on modal/a100_40 with NVIDIA A100-SXM4-40GB, 1m41s: charged 1m34s, 0s of it busy, 0 commands; closed as the work phase ended",
        "pumpkin: run 20261114-215915-8388a47f started",
    ];
    let shown: Vec<String> = (lines.iter())
        .map(|line| format!("{}: {line}", wait::lease_ready(line)))
        .collect();
    expect![[r#"
        true: pumpkin:    0:38 cpu lease 0 of agent 0 ready
        true: pumpkin:    2:06 gpu lease 1 of agent 0 ready with NVIDIA A100-SXM4-80GB
        true: pumpkin:    8:25 gpu lease 3 of agent 0 ready with NVIDIA A100 80GB PCIe (its role needs NVIDIA A100-SXM4-80GB)
        false: pumpkin:    0:55 box lease 0 of job 0 ready
        false: pumpkin:    0:39 cpu lease 0 ready
        false: pumpkin:    0:19 cpu lease 0 of agent 0 asked on aws/judge
        false: pumpkin:    1:14 gpu lease 1 of agent 0 ended: closed by its holder
        false:      0:41 gpu lease 1 on modal/a100_40 with NVIDIA A100-SXM4-40GB, 1m41s: charged 1m34s, 0s of it busy, 0 commands; closed as the work phase ended
        false: pumpkin: run 20261114-215915-8388a47f started"#]].assert_eq(&shown.join("\n"));
}

// What a failed `./pumpkin reap` said, then whether its failure is only other runs': of runs
// that it can't reap, or whose provider it can't reach, and of a connection that ended.
#[test]
fn others_failed() {
    let this = "20261114-215915-8388a47f";
    let other = "pumpkin: run 20261114-220102-1c2d3e4f's sandboxes on provider `modal`, if it \
                 left any, can't be removed, as it can't be reached: no token\n\
                 pumpkin: 1 run can't be reaped, as said above: fix what each says, and reap \
                 again\n";
    let among = "pumpkin: run 20261114-220102-1c2d3e4f can't be reaped: a record's file is \
                 missing\n\
                 pumpkin: run 20261114-215915-8388a47f can't be reaped: a record's file is \
                 missing\n\
                 pumpkin: 2 runs can't be reaped, as said above: fix what each says, and reap \
                 again\n";
    let none = "pumpkin: the connection to controller `alice` ended (exit status: 255), as said \
                above: reap again: pumpkin reap\n";
    let cases = [
        ("only another run named", other),
        ("this run named among others", among),
        ("no run named", none),
    ];
    let shown: Vec<String> = (cases.iter())
        .map(|(case, said)| format!("{case}: {}", wait::others_failed(said, this)))
        .collect();
    expect![[r#"
        only another run named: true
        this run named among others: false
        no run named: false"#]]
    .assert_eq(&shown.join("\n"));
}
