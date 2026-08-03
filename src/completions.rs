//! Shell completion: the zsh function, and getting it switched on.
//!
//! The function is hand-written and ships inside the binary as a string constant.
//! A generator would be a tenth direct dependency and still could not produce the
//! candidate lists worth having — downloaded papers, the archive's categories,
//! author names, saved watches — which are the only parts of this command line
//! anyone needs help typing.

use crate::{db, library_listing, names, watches, AUTHOR_MATCHES};
use anyhow::{bail, Context, Result};
use std::io::IsTerminal;
use std::path::PathBuf;

/// Hand-written rather than generated: `clap_complete` would be a tenth dependency
/// and still could not offer the downloaded papers, which is the only part worth
/// completing. `_describe` splits each entry on its *first* colon, so a title
/// containing one is safe.
const ZSH_COMPLETION: &str = r#"# eprint zsh completion. Install with:
#   echo 'eval "$(eprint completions zsh)"' >> ~/.zshrc
#
# Bootstraps zsh's completion system if the shell has not already done so. A bare
# .zshrc has no compinit, which leaves `compdef` undefined and Tab doing nothing
# anywhere — not just for eprint. `-i` skips insecure completion files rather than
# trusting them, so this stays quiet without lowering the bar.
if ! whence compdef > /dev/null 2>&1; then
  autoload -Uz compinit && compinit -i
fi

