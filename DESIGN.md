# Design notes

Decisions that are already made, and the reasoning that produced them. Nothing
here is a task; anything still to be done lives in [BACKLOG.md](BACKLOG.md) or
in the issue tracker.

The point of the file is the question nobody can answer from the code: *why
not*. A closed issue is unfindable a year later, and "we considered X and
rejected it because Y" is exactly what somebody reaching for X needs to read
first. The measurements are here for the same reason — the next dependency
argument should start from numbers somebody actually took.

## Decided against

- **Shelling out from *rules*.** Killed the `command = [...]` escape hatch on
  a rule, and it cost nothing: the URL table it existed to reach just lives in
  the Lua config now. Handlers stay fast enough that the stale-rewrite race
  never opens for text.

  What this does *not* buy is a config you can run unread. `notify_command` is
  still a program the config names, with rule output as an argument — that is
  the feature, and the trust boundary is therefore the file, not the
  interpreter. Said plainly in the README and `clipmunge(1)` rather than
  implied away.
- **Rule chaining.** First match wins, in declaration order. Chaining reads
  well right up until two rules feed each other, and the marker MIME cannot
  catch a loop that happens inside one pass.
- **A declarative TOML rule format.** It had already grown `when`, `command`,
  per-MIME templates and escaping rules before a line was written — a bad
  programming language in a config file. Lua is a good one.
- **A `zwlr_data_control_v1` fallback.** The two protocols are identical in
  shape, so this is cheap — a trait over both, or a macro. It would also be
  the difference between running and not running on Debian 13 (sway 1.10.1)
  and Ubuntu 24.04 LTS (sway 1.9), which is not a small audience. Declined
  anyway: ext-data-control is the standard, those releases are already being
  superseded, and every protocol carried twice is carried for years. The
  requirement is stated in the README instead, and the daemon says so on
  startup rather than failing obscurely.
- **Trimming `regex` in favour of Lua patterns.** Lua patterns have no
  alternation and no bounded quantifiers, so `D\d{6,9}` is inexpressible. The
  crate costs 917 KB with unicode trimmed to `\d`/`\w`/`\s` and case folding,
  and it cannot backtrack, so a hostile pattern in a third-party rule set
  cannot hang the daemon.

## Other MIME types: what a second flavour would cost

Text and HTML only, for now. The read path is already bytes-and-MIME rather
than a string, so a new flavour does not need the core rewritten — that seam
was cut on purpose.

Use cases worth having, in order of obviousness:

- strip EXIF/GPS from a copied image, the exact analogue of dropping `utm_*`
- re-encode a 20 MB PNG screenshot into something pasteable
- downscale to a maximum long edge

Measured costs, on a release build with LTO:

| addition                                  | binary | delta   |
| ----------------------------------------- | ------ | ------- |
| baseline (empty binary)                   | 289 KB | —       |
| `img-parts` (EXIF surgery, no decode)     | 297 KB | +8 KB   |
| `image` with png+jpeg+webp, decode+encode | 957 KB | +668 KB |

Cheaper than `regex`, so no need for a cargo feature. Stripping EXIF is
essentially free because it walks JPEG APP segments and PNG chunks rather than
decoding anything, which makes it a reasonable default for everyone.

What images break that text does not:

- **Size caps must become per-type.** `READ_LIMIT` is one number today. Text
  wants tens of KB, an image wants tens of MB, and a decoded 4K RGBA buffer is
  33 MB on its own.
- **The generation counter is already load-bearing.** Decode, resize and
  re-encode of a 1920x1080 PNG measured 150 ms. That is long enough to copy
  something else, and publishing a rewrite of the previous clipboard is worse
  than doing nothing. It fires today because `drain_events` reads the socket
  before the check; it used to be dead code, because nothing dispatched
  between taking the generation and comparing it, so the two could not
  differ. Anything added to the slow path keeps that drain in front of the
  publish, and `tick` has to keep skipping its sleep while `got_selection` is
  set, or the selection the drain picked up waits for an unrelated event.
- **Pixels stay in Rust.** Lua is policy; `clipmunge.image.*` does the work.
  Decoding a PNG in a sandboxed interpreter is not a plan.
- The `image` crate's WebP encoder is **lossless only** — 730 KB against JPEG's
  96 KB on the same frame. Lossy WebP needs libwebp over FFI and gives up the
  pure-Rust build.

## What the secret hint does not cover

`secret_mimes` skips a selection whose owner advertises
`x-kde-passwordManagerHint`. Worth having and nearly free, but it is a
courtesy protocol and the measurements are not encouraging — on Firefox 154,
copying from `about:logins` sets the hint; copying out of an
`<input type=password>` does not, and neither does the 1Password browser
extension, which is how most people actually put a password on the clipboard.

Nothing better is available. Guessing at content — "this looks like a
password" — is not a plan: the false positives are silent and the false
negatives are the ones that matter. The real property is that a rule only
fires on a match, and the shipped rules want a URL or a bare identifier.

So the honest statement is the one in the README: this is a second line. Do
not let it grow into a claim that clipmunge knows what a password is.

## Domain-blind tracker list

`strip_params` matches parameter names with no idea what host they sit on, so
a short entry means what it means everywhere. `si` is Spotify and YouTube;
`spm` is Alibaba; `ref_src` is a Twitter embed. On a site that uses one of
those names for something real, the rule quietly drops it.

ClearURLs solves this with per-domain rule sets, which is a data file, a
matcher and a maintenance burden. Not obviously worth it here: the config is
Lua, so a rule that cares passes its own list, and the pattern in `match`
already scopes a rule to a host if written that way. Revisit when somebody
turns up with a URL the default list breaks.
