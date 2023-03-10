#![deny(rust_2018_idioms)]
#![feature(exit_status_error)]
#![feature(result_option_inspect)]

use chrono_tz::{Poland as AppTimeZone, Tz};
use clap::{Parser, Subcommand};
#[allow(unused_imports)]
use eyre::{bail, eyre, Result, WrapErr};
use futures::future::ready;
use futures::stream::{StreamExt, TryStreamExt};
use itertools::Itertools;
use std::path::Path;
use std::{fmt::Display, path::PathBuf, process::Stdio};
use tokio::io::AsyncBufReadExt;
#[allow(unused_imports)]
use tracing::{debug, debug_span, error, info, info_span, instrument, trace, warn};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    // /// Optional name to operate on
    // name: Option<String>,

    // /// Sets a custom config file
    // #[arg(short, long, value_name = "FILE")]
    // config: Option<PathBuf>,

    // /// Turn debugging information on
    // #[arg(short, long, action = clap::ArgAction::Count)]
    // debug: u8,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// does testing things
    Scan,
}

fn now() -> chrono::DateTime<Tz> {
    chrono::Utc::now().with_timezone(&AppTimeZone)
}

pub trait CommandExt {
    fn debug(&mut self) -> &mut Self;
}

impl CommandExt for tokio::process::Command {
    fn debug(&mut self) -> &mut Self {
        warn!(command=?self, "executing command");
        self
    }
}

#[derive(Debug)]
struct Device {
    id: String,
    description: String,
}

impl Display for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.id, self.description)
    }
}

fn base() -> tokio::process::Command {
    let mut base = tokio::process::Command::new("scanimage");
    base.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    base
}

async fn load_file<P: AsRef<Path>>(path: P) -> Result<(String, Vec<u8>)> {
    let content = tokio::fs::read(&path).await.wrap_err("opening file")?;
    tokio::fs::remove_file(&path)
        .await
        .wrap_err("removing file")
        .and_then(|_| {
            path.as_ref()
                .file_name()
                .ok_or_else(|| eyre!("file without a filename?"))
                .and_then(|filename| {
                    filename
                        .to_str()
                        .ok_or_else(|| eyre!("non utf-8 filename"))
                        .map(ToOwned::to_owned)
                })
                .map(|filename| (filename, content))
        })
}

#[instrument(err)]
async fn find_file_based_on_batch_prefix(
    line_number: u32,
    batch_prefix: String,
) -> Result<(String, Vec<u8>)> {
    info!("loading file into memory");
    Box::pin(
        tokio_stream::wrappers::ReadDirStream::new(
            tokio::fs::read_dir(".")
                .await
                .wrap_err("reading directory")?,
        )
        .filter_map(|e| ready(e.ok()))
        .map(|e| e.path())
        .filter(|p| ready(p.is_file()))
        .filter(move |name| {
            let name = name
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            ready(
                name.ends_with("tiff")
                    && name.starts_with(batch_prefix.as_str())
                    && name
                        .trim_start_matches(batch_prefix.as_str())
                        .contains(&line_number.to_string()),
            )
        })
        .then(|file| async move { load_file(file).await }),
    )
    .next()
    .await
    .ok_or_else(|| eyre!("no file found"))
    .and_then(|v| v.inspect(|(filename, _)| info!(filename, "succesfully loaded")))
}

