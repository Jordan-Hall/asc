//! The verb registry of the stack CLI (SPEC §11.2, §20.7). A verb unit adds one file under
//! `src/stack/cli/` and registers it with [`verb!`]; `automod::dir!` declares the module and
//! the `linkme` slice [`VERBS`] collects the entry, so nothing here changes per verb.
//! `cli::find` and `cli::native_specs` read the slice next to the git command table.

use linkme::distributed_slice;

use crate::cli::Command;

/// Every registered stack-CLI verb (a `cli::Command`, so it routes through the same native
/// table as a git command).
#[allow(unsafe_code)] // linkme declares the slice with a link section.
#[distributed_slice]
pub static VERBS: [Command];

/// Registers the verb of the file it is called in: `verb!(SLOT, "word")`, where the file
/// defines `Args` (a clap parser), `NATIVE` and `async fn run(&Ctx, Args) -> Result<ExitCode>`.
#[allow(unused_macros)]
macro_rules! verb {
    ($slot:ident, $name:literal) => {
        #[allow(unsafe_code)] // linkme places the entry in a link section.
        #[::linkme::distributed_slice($crate::stack::cli::VERBS)]
        static $slot: $crate::cli::Command = $crate::cli::Command {
            name: $name,
            native: NATIVE,
            run: {
                fn go<'a>(
                    ctx: &'a $crate::cli::common::Ctx,
                    argv: ::std::vec::Vec<::std::ffi::OsString>,
                ) -> $crate::cli::BoxFut<'a> {
                    ::std::boxed::Box::pin(async move {
                        let args = <Args as ::clap::Parser>::try_parse_from(argv).map_err(|e| {
                            $crate::cli::common::CliError::Usage(e.render().to_string().trim_end().to_owned())
                        })?;
                        run(ctx, args).await
                    })
                }
                go
            },
        };
    };
}
#[allow(unused_imports)]
pub(crate) use verb;

automod::dir!("src/stack/cli");

#[allow(unused_imports)]
pub use registry::{find, problems, registered};
