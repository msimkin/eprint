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
    --venue)
      values=(${(f)"$(eprint completions venues 2>/dev/null)"})
      (( ${#values} )) && _describe -t venues 'venue' values $nocase
      return
      ;;
    --notify)
      values=(
        'off:no notifications'
        'all:one banner per new paper'
        'summary:one banner saying how many arrived'
        'watched:only papers matching a watch'
      )
      _describe -t modes 'notification mode' values $nocase
      return
      ;;
    --launcher)
      values=('on:add the desktop launcher' 'off:remove it')
      _describe -t states 'launcher' values $nocase
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
      '--venue[filter by publication venue]'
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
        '--notify[notify about new papers]'
        '--launcher[add or remove the desktop launcher]'
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

/// The same function for bash, which is what Ubuntu gives you.
///
/// Deliberately self-contained: no `_init_completion`, no
/// `_get_comp_words_by_ref`, because those live in the `bash-completion`
/// package and this should work without it. Written to bash 3.2 syntax — not
/// for macOS's sake, but because anything newer would fail to *parse* on a
/// shell that old, taking the whole `eval` down with it.
///
/// Bash has no description column, so the `value:description` lines the other
/// targets emit are cut at the first colon. That loses the paper counts zsh
/// shows; there is nowhere to put them.
const BASH_COMPLETION: &str = r##"
# eprint bash completion. Install with:
#   echo 'eval "$(eprint completions bash)"' >> ~/.bashrc

_eprint_offer() {
  # $1 newline-separated candidates, $2 the word being completed.
  #
  # Every entry put in COMPREPLY must begin with $2 *exactly*. readline works
  # out the longest common prefix of the matches case-sensitively and replaces
  # the typed word with it, so one candidate that differs in case is enough to
  # shorten the line instead of extending it: with the archive's all-caps
  # "DAMIEN COUROUSSE" in the list, typing `--author Dam` collapsed to `D`.
  local IFS=$'\n'
  local word esc pattern cur
  COMPREPLY=()
  # bash hands the word over exactly as it sits on the line, backslashes and
  # all. A name half-inserted as "Damien\ St" has to be unescaped before it can
  # match anything, or completion dies the moment a space is involved.
  cur=${2//\\/}

  # Exactly what was typed, case and all. The common case, and the one that
  # keeps a name's real spelling.
  for word in $(compgen -W "$1" -- "$cur"); do
    # Author names contain spaces. Unescaped, bash inserts the first word and
    # leaves the rest looking like a second argument.
    printf -v esc '%q' "$word"
    COMPREPLY[${#COMPREPLY[@]}]=$esc
  done
  if [ ${#COMPREPLY[@]} -gt 0 ]; then return; fi

  # Nothing matched as typed, so fold case — `--author shamir` and
  # `--category crypto` are perfectly good filters everywhere else in the tool
  # and should be here too. Each candidate's opening characters are rewritten
  # to what was actually typed, which is what keeps the invariant above true;
  # the filters fold case, so the value still finds the same papers.
  pattern=$(printf '%s' "$cur" | sed 's/[^a-zA-Z0-9_ -]/\\&/g')
  for word in $(printf '%s\n' "$1" | grep -i "^$pattern" 2>/dev/null); do
    printf -v esc '%q' "$cur${word:${#cur}}"
    COMPREPLY[${#COMPREPLY[@]}]=$esc
  done
}

_eprint() {
  local cur prev flag cmd sub vals search_flags
  cur=${COMP_WORDS[COMP_CWORD]}
  prev=${COMP_WORDS[COMP_CWORD-1]}
  COMPREPLY=()

  # `--flag=value` arrives split in three, because '=' is in COMP_WORDBREAKS:
  # the value is the current word and the flag is two back.
  flag=$prev
  if [ "$prev" = "=" ]; then
    flag=${COMP_WORDS[COMP_CWORD-2]}
  fi

  # Flag values before the per-command arms, because --category and --author are
  # taken by searches, browse and `watch add` alike and the answer is the same.
  case $flag in
    --category)
      vals=$(eprint completions categories 2>/dev/null | cut -d: -f1)
      _eprint_offer "$vals" "$cur"; return ;;
    --venue)
      vals=$(eprint completions venues 2>/dev/null | cut -d: -f1)
      _eprint_offer "$vals" "$cur"; return ;;
    --author)
      # Filtered by what has been typed: the whole list is 21,000 names.
      vals=$(eprint completions authors "$cur" 2>/dev/null | cut -d: -f1)
      _eprint_offer "$vals" "$cur"; return ;;
    --notify)
      _eprint_offer 'off
all
summary
watched' "$cur"; return ;;
    --launcher)
      _eprint_offer 'on
off' "$cur"; return ;;
    --scope)
      _eprint_offer 'all
title' "$cur"; return ;;
    --theme)
      _eprint_offer 'auto
dark
light
mono' "$cur"; return ;;
  esac

  cmd=${COMP_WORDS[1]}
  sub=${COMP_WORDS[2]}

  if [ "$COMP_CWORD" -eq 1 ]; then
    _eprint_offer 'browse
open
show
watch
bib
status
update
config' "$cur"
    return
  fi

  # A word being typed as a flag. These mirror the flags clap shows in --help,
  # so they must be updated alongside it; deliberately hidden flags stay out,
  # since completion is a discovery surface too.
  if [ "${cur:0:1}" = "-" ]; then
    search_flags='-n
--limit
--date
--author
--category
--venue
-t
--title'
    case $cmd in
      open) vals='--rm' ;;
      show|status) vals='' ;;
      browse) vals=$search_flags ;;
      bib) vals='--entry
