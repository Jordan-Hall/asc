//! `count-objects` (SPEC §11.2): asc keeps no loose objects, packs or garbage in a git object
//! directory (objects live in `store/` and `cache/`), so every count is zero. The
//! output has git's shape so scripts that parse it keep working; `asc doctor` and
//! `asc doctor --report-reclaimable` report real sizes. Exit 0.

use std::ffi::OsString;
use std::process::ExitCode;

use super::common::{Ctx, Result};
use super::refs_plumbing::args::{Cursor, Word};
use crate::native::NativeSpec;

const USAGE: &str = "usage: git count-objects [-v] [-H | --human-readable]

    -v, --[no-]verbose    be verbose
    -H, --[no-]human-readable
                          print sizes in human readable format
";

#[derive(clap::Parser, Debug)]
#[command(
    name = "count-objects",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub rest: Vec<OsString>,
}

pub const NATIVE: &[NativeSpec] = &[NativeSpec {
    cmd: "count-objects",
    flags: &[
        "-v",
        "--verbose",
        "--no-verbose",
        "-H",
        "--human-readable",
        "--no-human-readable",
    ],
    placeholders: &[],
}];

pub async fn run(_ctx: &Ctx, a: Args) -> Result<ExitCode> {
    let mut cur = Cursor::new(&a.rest, USAGE);
    let (mut verbose, mut human) = (false, false);
    while let Some(word) = cur.next() {
        match word {
            Word::End => {
                if cur.remaining() > 0 {
                    return Err(cur.usage_only());
                }
            }
            Word::Operand(_) => return Err(cur.usage_only()),
            Word::Short(flags) => {
                for c in flags.chars() {
                    match c {
                        'v' => verbose = true,
                        'H' => human = true,
                        other => {
                            return Err(cur.usage_error(&format!("unknown switch `{other}'")));
                        }
                    }
                }
            }
            Word::Long(name, _) => match name.as_str() {
                "verbose" => verbose = true,
                "no-verbose" => verbose = false,
                "human-readable" => human = true,
                "no-human-readable" => human = false,
                other => return Err(cur.usage_error(&format!("unknown option `{other}'"))),
            },
        }
    }
    let size = if human { "0 bytes" } else { "0" };
    if verbose {
        println!("count: 0");
        println!("size: {size}");
        println!("in-pack: 0");
        println!("packs: 0");
        println!("size-pack: {size}");
        println!("prune-packable: 0");
        println!("garbage: 0");
        println!("size-garbage: {size}");
    } else if human {
        println!("0 objects, {size}");
    } else {
        println!("0 objects, 0 kilobytes");
    }
    Ok(ExitCode::SUCCESS)
}
