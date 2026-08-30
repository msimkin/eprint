//! What a bare `eprint` shows, and why.
//!
//! Selection and labelling only — nothing here draws anything. It lives in the
//! library rather than beside the renderer because this is the most heavily
//! reasoned behaviour in the tool, and each rule below is the answer to a specific
//! complaint: the floor that is not a cap, the top-up that keeps the feed from ever
//! being empty, the replay that survives a quiet archive, and a header whose two
//! dates deliberately mean different things. A second implementation of it would be
//! wrong within a release, the same way three copies of the FTS retry let a header
//! disagree with the results it was counting.
//!
//! Reading is separate from marking. [`build`] writes nothing; [`mark_seen`]
//! advances the markers, and the caller runs it only once the listing is actually in
//! front of someone. A batch that is computed and never displayed must not consume
//! the window it measured — which matters far more to a front-end that refreshes in
//! the background than it ever did to a terminal.

use crate::db::{self, Connection, Hit};
use crate::{dates, harvest, text};
use anyhow::Result;

/// Sanity bound on one arrival batch, for someone coming back after a long
/// absence. Not a display limit: `latest_limit` is a floor, not a cap.
pub const BATCH_MAX: usize = 500;

/// One feed, and enough about how it was chosen for a caller to say so.
pub struct Feed {
    pub hits: Vec<Hit>,
    /// The header's second half: `7 new since 03/08/2026`, `nothing new since
    /// 27/07/2026`, `last batch, from 30/07/2026 · nothing new yet`, or `since …`.
    pub label: String,
    /// How many genuinely arrived since the last look, before any replay or top-up.
    pub fresh: usize,
    pub replayed: bool,
    pub topped_up: bool,
    /// The watermark this diff was taken from, handed back to [`mark_seen`].
    pub window: String,
}

/// Choose the batch and describe it. Reads only.
pub fn build(conn: &Connection, floor: usize, exact: Option<usize>) -> Result<Feed> {
    let watermark = db::meta_get(conn, harvest::KEY_LAST_SEEN)?
        .unwrap_or_else(|| dates::format_iso(dates::now() - 7 * 86400));

    let day = |ts: &str| text::fmt_date(ts);

    // No cap on the batch itself, only a sanity bound for the case where someone
    // returns from a long absence.
    let mut hits = db::added_since(conn, &watermark, BATCH_MAX)?;
    let fresh = hits.len();
    // The window whose papers are on screen: the fresh diff normally, the
    // remembered one when there is no fresh diff to show.
    let mut window = watermark.clone();
    let mut replayed = false;

    // ePrint posts in bursts, so most runs find nothing. Rather than report an
    // empty diff, show the last one again until the archive actually moves.
    if hits.is_empty() {
        if let Some(prev) = db::meta_get(conn, harvest::KEY_NEW_BATCH)? {
            let again = db::added_since(conn, &prev, BATCH_MAX)?;
            if !again.is_empty() {
                hits = again;
                window = prev;
                replayed = true;
            }
        }
    }

    // Still short of the floor — either nothing is new or the batch was tiny — so
    // top up with the most recent arrivals. They are ordered the same way, so the
    // new ones stay at the top and the rest are simply context.
    let topped_up = hits.len() < floor;
    if topped_up {
        hits = db::recent_arrivals(conn, floor)?;
    }
    // With `-n`, the number given wins outright, batch or not.
    if let Some(n) = exact {
        hits.truncate(n);
    }

    // The two dates in this header deliberately mean different things. A count of
    // new papers is *about* your last look, so it is dated by it. "Nothing new",
    // dated by your last look, only ever says "you ran this recently" — the useful
    // answer there is when the archive itself last posted.
    let posted = db::newest(conn)?.map(|(_, date)| day(&date));

    // Order matters: a topped-up listing is no longer "the last batch", even if a
    // replay is what it was topped up from, so that case is reported first.
    let label = if topped_up {
        match (fresh, &posted) {
            (0, Some(p)) => format!("nothing new since {p}"),
            // Only with no papers at all, which the caller has already handled.
            (0, None) => "nothing new".to_string(),
            (n, _) => format!("{n} new since {}", day(&watermark)),
        }
    } else if replayed {
        // The batch's own newest paper, not the window that produced it: the window
        // start is another "when you last ran it" date, and a late-published paper
        // (recent arrival, older date) would make the index-wide answer name a date
        // no paper on screen carries.
        let batch = hits
            .iter()
            .map(|h| h.paper.date.as_str())
            .max()
            .map(day)
            .unwrap_or_else(|| day(&window));
        format!("last batch, from {batch} · nothing new yet")
    } else {
        format!("since {}", day(&window))
    };

    Ok(Feed {
        hits,
        label,
        fresh,
        replayed,
        topped_up,
        window,
    })
}

/// Advance the markers, once the listing has actually been shown.
///
/// Remember a *fresh* diff so later runs can replay it; a replay needs no pointer
/// write, since the pointer already names that window, and a topped-up listing is
/// not a batch at all. Writing the pointer only for a fresh, non-empty diff is what
/// stops a replay pinning itself forever.
pub fn mark_seen(conn: &Connection, window: &str, fresh: usize) -> Result<()> {
    if fresh > 0 {
        db::meta_set(conn, harvest::KEY_NEW_BATCH, window)?;
    }
    db::meta_set(
        conn,
        harvest::KEY_LAST_SEEN,
        &dates::format_iso(dates::now()),
    )
}