--update
--force' ;;
      update) vals='--full
--quiet' ;;
      config) vals='--init
-e
--edit
--completions
--notify
--launcher' ;;
      watch)
        case $sub in
          add) vals='--author
--category
-t
--title' ;;
          rm) vals='--all' ;;
          *) vals='' ;;
        esac ;;
      # A bare `eprint`, a query, or the hidden `search`: the feed's own flags.
      *) vals="$search_flags
-a
--abstracts" ;;
    esac
    if [ -n "$vals" ]; then _eprint_offer "$vals" "$cur"; fi
    return
  fi

  case $cmd in
    open|show|bib)
      vals=$(eprint completions ids 2>/dev/null | cut -d: -f1)
      _eprint_offer "$vals" "$cur" ;;
    watch)
      if [ "$COMP_CWORD" -eq 2 ]; then
        _eprint_offer 'add
rm
list' "$cur"
      elif [ "$sub" = "rm" ]; then
        # `watch rm` takes the position `eprint watch` prints, and those numbers
        # renumber after a removal.
        vals=$(eprint completions watches 2>/dev/null | cut -d: -f1)
        _eprint_offer "$vals" "$cur"
      fi ;;
  esac
}

complete -F _eprint eprint
"##;

pub(crate) fn do_completions(what: &str, needle: Option<&str>) -> Result<()> {
    match what {
        "zsh" => print!("{ZSH_COMPLETION}"),
        "bash" => print!("{BASH_COMPLETION}"),
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
        "venues" => {
            let conn = db::open()?;
            // Ranked, not alphabetical: `db::venue_names` orders by standing in the
            // field, so the flagships are the first thing a menu shows. A colon can
            // never appear in a venue name, which is what `_describe` splits on.
            for (name, n) in db::venue_names(&conn)? {
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
             try `zsh`, `bash`, `ids`, `categories`, `venues`, `watches` or `authors`"
        ),
    }
    Ok(())
}

/// Which shell is running, as far as `$SHELL` admits. `None` for anything with
/// no completion function here — fish and friends.
pub(crate) fn shell_kind() -> Option<&'static str> {
    let shell = std::env::var("SHELL").unwrap_or_default();
    // Matched on the basename so `/usr/local/bin/zsh-5.9` and `-bash` both land.
    let name = shell.rsplit('/').next().unwrap_or_default().to_string();
    if name.contains("zsh") {
        Some("zsh")
    } else if name.contains("bash") {
        Some("bash")
    } else {
        None
    }
}

/// The line a shell needs to load the completion function. Cargo has no
/// post-install hook, so someone has to put it there; this makes it one command
/// instead of an editor session.
pub(crate) fn completion_line(kind: &str) -> String {
    format!(r#"eval "$(eprint completions {kind})"   # eprint Tab completion"#)
}

pub(crate) fn rc_path() -> Option<PathBuf> {
    match shell_kind()? {
        // `$ZDOTDIR` only means anything to zsh.
        "zsh" => {
            let dir = std::env::var("ZDOTDIR")
                .ok()
                .map(PathBuf::from)
                .or_else(dirs::home_dir)?;
            Some(dir.join(".zshrc"))
        }
        "bash" => Some(dirs::home_dir()?.join(".bashrc")),
        _ => None,
    }
}

/// Has the line already been added? Matched loosely — on the command rather than
/// the shell name — so a hand-written variant still counts, and switching shells
/// does not make an installed line invisible.
pub(crate) fn completions_installed() -> bool {
    rc_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| t.contains("eprint completions"))
        .unwrap_or(false)
}

pub(crate) fn install_completions() -> Result<()> {
    let shell = std::env::var("SHELL").unwrap_or_default();
    let Some(kind) = shell_kind() else {
        bail!(
            "there is a completion function for zsh and for bash, and $SHELL is {:?}.\n       \
             `eprint completions zsh` prints one of them if you want to adapt it.",
            shell
        );
    };
    let path = rc_path().context("could not determine your home directory")?;
    if completions_installed() {
        println!("\n  already set up in {}\n", path.display());
        return Ok(());
    }
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&completion_line(kind));
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
        || shell_kind().is_none()
        || completions_installed()
        || db::meta_get(conn, KEY).unwrap_or(None).is_some()
    {
        return;
    }
    let _ = db::meta_set(conn, KEY, "shown");
    eprintln!("tip: `eprint config --completions` switches on Tab completion for paper ids");
}
