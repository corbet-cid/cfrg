//! `cfrg contents`: read named root files of repository branches through the
//! forge API. The query is one JSON document on standard input; one JSON line
//! per target goes to standard output, in request order.
use cfrg::{
    contents::{self, ContentsSource, Query},
    land::Capability,
    model::Forge,
    native::{
        http::{Http, Pacing, Request, Response, Transport},
        Endpoint,
    },
    Result,
};
use clap::Args;
use std::{
    io::{Read, Write},
    path::PathBuf,
    time::Duration,
};

/// Largest query accepted on standard input.
const MAX_QUERY: u64 = 32 * 1024 * 1024;

#[derive(Args)]
pub struct Options {
    #[arg(long, value_enum)]
    forge: Forge,
    /// HTTPS origin of the forge: host and optional port only.
    #[arg(long)]
    origin: String,
    /// Environment variable that holds the API token.
    #[arg(long)]
    token_env: String,
    /// Rate-window state; a fresh temporary directory when omitted.
    #[arg(long, env = "CFRG_CONTENTS_STATE_DIR")]
    state_dir: Option<PathBuf>,
    /// Milliseconds between two requests: a scan of hundreds of repositories
    /// must stay under the forge's burst limit.
    #[arg(long, default_value_t = 100)]
    gap_ms: u64,
}

/// Exit code 0 when every target was read, 2 when some were not.
pub fn run(options: Options) -> Result<u8> {
    let source: &dyn ContentsSource = match options.forge {
        Forge::Forgejo => &cfgj::contents::SOURCE,
        Forge::Gitlab => return unsupported(cglb::CONTENTS),
        Forge::Bitbucket => return unsupported(cbkt::CONTENTS),
        Forge::Github => return unsupported(cghb::CONTENTS),
    };
    let mut input = Vec::new();
    std::io::stdin()
        .take(MAX_QUERY + 1)
        .read_to_end(&mut input)?;
    if input.len() as u64 > MAX_QUERY {
        return Err("Contents query too large".into());
    }
    let query: Query = serde_json::from_slice(&input)?;
    let endpoint = Endpoint {
        origin: options.origin,
        token_env: options.token_env,
    };
    let mut opened = open(options.state_dir)?;
    let mut out = std::io::stdout().lock();
    let mut spaced = Spaced {
        inner: &mut opened.http,
        gap: Duration::from_millis(options.gap_ms),
    };
    let failed = contents::read(source, &mut spaced, &endpoint, &query, &mut |report| {
        writeln!(out, "{}", serde_json::to_string(report)?)?;
        out.flush()?;
        Ok(())
    })?;
    Ok(if failed == 0 { 0 } else { 2 })
}

fn unsupported(capability: Capability) -> Result<u8> {
    Err(format!(
        "Contents is {:?} for this forge: {}",
        capability.support, capability.note
    )
    .into())
}

/// Leaves a gap after every request.
struct Spaced<'a> {
    inner: &'a mut dyn Transport,
    gap: Duration,
}

impl Transport for Spaced<'_> {
    fn send(&mut self, request: Request) -> Result<Response> {
        let response = self.inner.send(request);
        std::thread::sleep(self.gap);
        response
    }

    fn resolve(&mut self, scope: &str) -> Result<()> {
        self.inner.resolve(scope)
    }
}

/// The paced read transport and the directory that keeps its state.
pub(crate) struct Opened {
    pub http: Http,
    _scratch: Option<tempfile::TempDir>,
}

/// Reads only. The transport itself adds no spacing (whole seconds is too coarse
/// for a scan); `contents` leaves its own millisecond gap, and a recorded rate
/// window still refuses every later request. Without `state_dir` the windows
/// live in a temporary directory that disappears with the process.
pub(crate) fn open(state_dir: Option<PathBuf>) -> Result<Opened> {
    let (dir, scratch) = match state_dir {
        Some(dir) => {
            std::fs::create_dir_all(&dir)?;
            (dir, None)
        }
        None => {
            let scratch = tempfile::tempdir()?;
            (scratch.path().to_path_buf(), Some(scratch))
        }
    };
    let http = Http::open(&dir.join("http.json"), false)?.paced(Pacing {
        read: 0,
        write: 3,
        creation: 3,
    });
    Ok(Opened {
        http,
        _scratch: scratch,
    })
}
