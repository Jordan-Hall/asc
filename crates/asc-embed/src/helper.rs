//! `asc-embed` helper: serves the frame protocol on stdio.
//!
//! Usage: `asc-embed <model-name> <model-dir>`. The directory holds `model.onnx`,
//! `tokenizer.json`, `config.json`, `special_tokens_map.json` and `tokenizer_config.json`.

use asc_embed::{Error, FastEmbedder, ModelFiles, Registry, protocol};
use std::path::Path;

fn read(dir: &Path, name: &str) -> Result<Vec<u8>, Error> {
    Ok(std::fs::read(dir.join(name))?)
}

fn run() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [name, dir] = args.as_slice() else {
        return Err(Error::Protocol("usage: asc-embed <model-name> <model-dir>".to_owned()));
    };
    let record = Registry::builtin().get(name)?.clone();
    let dir = Path::new(dir);
    let files = ModelFiles {
        onnx: read(dir, "model.onnx")?,
        tokenizer: read(dir, "tokenizer.json")?,
        config: read(dir, "config.json")?,
        special_tokens: read(dir, "special_tokens_map.json")?,
        tokenizer_config: read(dir, "tokenizer_config.json")?,
    };
    let mut embedder = FastEmbedder::load(&record, files)?;
    protocol::serve(&mut embedder, &mut std::io::stdin().lock(), &mut std::io::stdout().lock())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("asc-embed: {e}");
        std::process::exit(1);
    }
}
