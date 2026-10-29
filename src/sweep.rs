//! Frequency sweep worker.
//!
//! Runs on its own thread so stepping and dwell timing never block the BLE/HTTP
//! tasks. A new `Start` command interrupts a running sweep; `Stop` finishes the
//! current step and leaves the output at the last frequency.

use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::app::{App, SweepCmd, SweepConfig};

enum Exit {
    Stop,
    Restart(SweepConfig),
}

pub fn run(app: Arc<App>, rx: Receiver<SweepCmd>) {
    let mut pending: Option<SweepConfig> = None;

    loop {
        let config = match pending.take() {
            Some(config) => config,
            None => match rx.recv() {
                Ok(SweepCmd::Start(config)) => config,
                Ok(SweepCmd::Stop) => continue,
                Err(_) => break,
            },
        };

        app.set_sweep_started(config);
        match run_sweep(&app, &rx, config) {
            Exit::Stop => app.set_sweep_stopped(),
            Exit::Restart(config) => pending = Some(config),
        }
    }

    app.set_sweep_stopped();
}

fn run_sweep(app: &Arc<App>, rx: &Receiver<SweepCmd>, config: SweepConfig) -> Exit {
    let direction: i64 = if config.end_hz >= config.start_hz {
        1
    } else {
        -1
    };
    let step = config.step_hz as i64;
    let dwell = Duration::from_millis(config.dwell_ms as u64);

    loop {
        let mut freq = config.start_hz as i64;
        loop {
            let in_range = if direction > 0 {
                freq <= config.end_hz as i64
            } else {
                freq >= config.end_hz as i64
            };
            if !in_range {
                break;
            }

            if let Some(exit) = check_commands(rx) {
                return exit;
            }

            let step_started = Instant::now();
            match app.set_channel(config.channel, Some(freq as u64), Some(true), None) {
                Ok(_) => app.set_sweep_current(freq as u64),
                Err(err) => {
                    log::warn!("sweep step {freq} Hz failed: {err}");
                    return Exit::Stop;
                }
            }

            // Dwell is measured from the start of the step; the I2C
            // reconfiguration time counts toward it.
            let remaining = dwell.saturating_sub(step_started.elapsed());
            if !remaining.is_zero() {
                match rx.recv_timeout(remaining) {
                    Ok(SweepCmd::Start(config)) => return Exit::Restart(config),
                    Ok(SweepCmd::Stop) => return Exit::Stop,
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return Exit::Stop,
                }
            }

            freq += direction * step;
        }

        if !config.repeat {
            break;
        }
        if let Some(exit) = check_commands(rx) {
            return exit;
        }
    }

    Exit::Stop
}

fn check_commands(rx: &Receiver<SweepCmd>) -> Option<Exit> {
    match rx.try_recv() {
        Ok(SweepCmd::Start(config)) => Some(Exit::Restart(config)),
        Ok(SweepCmd::Stop) => Some(Exit::Stop),
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => Some(Exit::Stop),
    }
}
