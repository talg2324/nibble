Tree-sitter queries vendored from Helix (https://github.com/helix-editor/helix) tag `25.07.1`,
`runtime/queries/{bash,json,yaml,toml,markdown,markdown.inline,html}`: the languages nib
ships with. MPL-2.0, © Helix contributors. Embedded into `nib` at build time by
`crates/nib-syntax`. Update together with the helix-core tag in the workspace `Cargo.toml` and
`nib_syntax::HELIX_TAG`. Other languages are installed at runtime with `nib grammar add`.

