use std::{
    borrow::Cow,
    convert::Infallible,
    fmt,
    path::PathBuf,
    str::FromStr,
    time::{Duration, Instant},
};

use color_eyre::{Report, Result};
use indicatif::{ProgressBar, ProgressStyle};
use jsonrpsee::client_transport::ws::Uri;
use sp_core::H256;

use crate::Chain;

#[derive(Clone, Debug)]
pub enum StorageFile {
    None,
    Path(PathBuf),
}

impl FromStr for StorageFile {
    type Err = Infallible;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("none") {
            Ok(StorageFile::None)
        } else {
            Ok(StorageFile::Path(PathBuf::from(s)))
        }
    }
}

#[derive(clap::Parser)]
pub struct Cli {
    /// Path to the creditcoin-node binary to use
    /// for chain-spec creation.
    #[clap(long = "bin")]
    pub binary: PathBuf,
    /// Path to the runtime WASM blob to use
    /// in the forked chain. If omitted this will
    #[clap(long)]
    pub runtime: Option<PathBuf>,
    /// Path to write the fork's chain-spec to
    #[clap(short, long, default_value = "fork.json")]
    pub out: PathBuf,
    /// Name of the original chain to fork from
    /// (e.g. "dev", "test", "main")
    #[clap(long = "orig")]
    pub original_chain: Chain,
    /// Name of the chain to use as the base for the fork's
    /// chain-spec
    #[clap(long = "base", default_value_t = Chain::Dev)]
    pub base_chain: Chain,
    /// Path to the cached runtime storage file. If passed
    /// and the file does not exist, the chain's state will
    /// be fetched and written to the given path. If the file
    /// does exist, the state in the file will be used. If omitted,
    /// state will be fetched from a running node and will not be
    /// saved to a file.
    #[clap(long)]
    pub storage: Option<StorageFile>,
    /// Block hash to fetch the on-chain state from.
    #[clap(long)]
    pub at: Option<H256>,
    /// Name for the new, forked chain. Defaults to `{original}-fork`.
    #[clap(long)]
    pub name: Option<String>,
    /// Chain ID for the new, forked chain. Defaults to `{original}-fork`.
    #[clap(long)]
    pub id: Option<String>,

    /// Url for the live node from which to pull state and other required data.
    #[clap(long, default_value = "ws://127.0.0.1:9944")]
    pub rpc: Uri,

    /// A list of pallets to keep state from. If omitted,
    /// most pallets with runtime storage will maintain their state
    #[clap(long)]
    pub pallets: Option<Vec<String>>,
}

impl fmt::Display for Chain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Chain::Dev => write!(f, "dev"),
            Chain::Other(c) => write!(f, "{c}"),
        }
    }
}

impl FromStr for Chain {
    type Err = Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_lowercase().as_str() {
            "dev" => Chain::Dev,
            _ => Chain::Other(s.to_owned()),
        })
    }
}

fn make_spinner_frame(pos: isize, bounce_width: isize, bar_width: isize) -> String {
    let mut bounce = String::from("[");
    for p in 0..bar_width.max(2) - 2 {
        if p >= pos && p < pos + bounce_width {
            bounce.push('=');
        } else {
            bounce.push(' ');
        }
    }
    bounce.push(']');
    bounce
}

fn make_spinner(bounce_width: usize, bar_width: usize) -> Vec<String> {
    let mut steps = Vec::new();

    let bounce_width: isize = bounce_width.try_into().unwrap();
    let bar_width: isize = bar_width.try_into().unwrap();
    let bar_content_width: isize = bar_width.max(2) - 2;

    let mut pos: isize = -bounce_width;
    while pos < bar_content_width {
        steps.push(make_spinner_frame(pos, bounce_width, bar_width));
        pos += 1;
    }
    while pos > -bounce_width {
        steps.push(make_spinner_frame(pos, bounce_width, bar_width));
        pos -= 1;
    }

    steps
}

pub struct ProgressBarManager {
    last_update: Instant,
    tick_interval: Duration,
    count: u64,
    progress: ProgressBar,
}

const SPINNER_TICK_INTERVAL: Duration = Duration::from_millis(100);
const BAR_TICK_INTERVAL: Duration = Duration::from_millis(100);

impl ProgressBarManager {
    pub fn new_spinner(msg: impl Into<Cow<'static, str>>) -> Result<Self> {
        let spinner_strings = make_spinner(10, 40);
        let spinner_strs = spinner_strings
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();

        let progress = ProgressBar::new_spinner()
            .with_style(
                ProgressStyle::with_template(
                    "{msg:23.green} {spinner} [{elapsed:.cyan}] ({per_sec:>12.magenta})",
                )?
                .tick_strings(spinner_strs.as_slice()),
            )
            .with_message(msg);

        Ok(Self {
            last_update: Instant::now(),
            tick_interval: SPINNER_TICK_INTERVAL,
            count: 0,
            progress,
        })
    }

    pub fn new_bar(length: u64, msg: impl Into<Cow<'static, str>>) -> Result<Self> {
        let progress = ProgressBar::new(length)
            .with_style(
                ProgressStyle::with_template(
                    "{msg:.green} [{bar:38}] {human_pos:>7.blue}/{human_len:7.blue} [{elapsed:.cyan}] ({per_sec:.magenta})",
                )?
                .progress_chars("=> "),
            ).with_message(msg);

        Ok(Self {
            last_update: Instant::now(),
            tick_interval: BAR_TICK_INTERVAL,
            count: 0,
            progress,
        })
    }

    pub fn inc(&mut self, amount: u64) {
        self.count += amount;
        if self.last_update.elapsed() >= self.tick_interval {
            self.progress.inc(self.count);
            self.count = 0;
            self.last_update = Instant::now();
        }
    }

    pub fn finish_with_message(self, msg: impl Into<Cow<'static, str>>) {
        self.progress.finish_with_message(msg);
    }
}
