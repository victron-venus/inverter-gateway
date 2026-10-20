use std::{future::Future, io};

use tokio::signal;
use tracing::{info, warn};

use inverter_gateway::config::Config;
use inverter_gateway::mqtt_bridge::MqttBridge;
use inverter_gateway::state::AppState;
use inverter_gateway::{http, transport};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("inverter-gateway {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    init_tracing();

    let cfg = Config::from_env()?;
    if cfg.allow_insecure {
        warn!(
            "GATEWAY_ALLOW_INSECURE=1 is set — bearer auth is DISABLED. Do not use in production."
        );
    }
    info!(bind = %cfg.http_bind, mqtt = %cfg.mqtt_host, "starting inverter-gateway");

    let shutdown = shutdown_signal()?;
    let listeners = transport::Listeners::bind(cfg.http_bind, cfg.https.as_ref()).await?;
    let state = AppState::new(cfg.clone());
    state.shared.start_sse_coalesce();
    let mqtt = MqttBridge::start(state.clone(), cfg.clone())?;

    let app = http::router(state.clone());
    let mut shutdown_result = Ok(());
    let result = listeners
        .serve(app, async {
            shutdown_result = shutdown.await;
        })
        .await;
    mqtt.shutdown().await;
    result?;
    shutdown_result?;
    Ok(())
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,inverter_gateway=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(unix)]
fn shutdown_signal() -> io::Result<impl Future<Output = io::Result<()>>> {
    prepare_shutdown(
        || {
            let mut stream = signal::unix::signal(signal::unix::SignalKind::interrupt())?;
            Ok(async move { stream.recv().await })
        },
        || {
            let mut stream = signal::unix::signal(signal::unix::SignalKind::terminate())?;
            Ok(async move { stream.recv().await })
        },
    )
}

#[cfg(windows)]
fn shutdown_signal() -> io::Result<impl Future<Output = io::Result<()>>> {
    prepare_shutdown(
        || {
            let mut stream = signal::windows::ctrl_c()?;
            Ok(async move { stream.recv().await })
        },
        || Ok(std::future::pending()),
    )
}