#[derive(Debug)]
pub enum MessageReceived {
    Line(String),
    ProcessExitedWithAnError(eyre::Report),
    ProcessExitedSuccess,
}
async fn perform_scan(batch_prefix: String, device: &Device) -> Result<Vec<(String, Vec<u8>)>> {
    let mut command = base();
    command
        .args(["-d", device.id.to_string().as_str()])
        .args(["--format", "tiff"])
        .args([&format!("--batch={batch_prefix}p%04d.tiff")])
        .args(["--resolution", "300"])
        .args(["--progress"])
        .args(["--source", "ADF"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .debug();
    let mut child = command.spawn().wrap_err("running the scan routine")?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<MessageReceived>();
    let send_lines = {
        move |tx: &tokio::sync::mpsc::UnboundedSender<_>, v: &str| -> Result<()> {
            v.lines()
                .flat_map(|line| line.split('\r'))
                .map(|line| line.trim())
                .map(move |line| {
                    tx.send(MessageReceived::Line(line.to_string()))
                        .wrap_err("ram problem?")
                })
                .collect::<Result<Vec<_>>>()
                .map(|_| ())
        }
    };
    {
        if let Some(stdout) = child.stdout.take() {
            let tx = tx.clone();
            tokio::task::spawn(async move {
                let mut reader = tokio::io::BufReader::new(stdout).lines();
                while let Some(line) = reader.next_line().await.ok().and_then(|o| o) {
                    let send_lines = send_lines.clone();
                    debug!(stdout=%line);
                    send_lines(&tx, &line).expect("sending lines");
                }
            });
        }
    }
    {
        if let Some(stderr) = child.stderr.take() {
            let tx = tx.clone();
            tokio::task::spawn(async move {
                let mut reader = tokio::io::BufReader::new(stderr).lines();
                while let Some(line) = reader.next_line().await.ok().and_then(|o| o) {
                    let send_lines = send_lines.clone();
                    debug!(stderr=%line);
                    send_lines(&tx, &line).expect("sending lines");
                }
            });
        }
    }
    tokio::task::spawn(async move {
        match child
            .wait()
            .await
            .wrap_err("running the scanner command resulted in a failure")
            .and_then(|status| status.exit_ok().wrap_err("scanning returned an error"))
        {
            Ok(_) => {
                debug!("scanning process exited");
                tx.send(MessageReceived::ProcessExitedSuccess).unwrap();
                // tx.closed().await;
            }
            Err(error) => tx
                .send(MessageReceived::ProcessExitedWithAnError(error))
                .expect("memory problem? thread crashed?"),
        }
    });
    tokio_stream::wrappers::UnboundedReceiverStream::new(rx)
        .filter_map(|line| async move {
            match line {
                MessageReceived::Line(line) => {
                    debug!(line=?line);
                    match extract_print_progress(&line) {
                        Err(message) => {
                            debug!(?message, "this line is no good fit");
                            None
                        }
                        Ok(l) => {
                            info!(line=%l, "scanning successful");
                            Some(Ok(l))
                        }
                    }
                }
                MessageReceived::ProcessExitedWithAnError(error) => Some(Err(error)),
                MessageReceived::ProcessExitedSuccess => None,
            }
        })
        .then(move |line| {
            let batch_prefix = batch_prefix.clone();
            async move {
                let line = line?;
                find_file_based_on_batch_prefix(line, batch_prefix.clone()).await
            }
        })
        .try_collect()
        .await
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install().ok();
    tracing_subscriber::fmt::init();
    let Cli { command } = Cli::parse();
    let now = now().format("%Y-%m-%d--%H-%M-%S");
    debug!("debug log enabled");
    let list_devices = || async {
        info!("listing available devices");
        base()
            .arg("-L")
            .debug()
            .output()
            .await
            .wrap_err("reading current devices")
            .and_then(|output| {
                output
                    .status
                    .exit_ok()
                    .wrap_err("failed to read devices")
                    .map(|_| output)
            })
            .and_then(|output| String::from_utf8(output.stdout).wrap_err("parsing output"))
            .map(|out| {
                out.lines()
                    .map(|line| {
                        debug!(?line, "processing line");
                        let id = line
                            .split(' ')
                            .skip(1)
                            .take_while(|word| word != &"is")
                            .join(" ")
                            .trim_matches('`')
                            .trim_matches('\'')
                            .to_owned();
                        let (id, description) = (id, line.to_owned());
                        Device { id, description }
                    })
                    .collect_vec()
            })
    };
    let batch_prefix = format!("{now}---");

    match command {
        Commands::Scan => {
            let devices = list_devices().await?;
            let device = inquire::Select::new("which scanner?", devices)
                .prompt()
                .wrap_err("select a proper option")?;
            info!("collecting first batch");
            let first_batch = perform_scan(batch_prefix.clone(), &device).await?;

            let all_pages =
                match inquire::Confirm::new("wanna flip the sides and do the double sided scan?")
                    .prompt()
                    .wrap_err("oops")?
                {
                    true => {
                        let second_batch = perform_scan(batch_prefix.clone(), &device).await?;
                        first_batch
                            .into_iter()
                            .interleave(second_batch.into_iter().rev())
                            .collect_vec()
                    }
                    false => first_batch,
                };
            if all_pages.is_empty() {
                bail!("something went wrong, there should be at least one image here...");
            }
            let tempdir = tempfile::tempdir().wrap_err("creating a temporary directory");
            let random_file = || {
                tempdir
                    .as_ref()
                    .map_err(|e| eyre!("{e:?}"))
                    .map(|tempdir| tempdir.path().join(format!("{}", uuid::Uuid::new_v4())))
            };
            let filenames = all_pages
                .into_iter()
                .map(|(file, content)| {
                    info!("writing {file}");
                    random_file().and_then(|tempfile| {
                        std::fs::write(&tempfile, content)
                            .wrap_err_with(|| format!("writing {file}"))
                            .map(|_| tempfile)
                    })
                })
                .collect::<Result<Vec<_>>>()?;

            let filename =
                inquire::Text::new("how would you like to name your file (without extension)")
                    .prompt()?;
            let path = PathBuf::from(filename).with_extension("pdf");
            filenames
                .into_iter()
                .fold(tokio::process::Command::new("convert"), |mut acc, next| {
                    acc.arg(next);
                    acc
                })
                .arg(&path)
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .debug()
                .spawn()
                .wrap_err("spawning the merge command")?
                .wait()
                .await
                .wrap_err("merging went wrong")?
                .exit_ok()
                .wrap_err("something went wrong...")?;
            info!("everything is done");
            println!();
            println!("{}", path.to_string_lossy());
            Ok(())
        }
    }
}

fn extract_print_progress(line: &str) -> Result<u32> {
    const MARKER: &str = "Scanned page";
    line.starts_with(MARKER)
        .then_some(line)
        .ok_or_else(|| eyre!("line doesn't start with the marker ('{MARKER}')"))
        .and_then(|line| {
            line.split(' ')
                .nth(2)
                .map(|digit| digit.trim_matches('.'))
                .map(ToOwned::to_owned)
                .ok_or_else(|| eyre!("line too short"))
        })
        .and_then(|num| num.parse().wrap_err("invalid number..."))
}

#[cfg(test)]
mod test {
    use super::*;
    use test_log::test;
    #[test]
    fn scanner_line_processing_ok() {
        assert!(extract_print_progress("").is_err())
    }
}
