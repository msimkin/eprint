# eprint

Fast command-line search over the [IACR Cryptology ePrint Archive](https://eprint.iacr.org).

Metadata for all ~26,000 papers is harvested from the archive's OAI-PMH interface into a
local SQLite FTS5 index, so searching is instant and works offline.

```
$ eprint "lattice signature" -n 3

3 of 590 results  (index: just now)

  2025/1492    Comment on On Gaussian sampling, smoothing parameter and application to
               lattice signatures
               Ling · Foundations · CC-BY-4.0 · 2025-08-19
               We show the key ideas of the above-referenced work for lattice Gaussian…

  2022/097     Lattice Signature can be as Simple as Lattice Encryption
               Ye, Xu, et al. · Public-key cryptography · CC-BY-4.0 · 2022-01-31
               Existing lattice signature schemes are much less efficient than encryption…
```

### Opening papers

On terminals that implement OSC 8 hyperlinks (iTerm2, Ghostty, kitty, WezTerm, VS Code,
recent GNOME Terminal), paper ids and titles are themselves clickable.

macOS **Terminal.app does not support OSC 8** — it only auto-detects plain text URLs. It is
detected automatically and a bare, cmd-clickable URL is printed under each result instead.
Force either behaviour with `--urls` / `--no-urls`.

`eprint open <id>` works everywhere regardless of terminal.

## Install

Requires a Rust toolchain (1.86 or newer). If you do not have one:
<https://rustup.rs>

```sh
git clone <repository-url> eprint
cd eprint
cargo install --path . --locked
```

That puts an `eprint` binary in `~/.cargo/bin`, which rustup already adds to your `PATH`,
so you can then run `eprint` from anywhere.

> **Use `--locked`.** `cargo install` ignores `Cargo.lock` unless you pass it. Without it
> Cargo re-resolves dependencies and pulls versions of `darling` and `instability` that
> require rustc 1.88, so the build fails on 1.86. The committed lockfile pins compatible
> versions. On rustc 1.88 or newer you can drop the flag, or run `cargo update` to lift the
> pins.

Check it worked:

```sh
eprint --version
```

The first search builds the local index automatically — about 30 seconds, once.

### Without installing

To run it from the build directory instead:

```sh
cargo build --release --locked
./target/release/eprint --help
```

### If `eprint` is not found

`~/.cargo/bin` is missing from your `PATH`. Add it to your shell's startup file:

```sh
# ~/.zshrc (zsh, the macOS default) or ~/.bashrc (bash)
export PATH="$HOME/.cargo/bin:$PATH"

# fish
fish_add_path ~/.cargo/bin
```

Then open a new shell. Alternatively, symlink the binary somewhere already on your `PATH`:

```sh
ln -s "$PWD/target/release/eprint" /usr/local/bin/eprint
```

### Updating and uninstalling

```sh
git pull && cargo install --path . --locked   # rebuild and replace
cargo uninstall eprint                        # remove the binary
```

Uninstalling leaves your index and config in place; see [Storage](#storage) to remove those
too.

## Use

```sh
eprint "threshold ecdsa"              # search
eprint search "threshold ecdsa"       # same thing, explicit
eprint "fully homomorphic" -a         # include full abstracts
eprint "Dan Boneh" -t                 # match titles and authors only
eprint zk --sort relevance            # best match first instead of newest
eprint garbled --year 2024 -n 50      # filter by year, more results
eprint --author Boneh --since 2y      # browse without a query
eprint --category "Public-key" -n 5   # filter by IACR category
eprint show 2026/1538                 # one paper in full
eprint open 2026/1538                 # open in browser
eprint open 2026/1538 --pdf           # straight to the PDF
eprint new                            # papers that arrived since you last looked
eprint new --since 30d --peek         # a window, without moving the marker
eprint status                         # index stats
eprint update                         # refresh now
eprint update --full                  # rebuild from scratch
```

## Interactive browser

```sh
eprint browse                      # everything, newest first
eprint browse "lattice signature"  # start from a query
eprint browse --author Boneh --since 2y
```

A full-screen browser for exploring rather than looking something up. Abstracts expand and
collapse in place, and matches stay highlighted inside the expanded text.

| Key | Action |
|---|---|
| `j` / `k`, arrows | move |
| `g` / `G` | first / last |
| `ctrl-d` / `ctrl-u`, page keys | jump |
| `space` / `tab` | expand or collapse the abstract |
| `a` | expand or collapse everything |
| `t` | toggle where the query is matched: `in: title, authors, abstract` ⇄ `in: title, authors` |
| `/` | edit the query — results filter live as you type |
| `ctrl-u` | clear the query (while editing) |
| `enter` / `o` | open the paper in your browser |
| `y` | copy the URL to the clipboard |
| `b` | copy the CryptoBib citation key (published version when known) |
| `B` | copy the full BibTeX record |
| `q` / `esc` | quit |

`/` refines the current query rather than replacing it, so you can narrow a search by
typing more terms; `ctrl-u` clears it to start fresh. Expansion is tracked per paper id, so
it survives re-searching.

Because nothing needs to be clicked, `browse` works identically on terminals without OSC 8
support — press `enter` instead.

## Keeping up

```sh
eprint new                     # arrivals since you last ran it
eprint new --since 30d         # an explicit window instead
eprint new --peek              # look without advancing the marker
```

Papers carry an `added` timestamp recording when they first entered *your* index, which is
what this filters on. A paper's own date can predate its arrival here, so filtering by that
would silently skip late-published submissions.

Unlike `search`, this refreshes the index synchronously when it is more than an hour old —
stale data is the entire failure mode for a "what's new" command.

## Citation keys (CryptoBib)

```sh
eprint bib --update      # download / refresh the CryptoBib database
eprint bib 2018/116      # citation key only
eprint Bib 2018/116      # the whole BibTeX record
eprint bib               # database status
```

`eprint Bib` (capital B) prints the complete record; `eprint bib <id> --entry` is the same
thing. The pairing mirrors `b` and `B` in the interactive browser.

Links each paper to its [CryptoBib](https://cryptobib.di.ens.fr/) citation key, preferring
the **published version** over the preprint where one is known:

```
$ eprint bib 2018/116

  EC:CGKW18
  published version
```

In `browse`, `b` copies the citation key and `B` copies the complete BibTeX record. The key
is shown in the meta line once an abstract is expanded, and `eprint show` displays it too.

```sh
eprint bib 2018/116 --entry >> refs.bib
```

Copied records are **self-contained**. Entries in `crypto.bib` reference venue names,
publishers and editors as `@String` macros defined in a separate `abbrev3.bib`, so a record
taken verbatim would not compile on its own. `--update` fetches both files and inlines the
definitions (resolving nested references and `#` concatenation). BibTeX's built-in month
macros are deliberately left unbraced so styles still render them properly.

**Staleness.** CryptoBib is never refreshed automatically. Once the local copy is more than
30 days old, `browse` shows `bib 45d old` in its header, copy confirmations append a
reminder, and `eprint bib` prints a note.

**Coverage.** Of 25,692 linked papers, **10,678 (42%) resolve to a published version**; the
rest fall back to their `EPRINT:` key, and papers absent from CryptoBib entirely fall back
to `cryptoeprint:YYYY/NNN`.

Matching is by exact normalised title plus at least one shared author surname. The author
check matters: without it, unrelated papers sharing a title get linked. Fuzzy title matching
was measured and rejected — it lifts coverage only from 42.8% to ~47.6% while introducing
false positives. The residual misses are mostly papers whose title changed between preprint
and publication, or whose published version is not in CryptoBib.

**Refreshing is cheap.** The server supports `ETag`, so an unchanged database returns 304
with an empty body and costs well under a second. A full rebuild downloads ~41 MB and takes
about 70 seconds. There is no automatic refresh — run `--update` when you want it.

CryptoBib is fetched at runtime and cached locally in your own database; nothing is
redistributed with this tool.

## Configuration

```sh
eprint config          # show the file location and effective settings
eprint config --init   # write a commented default file
```

Lives at `~/.config/eprint/config.toml` (override with `$EPRINT_CONFIG`):

```toml
theme = "auto"    # auto | dark | light | mono
sort  = "date"    # date | relevance
scope = "all"     # all | title
limit = 20
```

Results list authors only. The date joins them once an abstract is open (`space` in
`browse`, `-a` in `search`). Category and licence are shown by `eprint show`.

Command-line flags override the config file, which overrides the built-in defaults.

### Colour

The palette uses your terminal's own 16 ANSI colours rather than fixed RGB values, so it
inherits whatever theme you have configured. Two variants avoid the ends of the palette
that go unreadable:

- `dark` — bright cyan / silver / bright blue, avoiding ANSI blue 4 and dark-gray 8, which
  are near-invisible on a black background
- `light` — blue and dark gray, which would wash out on a dark background
- `mono` — no colour at all, only bold, dim, underline and reverse

Matches set both foreground *and* background (black on yellow), so highlighting keeps its
contrast under either variant.

`auto` reads `COLORFGBG` when the terminal sets it — rxvt and Konsole do, but macOS
Terminal.app and iTerm2 do not — and otherwise assumes a dark background. If that guess is
wrong, set `theme` explicitly; that is what the setting is for.

### Paging

Output longer than one screen is piped through a pager automatically. The default is
`less -RFX`, which keeps colour, exits immediately when the output already fits, and leaves
results in your scrollback instead of clearing the screen on exit.

Override with `$EPRINT_PAGER` or `$PAGER`; disable with `--no-pager`.

### Query syntax

Queries use SQLite FTS5 syntax with Porter stemming, so `signature` also matches
`signatures`. Multiple bare words are ANDed.

| Example | Meaning |
|---|---|
| `lattice signature` | both terms |
| `"fully homomorphic"` | exact phrase |
| `lattice NOT signature` | exclude a term |
| `zk OR snark` | either term |
| `homomorph*` | prefix match |

Punctuation that FTS5 would reject (`zero-knowledge`, `MPC (dishonest majority)!`) is
handled automatically — the query is retried with each term quoted.

**Partial words match.** The index matches whole tokens, so `bone` would not find `Boneh`
on its own. Bare terms are therefore treated as prefixes automatically: `bone` behaves as
`bone*`. Quoted phrases, operators, column filters and terms already ending in `*` are left
exactly as written. Pass `--exact` for strict whole-word matching.

### Sorting

Results are **newest first by default**. Pass `--sort relevance` for BM25 ranking, which
weights title matches above abstract matches — better for topical searches where you want
the seminal papers rather than the most recent ones.

`--author X` with no query terms is always date-sorted, since there is no relevance signal
to rank by. The active sort is always shown in the results header.

`browse` is always date-sorted; use `search --sort relevance` when you want ranking.

### Search scope

By default a query matches **title, authors and abstract**. `-t` / `--title` narrows it to
title and authors only — useful when searching for a person, where abstract matches are
mostly papers *citing* them rather than papers *by* them.

```sh
eprint "Dan Boneh"        # 109 hits, includes papers citing him
eprint "Dan Boneh" -t     # title and authors only
```

In `browse`, press `t` to toggle scope and watch the result count change live.

### Scripting

`--json` emits structured results; colour and hyperlinks are suppressed automatically
when output is piped.

```sh
eprint "verifiable delay" --json | jq -r '.[].url'
eprint zk --color | less -R        # force colour through a pager
```

## How the index stays current

Searching checks the index age. If it is more than 24 hours old, a background refresh is
spawned and your search returns immediately against existing data — it never blocks. An
incremental update takes about 0.1s. Use `--no-update` to suppress it, or `eprint update`
to refresh on demand.

Incremental harvests use the server's own `responseDate` as the watermark and re-request a
two-day overlap window, so records cannot be missed through clock skew. Withdrawn papers
arrive as OAI-PMH tombstones and are deleted from the index.

## Scope and licensing

Metadata comes from the archive's OAI-PMH endpoint, which the site publishes for exactly
this purpose. Search covers **title, authors, abstract and IACR category**. Author-supplied
keywords are not part of the `oai_dc` feed and so are not indexed; abstracts generally
contain the same terminology.

Full-text PDFs are licensed individually per paper and are deliberately not bulk
downloaded. `eprint open --pdf` hands the URL to your browser so the download happens in a
normal browser session under that paper's licence. Each result displays its licence
(`CC-BY-4.0`, `CC-BY-NC-ND-4.0`, `CC0`, …) so you can see the terms before opening.

The harvester identifies itself honestly by User-Agent, paces requests, and honours
`Retry-After` on 503/429.

## Storage

Everything lives in two places, both safe to delete — the index rebuilds from scratch.

| What | macOS | Linux |
|---|---|---|
| Index + citation keys | `~/Library/Application Support/eprint/eprint.db` | `$XDG_DATA_HOME/eprint/eprint.db` |
| Config | `~/.config/eprint/config.toml` | `~/.config/eprint/config.toml` |

The database is roughly 95 MB with metadata only, or ~105 MB once CryptoBib entries are
stored. Override the locations with `$EPRINT_DB` and `$EPRINT_CONFIG`.

## Development

```sh
cargo build --locked            # debug build
cargo build --release --locked  # optimised build
cargo clippy --all-targets      # lints
cargo fmt                       # formatting
```

Pass `EPRINT_DB=/tmp/scratch.db` to work against a throwaway index instead of your real one.

### Layout

| File | Responsibility |
|---|---|
| `src/main.rs` | CLI definition, command dispatch, config/flag resolution |
| `src/db.rs` | SQLite schema, migrations, FTS5 search and ranking |
| `src/harvest.rs` | OAI-PMH harvester for paper metadata |
| `src/bib.rs` | CryptoBib fetch, BibTeX parsing, `@String` macro expansion |
| `src/render.rs` | Inline output: wrapping, highlighting, hyperlinks |
| `src/tui.rs` | Interactive browser (ratatui) |
| `src/theme.rs` | Colour palettes shared by both front-ends |
| `src/config.rs` | Config file reading |

Two independent data sources feed one SQLite database: ePrint's OAI-PMH endpoint supplies
paper metadata (`papers`, plus an FTS5 index), and CryptoBib supplies citation keys
(`bib`). Neither is bundled with the tool; both are fetched at runtime into your own
database. Schema changes are applied by `migrate()` in `src/db.rs`, which runs on every
open, so existing databases upgrade in place.

## Licence

MIT — see [LICENSE](LICENSE).

The two data sources have their own terms, independent of this tool:

- **IACR Cryptology ePrint Archive** — papers are licensed individually; each result shows
  its licence. Metadata is taken from the OAI-PMH interface the archive publishes for that
  purpose.
- **[CryptoBib](https://cryptobib.di.ens.fr/)** — fetched at runtime and cached locally.
  Nothing from it is redistributed with this tool.
