//! `jrif` command-line tool.
//!
//! Subcommands:
//!   index <payload.json> [--out <path>] [--min-chunk-bytes N] [--jsonl] [--pretty]
//!   inspect <payload.json> [--jrif <path>]

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bytes::Bytes;
use jrif::{Index, Indexer};
use thiserror::Error;
use tokio::fs;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};

const EXIT_OK: u8 = 0;
const EXIT_RUNTIME: u8 = 1;
const EXIT_MISUSE: u8 = 2;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().collect();
    match run(args).await {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("jrif: {err}");
            ExitCode::from(err.exit_code())
        }
    }
}

async fn run(args: Vec<OsString>) -> Result<u8, CliError> {
    let mut iter = args.into_iter();
    let _argv0 = iter.next();
    let Some(sub) = iter.next() else {
        print_usage();
        return Ok(EXIT_MISUSE);
    };
    let sub = sub.to_string_lossy();
    let rest: Vec<OsString> = iter.collect();
    match sub.as_ref() {
        "index" => cmd_index(rest).await,
        "inspect" => cmd_inspect(rest).await,
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(EXIT_OK)
        }
        other => Err(CliError::Misuse(format!("unknown subcommand: {other}"))),
    }
}

fn print_usage() {
    let usage = "\
usage: jrif <command> [args]

commands:
  index <payload.json> [--out PATH] [--min-chunk-bytes N] [--jsonl] [--pretty]
        Read JSON from PAYLOAD (or '-' for stdin) and write a .jrif sidecar.
        Default --out is <payload>.jrif; with stdin, default is '-' (stdout).
        With --jsonl, parse PAYLOAD as JSONL (one JSON value per line). The
        emitted sidecar uses one `item` chunk per record, with ranges into
        the original payload bytes. The JSONL payload is not itself a valid
        JSON array, so its root range MUST NOT be fetched and parsed
        directly; consumers should navigate via the item chunks instead.

  inspect <payload.json> [--jrif PATH]
        Parse the .jrif sidecar (default <payload>.jrif) and report the
        jrif version tag plus the payload size.

  help  Show this message.
";
    eprint!("{usage}");
}

async fn cmd_index(args: Vec<OsString>) -> Result<u8, CliError> {
    let mut payload_arg: Option<OsString> = None;
    let mut out_arg: Option<OsString> = None;
    let mut min_chunk: Option<u64> = None;
    let mut pretty = false;
    let mut jsonl = false;

    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let a = arg.to_string_lossy();
        match a.as_ref() {
            "--out" | "-o" => {
                out_arg = Some(
                    it.next()
                        .ok_or_else(|| CliError::Misuse("--out requires a value".into()))?,
                );
            }
            "--min-chunk-bytes" => {
                let v = it
                    .next()
                    .ok_or_else(|| CliError::Misuse("--min-chunk-bytes requires a value".into()))?;
                min_chunk =
                    Some(v.to_string_lossy().parse::<u64>().map_err(|e| {
                        CliError::Misuse(format!("invalid --min-chunk-bytes: {e}"))
                    })?);
            }
            "--jsonl" => jsonl = true,
            "--pretty" => pretty = true,
            "-h" | "--help" => {
                print_usage();
                return Ok(EXIT_OK);
            }
            _ if (a == "-" || !a.starts_with('-')) && payload_arg.is_none() => {
                payload_arg = Some(arg);
            }
            other => {
                return Err(CliError::Misuse(format!("unexpected argument: {other}")));
            }
        }
    }

    let payload_arg = payload_arg.ok_or_else(|| {
        CliError::Misuse("index requires a payload path (use '-' for stdin)".into())
    })?;

    let (payload, source_label) = read_input(&payload_arg).await?;
    let mut indexer = Indexer::new();
    if let Some(n) = min_chunk {
        indexer = indexer.min_chunk_bytes(n);
    }
    let jrif: Bytes = if jsonl {
        indexer
            .build_jsonl(&payload)
            .map_err(|e| CliError::Runtime(format!("indexing {source_label}: {e}")))?
    } else {
        indexer
            .build(&payload)
            .map_err(|e| CliError::Runtime(format!("indexing {source_label}: {e}")))?
    };

    let serialized: Vec<u8> = if pretty {
        let v: serde_json::Value =
            serde_json::from_slice(&jrif).map_err(|e| CliError::Runtime(e.to_string()))?;
        serde_json::to_vec_pretty(&v).map_err(|e| CliError::Runtime(e.to_string()))?
    } else {
        jrif.to_vec()
    };

    let out_path = resolve_index_output(&payload_arg, out_arg.as_deref());
    write_output(out_path.as_deref(), &serialized).await?;
    Ok(EXIT_OK)
}