_eprint() {
  # zsh matches candidates against the typed text case-sensitively unless told
  # otherwise, which would make completion the only case-sensitive thing in the
  # tool: `--author shamir` and `--category crypto` are perfectly good filters, so
  # they should also be perfectly good things to type at a Tab.
  local -a cmds papers values
  local -a nocase
  nocase=(-M 'm:{a-zA-Z}={A-Za-z}')
  cmds=(
    'browse:Interactive full-screen browser'
    'open:Open a paper PDF'
    'show:Show one paper in full'
    'watch:Saved searches that mark papers'
    'bib:Citation keys from CryptoBib'
    'status:Index statistics'
    'update:Refresh the local index'
    'config:Show or create the configuration file'
  )
  if (( CURRENT == 2 )); then
    _describe -t commands 'eprint command' cmds $nocase
    return
  fi
  local cur=${words[CURRENT]} flag=${words[CURRENT-1]}
  # `--flag=value` reaches us as a single word, so strip the prefix and complete
  # the value. `compset -P` moves the matched part into IPREFIX, which keeps it in
  # the line when a match is inserted.
  case $cur in
    --*=*)
      flag=${cur%%=*}
      compset -P "${flag}="
      cur=
      ;;
  esac

  # Values for the flags with a closed, knowable set. Before the per-command arms,
  # because --category is taken by searches, browse and `watch add` alike and the
  # answer is the same in all three.
  case $flag in
    --category)
      values=(${(f)"$(eprint completions categories 2>/dev/null)"})
      (( ${#values} )) && _describe -t categories 'IACR category' values $nocase
      return
      ;;
    --author)
      # Filtered by what has been typed, because the whole list is 21,000 names.
      # The tool offers each match twice, as the full name and as the surname, so
      # plain prefix completion works whichever end you start from — and zsh
      # escapes the space in a full name for you, which is the trap this removes.
      # (`compadd -U` was tried, to insert a substring match: it appends to the
      # typed text instead of replacing it, and a matcher spec mangles candidates
      # containing spaces. Two candidates and no magic beats both.)
      local -a names
      names=(${(f)"$(eprint completions authors ${(Q)cur} 2>/dev/null)"})
      if (( ${#names} )); then
        _describe -t authors 'author' names $nocase
      else
        # `-r` because a bare `_message` is swallowed here and never shown.
        _message -r 'a few more letters of the name'
      fi
      return
      ;;
    --scope)
      _values $nocase 'scope' 'all[title, authors and abstract]' 'title[titles and authors only]'
      return
      ;;
    --theme)
      _values $nocase 'theme' 'auto[follow the terminal]' 'dark' 'light' 'mono[attributes only]'
      return
      ;;
  esac

  # A word being typed as a flag. Without this, `--categ<TAB>` and even a complete
  # `--category<TAB>` (no trailing space yet) did nothing at all, which reads as
  # broken completion rather than as "add a space". These lists mirror the flags
  # clap shows in --help, so they must be updated alongside it; deliberately
  # hidden flags stay out, since completion is a discovery surface too.
  if [[ $cur == -* ]]; then
    local -a flags
    local search_flags=(
      '-n[maximum results]' '--limit[maximum results]'
      '--date[date or range, e.g. 2023..2024]'
      '--author[filter by author name]' '--category[filter by IACR category]'
      '-t[titles and authors only]' '--title[titles and authors only]'
    )
    case ${words[2]} in
      open) flags=('--rm[delete downloaded copies]') ;;
      show|status) ;;
      browse) flags=($search_flags) ;;
      bib) flags=(
        '--entry[print the full BibTeX record]'
        '--update[download or refresh CryptoBib]'
        '--force[re-download even if unchanged]'
      ) ;;
      update) flags=('--full[re-harvest everything]' '--quiet[suppress progress]') ;;
      config) flags=(
        '--init[write a default config file]'
        '-e[open the config in $EDITOR]' '--edit[open the config in $EDITOR]'
        '--completions[switch on Tab completion]'
      ) ;;
      watch)
        case ${words[3]} in
          add) flags=(
            '--author[watch an author]' '--category[watch an IACR category]'
            '-t[titles and authors only]' '--title[titles and authors only]'
          ) ;;
          rm) flags=('--all[remove every watch]') ;;
        esac
        ;;
      # A bare `eprint`, a query, or the hidden `search`: the feed's own flags.
      *) flags=($search_flags '-a[include full abstracts]' '--abstracts[include full abstracts]') ;;
    esac
    (( ${#flags} )) && _values $nocase 'option' $flags
    return
  fi
  case ${words[2]} in
    open|show|bib)
      papers=(${(f)"$(eprint completions ids 2>/dev/null)"})
      (( ${#papers} )) && _describe -t papers 'downloaded papers' papers $nocase
      ;;
    watch)
      if (( CURRENT == 3 )); then
        _values $nocase 'watch command' 'add[save a search]' 'rm[remove one]' 'list[show them]'
      elif [[ ${words[3]} == rm ]]; then
        # `watch rm` takes the position `eprint watch` prints, and those numbers
        # renumber after a removal — so they are worth showing with their labels
        # rather than counted by hand.
        values=(${(f)"$(eprint completions watches 2>/dev/null)"})
        (( ${#values} )) && _describe -t watches 'saved watch' values $nocase
      fi
      ;;
  esac
}

# One candidate per row. zsh compacts a listing by putting every match that shares
# a description on one line and printing the description once — `_describe` checks
# `list-grouped`, which is true unless it is told otherwise. That is the wrong
# trade here: every candidate carries a count worth reading, so eleven authors with
# one paper each came out as three crowded rows with a single "1 paper" at the far
# right, while the authors whose counts happened to be unique lined up neatly.
# Scoped to this command, so every other completion keeps zsh's default.
zstyle ':completion:*:*:eprint:*' list-grouped false

compdef _eprint eprint
"#;

pub(crate) fn do_completions(what: &str, needle: Option<&str>) -> Result<()> {
    match what {
        "zsh" => print!("{ZSH_COMPLETION}"),
        "ids" => {
            for (id, title, _) in library_listing() {
                // `id:title`, the shape `_describe` expects.
                println!("{id}:{title}");
            }
        }
        // The other value sets small enough to be worth offering whole. Both are
        // read from live data, so neither can go stale against a release.
        "categories" => {
            let conn = db::open()?;
            for (name, n) in db::categories(&conn)? {
                println!("{name}:{n} papers");
            }
        }
        "watches" => {
            let conn = db::open()?;
            // The number is the position `eprint watch` prints, which is what
            // `watch rm` takes; the description reads the way the list does.
            for (i, w) in watches(&conn).iter().enumerate() {
                println!("{}:{}", i + 1, w.describe());
            }
        }
        // Unlike the others this one is filtered, because the unfiltered answer is
        // 21,000 names. A name containing a colon would split wrong in `_describe`,
        // so it is dropped rather than shown mangled — no author in the archive has
        // one, and a name that did would be broken metadata.
        "authors" => {
            let conn = db::open()?;
            for c in names::authors_matching(&conn, needle.unwrap_or(""), AUTHOR_MATCHES)? {
                if c.value.contains(':') {
                    continue;
                }
                // Naming the person keeps two candidates from sharing a
                // description, which `_describe` would pack onto one row — two
                // names on one line reads as one mangled entry.
                let plural = if c.papers == 1 { "" } else { "s" };
                match c.person.is_empty() {
                    true => println!("{}:{} paper{plural}", c.value, c.papers),
                    false => println!("{}:{} paper{plural} · {}", c.value, c.papers, c.person),
                }
            }
        }
        other => bail!(
            "unknown completion target {other:?} — \
             try `zsh`, `ids`, `categories`, `watches` or `authors`"
        ),
    }
    Ok(())
}

/// The line a shell needs to load the completion function, and where it goes.
/// Cargo has no post-install hook, so someone has to put it there; this makes it
/// one command instead of an editor session.
pub(crate) const COMPLETION_LINE: &str = r#"eval "$(eprint completions zsh)"   # eprint Tab completion"#;

pub(crate) fn rc_path() -> Option<PathBuf> {
    let dir = std::env::var("ZDOTDIR")
        .ok()
        .map(PathBuf::from)
        .or_else(dirs::home_dir)?;
    Some(dir.join(".zshrc"))
}

/// Has the line already been added? Matched loosely, so a hand-written variant
/// still counts and nothing is ever appended twice.
pub(crate) fn completions_installed() -> bool {
    rc_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| t.contains("eprint completions zsh"))
        .unwrap_or(false)
}

pub(crate) fn install_completions() -> Result<()> {
    let shell = std::env::var("SHELL").unwrap_or_default();
    if !shell.ends_with("zsh") {
        bail!(
            "only zsh completion exists so far, and $SHELL is {:?}.\n       \
             The function itself is `eprint completions zsh` if you want to adapt it.",
            shell
        );
    }
    let path = rc_path().context("could not determine your home directory")?;
    if completions_installed() {
        println!("\n  already set up in {}\n", path.display());
        return Ok(());
    }
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(COMPLETION_LINE);
    text.push('\n');
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    println!(
        "\n  added one line to {}\n  open a new shell, then `eprint open <TAB>`\n",
        path.display()
    );
    Ok(())
}

/// Mentioned once, ever, and only to someone who could act on it: a hint that
/// repeats is nagging, and one that appears in a pipe is noise.
pub(crate) fn nudge_completions(conn: &rusqlite::Connection) {
    const KEY: &str = "completions_hint";
    if !std::io::stderr().is_terminal()
        || !std::env::var("SHELL").unwrap_or_default().ends_with("zsh")
        || completions_installed()
        || db::meta_get(conn, KEY).unwrap_or(None).is_some()
    {
        return;
    }
    let _ = db::meta_set(conn, KEY, "shown");
    eprintln!("tip: `eprint config --completions` switches on Tab completion for paper ids");
}