fn prepare_shutdown<C, T>(
    register_ctrl_c: impl FnOnce() -> io::Result<C>,
    register_terminate: impl FnOnce() -> io::Result<T>,
) -> io::Result<impl Future<Output = io::Result<()>>>
where
    C: Future<Output = Option<()>>,
    T: Future<Output = Option<()>>,
{
    let ctrl_c = register_ctrl_c().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to register ctrl-c handler: {error}"),
        )
    })?;
    let terminate = register_terminate().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to register terminate handler: {error}"),
        )
    })?;
    Ok(async move {
        let (name, received) = tokio::select! {
            received = ctrl_c => ("ctrl-c", received),
            received = terminate => ("terminate", received),
        };
        if received.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("{name} signal stream closed"),
            ));
        }
        warn!("{name} received");
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::Cell,
        future::{pending, ready, Pending},
    };

    #[test]
    fn ctrl_c_registration_error_is_returned_before_waiting() {
        let terminate_called = Cell::new(false);
        let error = prepare_shutdown(
            || {
                Err::<Pending<Option<()>>, _>(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected ctrl-c registration failure",
                ))
            },
            || {
                terminate_called.set(true);
                Ok(pending::<Option<()>>())
            },
        )
        .err()
        .expect("registration failure must not produce a shutdown future");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            error.to_string(),
            "failed to register ctrl-c handler: injected ctrl-c registration failure"
        );
        assert!(!terminate_called.get());
    }

    #[test]
    fn terminate_registration_error_is_returned_before_waiting() {
        let ctrl_c_called = Cell::new(false);
        let error = prepare_shutdown(
            || {
                ctrl_c_called.set(true);
                Ok(pending::<Option<()>>())
            },
            || {
                Err::<Pending<Option<()>>, _>(io::Error::other(
                    "injected terminate registration failure",
                ))
            },
        )
        .err()
        .expect("registration failure must not produce a shutdown future");

        assert!(ctrl_c_called.get());
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(
            error.to_string(),
            "failed to register terminate handler: injected terminate registration failure"
        );
    }

    #[test]
    fn registered_handlers_wait_for_either_signal() {
        for send_ctrl_c in [true, false] {
            let registrations = Cell::new(0);
            let (ctrl_c_sender, ctrl_c_receiver) = tokio::sync::oneshot::channel();
            let (terminate_sender, terminate_receiver) = tokio::sync::oneshot::channel();
            let shutdown = prepare_shutdown(
                || {
                    registrations.set(registrations.get() + 1);
                    Ok(async { ctrl_c_receiver.await.ok() })
                },
                || {
                    registrations.set(registrations.get() + 1);
                    Ok(async { terminate_receiver.await.ok() })
                },
            )
            .unwrap();
            assert_eq!(registrations.get(), 2, "registration must be synchronous");
            let mut shutdown = tokio_test::task::spawn(shutdown);
            tokio_test::assert_pending!(shutdown.poll());

            if send_ctrl_c {
                ctrl_c_sender.send(()).unwrap();
                tokio_test::assert_ready!(shutdown.poll()).unwrap();
                drop(terminate_sender);
            } else {
                terminate_sender.send(()).unwrap();
                tokio_test::assert_ready!(shutdown.poll()).unwrap();
                drop(ctrl_c_sender);
            }
        }
    }

    #[test]
    fn closed_ctrl_c_stream_returns_an_error() {
        let shutdown =
            prepare_shutdown(|| Ok(ready(None)), || Ok(pending::<Option<()>>())).unwrap();
        let mut shutdown = tokio_test::task::spawn(shutdown);
        let error = tokio_test::assert_ready!(shutdown.poll()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(error.to_string(), "ctrl-c signal stream closed");
    }

    #[test]
    fn closed_terminate_stream_returns_an_error() {
        let shutdown =
            prepare_shutdown(|| Ok(pending::<Option<()>>()), || Ok(ready(None))).unwrap();
        let mut shutdown = tokio_test::task::spawn(shutdown);
        let error = tokio_test::assert_ready!(shutdown.poll()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(error.to_string(), "terminate signal stream closed");
    }

    #[cfg(unix)]
    mod real_signals {
        use super::*;
        use std::{
            io::{BufRead, BufReader, Write},
            process::{Child, Command, Stdio},
            sync::mpsc,
            thread::{self, JoinHandle},
            time::{Duration, Instant},
        };

        const CHILD_ENV: &str = "INVERTER_GATEWAY_SIGNAL_TEST_CHILD";
        const READY: &str = "INVERTER_GATEWAY_SIGNAL_TEST_READY";
        const DEADLINE: Duration = Duration::from_secs(10);

        struct TestChild {
            process: Child,
            output: Option<JoinHandle<()>>,
        }

        impl Drop for TestChild {
            fn drop(&mut self) {
                // Only this test's own child can be killed or reaped.
                let _ = self.process.kill();
                let _ = self.process.wait();
                if let Some(output) = self.output.take() {
                    let _ = output.join();
                }
            }
        }

        fn check_signal(signal: &str) {
            let process = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::real_signals::shutdown_signal_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env(CHILD_ENV, "1")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            let mut child = TestChild {
                process,
                output: None,
            };
            let stdout = child.process.stdout.take().unwrap();
            let (lines, receiver) = mpsc::channel();
            child.output = Some(thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    match line {
                        Ok(line) => {
                            if lines.send(line).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }));
            let deadline = Instant::now() + DEADLINE;
            loop {
                let line = receiver
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .expect("signal child did not report readiness before the deadline");
                if line == READY {
                    break;
                }
            }
            assert!(
                child.process.try_wait().unwrap().is_none(),
                "signal child exited before receiving a signal"
            );
            let status = Command::new("/bin/kill")
                .args(["-s", signal, &child.process.id().to_string()])
                .status()
                .unwrap();
            assert!(status.success(), "failed to send {signal} to signal child");
            loop {
                if let Some(status) = child.process.try_wait().unwrap() {
                    assert!(
                        status.success(),
                        "signal child failed for {signal}: {status}"
                    );
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "signal child did not finish after {signal}"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }

        #[test]
        fn sigint_shuts_down_an_isolated_child() {
            check_signal("INT");
        }

        #[test]
        fn sigterm_shuts_down_an_isolated_child() {
            check_signal("TERM");
        }

        #[test]
        #[ignore = "helper process for isolated OS-signal tests"]
        fn shutdown_signal_child() {
            if std::env::var(CHILD_ENV).as_deref() != Ok("1") {
                return;
            }
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let mut shutdown = Box::pin(shutdown_signal().unwrap());
                    assert!(futures_util::poll!(&mut shutdown).is_pending());
                    println!("\n{READY}");
                    io::stdout().flush().unwrap();
                    tokio::time::timeout(Duration::from_secs(5), shutdown)
                        .await
                        .expect("child timed out waiting for its signal")
                        .expect("child signal handler failed");
                });
        }
    }
}