async fn cmd_inspect(args: Vec<OsString>) -> Result<u8, CliError> {
    let mut payload_arg: Option<OsString> = None;
    let mut jrif_arg: Option<OsString> = None;

    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let a = arg.to_string_lossy();
        match a.as_ref() {
            "--jrif" => {
                jrif_arg = Some(
                    it.next()
                        .ok_or_else(|| CliError::Misuse("--jrif requires a path".into()))?,
                );
            }
            "-h" | "--help" => {
                print_usage();
                return Ok(EXIT_OK);
            }
            _ if (a == "-" || !a.starts_with('-')) && payload_arg.is_none() => {
                payload_arg = Some(arg);
            }
            other => {
                return Err(CliError::Misuse(format!("unexpected argument: {other}")));
            }
        }
    }

    let payload_arg =
        payload_arg.ok_or_else(|| CliError::Misuse("inspect requires the payload path".into()))?;
    let payload_path = PathBuf::from(&payload_arg);
    let jrif_path = jrif_arg.map_or_else(|| sidecar_path(&payload_path), PathBuf::from);

    let payload_fut = async {
        fs::read(&payload_path)
            .await
            .map_err(|e| CliError::Runtime(format!("read {}: {e}", payload_path.display())))
    };
    let jrif_fut = async {
        fs::read(&jrif_path)
            .await
            .map_err(|e| CliError::Runtime(format!("read {}: {e}", jrif_path.display())))
    };
    let (payload_vec, jrif_bytes) = tokio::try_join!(payload_fut, jrif_fut)?;
    let payload: Bytes = payload_vec.into();

    let payload_len = payload.len();
    Index::open(&jrif_bytes, payload)
        .await
        .map_err(|e| CliError::Runtime(format!("open {}: {e}", jrif_path.display())))?;

    println!(
        "ok\tpayload={}\tjrif={}\tsize={}",
        payload_path.display(),
        jrif_path.display(),
        payload_len
    );
    Ok(EXIT_OK)
}

async fn read_input(arg: &OsString) -> Result<(Bytes, String), CliError> {
    if arg == "-" {
        let mut buf = Vec::new();
        io::stdin()
            .read_to_end(&mut buf)
            .await
            .map_err(|e| CliError::Runtime(format!("read stdin: {e}")))?;
        Ok((buf.into(), "<stdin>".to_owned()))
    } else {
        let path = PathBuf::from(arg);
        let bytes = fs::read(&path)
            .await
            .map_err(|e| CliError::Runtime(format!("read {}: {e}", path.display())))?;
        Ok((bytes.into(), path.display().to_string()))
    }
}

fn resolve_index_output(
    payload_arg: &OsString,
    out_arg: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if let Some(out) = out_arg {
        if out == "-" {
            return None;
        }
        return Some(PathBuf::from(out));
    }
    if payload_arg == "-" {
        return None;
    }
    Some(sidecar_path(Path::new(payload_arg)))
}

fn sidecar_path(payload: &Path) -> PathBuf {
    let mut s = payload.as_os_str().to_owned();
    s.push(".jrif");
    PathBuf::from(s)
}

async fn write_output(path: Option<&Path>, bytes: &[u8]) -> Result<(), CliError> {
    if let Some(p) = path {
        fs::write(p, bytes)
            .await
            .map_err(|e| CliError::Runtime(format!("write {}: {e}", p.display())))?;
        println!("{}", p.display());
    } else {
        let mut out = io::stdout();
        out.write_all(bytes)
            .await
            .map_err(|e| CliError::Runtime(format!("write stdout: {e}")))?;
    }
    Ok(())
}

#[derive(Debug, Error)]
enum CliError {
    #[error("{0}")]
    Runtime(String),
    #[error("{0}")]
    Misuse(String),
}

impl CliError {
    const fn exit_code(&self) -> u8 {
        match self {
            Self::Runtime(_) => EXIT_RUNTIME,
            Self::Misuse(_) => EXIT_MISUSE,
        }
    }
}
