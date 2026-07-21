use std::time::Duration;

use tokio::time::timeout;
use tutti_server::pty::{PaneSize, PtyPane, PtySpec, Snapshot};

const TIMEOUT: Duration = Duration::from_secs(5);

/// Poll the pane's snapshot until `pred` holds or the timeout elapses, waking
/// on the output-notification channel rather than fixed sleeps.
async fn wait_for(pane: &PtyPane, pred: impl Fn(&Snapshot) -> bool) -> bool {
    let mut rx = pane.output_receiver();
    timeout(TIMEOUT, async {
        loop {
            if pred(&pane.snapshot()) {
                return true;
            }
            if rx.changed().await.is_err() {
                return pred(&pane.snapshot());
            }
        }
    })
    .await
    .unwrap_or(false)
}

#[tokio::test]
async fn spawn_captures_output_and_exits_zero() {
    let mut spec = PtySpec::new("/bin/sh");
    spec.args = vec!["-c".into(), "printf hello-tutti; sleep 0.2".into()];

    let pane = PtyPane::spawn(spec, PaneSize::new(24, 80)).expect("spawn");

    let found = wait_for(&pane, |s| s.text().contains("hello-tutti")).await;
    assert!(
        found,
        "expected hello-tutti in snapshot, got: {:?}",
        pane.snapshot().lines
    );

    let exit = timeout(TIMEOUT, pane.wait()).await.expect("exit timed out");
    assert!(exit.success, "expected clean exit, got {exit:?}");
    assert_eq!(exit.code, 0);
}

#[tokio::test]
async fn resize_updates_snapshot_dimensions() {
    let pane = PtyPane::spawn(PtySpec::new("/bin/cat"), PaneSize::new(24, 80)).expect("spawn");

    let before = pane.snapshot();
    assert_eq!((before.rows, before.cols), (24, 80));

    pane.resize(20, 100).expect("resize");

    let after = pane.snapshot();
    assert_eq!((after.rows, after.cols), (20, 100));

    pane.kill().expect("kill");
    timeout(TIMEOUT, pane.wait())
        .await
        .expect("exit not detected");
}

#[tokio::test]
async fn input_echoes_into_grid_and_kill_is_detected() {
    let pane = PtyPane::spawn(PtySpec::new("/bin/cat"), PaneSize::new(24, 80)).expect("spawn");

    assert!(pane.exit_status().is_none());

    pane.write_input(b"ping\n").expect("write");

    let echoed = wait_for(&pane, |s| s.text().contains("ping")).await;
    assert!(
        echoed,
        "expected ping to echo into grid, got: {:?}",
        pane.snapshot().lines
    );

    pane.kill().expect("kill");

    let exit = timeout(TIMEOUT, pane.wait())
        .await
        .expect("kill not detected");
    assert!(!exit.success, "killed child should not report success");
}
