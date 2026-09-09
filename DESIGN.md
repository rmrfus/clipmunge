# Design notes

## Rule engine

Lua supports table lookups and custom URL transformations without a separate
template language. Rules run in declaration order; the first valid replacement
wins. Rules do not chain.

The config is trusted because it can set `notify_command`. The interpreter
excludes `io`, `os`, `debug` and `coroutine`, disables C module loading, and
sets the default Lua module search path to the config directories. Libraries must be excluded
at state creation: deleting globals leaves them accessible in `package.loaded`.
Coroutines are excluded because the instruction hook covers only its thread.

Instruction and memory limits constrain accidental runaway code. They do not
make arbitrary configs safe to install. Notifications run outside Lua, after
a rewrite is published, with output substituted into arguments without a shell.

## Clipboard handling

`ext-data-control-v1` lets each MIME type supply different bytes, so plain text
and HTML can represent the same selection differently. The older
`wlr-data-control` protocol is not supported to avoid maintaining two backends.

Selections store bytes by MIME type. Advertised order is fixed: plain text,
HTML, source URL, URI list, RTF, then other types by name. Some clients choose
the first supported type; Lua table iteration order is not stable between runs.

The private `application/x-clipmunge` MIME marks our output. Check it before
reading: reading our own offer would wait for a send event on the blocked
queue. Offers carrying a configured secret MIME are also skipped before reads.
The secret hint is optional for source applications and cannot identify all
passwords.

Before publishing, drain pending events and compare the selection generation
to discard stale results. Process a selection discovered by that drain before
sleeping again. Reads have per-flavour and per-selection deadlines. Writes
stop waiting for pipe capacity at their deadline; this is not a total runtime
limit if a client keeps accepting data.

## URL matching

Rust regex supports alternation and bounded repetition with linear-time
matching. Lua patterns lack those operators. Unicode features include
`\d`, `\w`, `\s` and case folding; script classes are disabled.

`strip_params` edits the query without parsing the rest of the URL. It splits
off the fragment first to preserve hash routes containing `?`. Parameter names
are matched without percent-decoding; empty query segments are removed when
a matching parameter is dropped.

The default tracker list applies to every domain. Short keys such as `si` or
`spm` can also be legitimate application parameters. Rules can supply their
own list and restrict their match to a host. Domain-specific defaults can be
added if concrete failures justify maintaining them.

## Recorded measurements

Historical measurements recorded in the pre-cleanup tree
[`26f4c58`](https://github.com/rmrfus/clipmunge/tree/26f4c58134a1dffaa94386de904609784f55eebb).
Sizes retain the original KB units. These are different experiments, not
additive costs; exact toolchains and image fixtures were not recorded.

| Measurement | Result | Context |
| --- | --- | --- |
| `regex` / `clap` | 917 KB / 347 KB | Reported contributions to clipmunge; regex Unicode features trimmed |
| `img-parts` | +8 KB (289 → 297 KB) | Empty-binary comparison, release with LTO; metadata edits without decoding |
| `image` | +668 KB (289 → 957 KB) | Same baseline; PNG, JPEG and WebP decode/encode |
| `codegen-units = 1` | −191 KB (3402 → 3211 KB) | clipmunge release binary; no build-time or runtime measurement |
| WebP lossless / JPEG | 730 KB / 96 KB | Encoded output sizes for the same frame |
| PNG decode, resize, encode | 150 ms | 1920×1080 input |

The clap feature comparison recorded 3,211,320 bytes with defaults and
3,211,000 without `color`: a 320-byte saving, since env_logger still needed
anstream. Also dropping `suggestions` saved 18,280 bytes against defaults.
Restoring regex Unicode script classes was recorded as roughly +250 KB.

For new measurements, record the revision, toolchain, features and build
command alongside the result. Compare equivalent Nix builds as described in
[development instructions](docs/development.md#dependency-size).

## Possible image support

The byte-based selection representation can carry images. Current reads cover
plain and rich text, and rule patterns match only plain text. Image support would need:

- MIME-based rule matching and reads for image types.
- Separate compressed and decoded size limits.
- Rust helpers for EXIF removal, resizing and encoding.
- Handling for processing delays without publishing stale results.

Image dependency choices and performance should be measured when this work
is implemented.
