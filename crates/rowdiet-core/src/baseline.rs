//! Brownfield gating. A baseline file records, per table, the layout that is committed: applied
//! in production, where reordering it would need a rewrite. A committed layout is never judged
//! again; what a later migration appends to it is. For a baselined table the gate asks one
//! question: given the committed prefix and the offset residues it can end at, is the appended
//! block as written dominance-optimal among orders of the block? New tables get the full search.
//! No allowance is stored, so there is nothing to ratchet.
//!
//! Entries are pinned to the table's physical layout, one slot per attribute number. `ADD
//! COLUMN` appends a slot. `DROP COLUMN` rewrites nothing in PostgreSQL: the dropped attribute
//! keeps its slot (stored as NULL from then on) and every other column stays where it was, so a
//! committed slot that is dropped since still marks the committed prefix. The columns in slots
//! past the committed ones are the appended block, whatever was dropped before them. Any other
//! change to a committed slot (a type change, or a table rebuilt in another order) expires the
//! entry, and the table is judged like a new one until re-accepted.

use crate::report::{Analysis, BlockFinding, TableReport, block_finding};
use std::collections::BTreeMap;

/// In-memory form of a baseline file (JSON on disk): the default gate plus each committed
/// layout. Built by [`build_from`], maintained by [`accept_tables`], consumed by [`evaluate`].
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Baseline {
    /// Version of rowdiet that wrote the file (informational).
    #[cfg_attr(feature = "serde", serde(default))]
    pub rowdiet: String,
    /// Default gate for tables without an entry and for appended blocks. Fractional values are
    /// meaningful: estimate-tier avoidable numbers move in eighths of a byte.
    pub fail_over: f64,
    /// Committed layouts, keyed by the fold key ([`TableReport::name`]). A file that lists one
    /// table twice is rejected rather than read last-wins.
    #[cfg_attr(feature = "serde", serde(deserialize_with = "unique_tables"))]
    pub tables: BTreeMap<String, BaselineEntry>,
}

/// One table's committed layout.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "EntryOnDisk")
)]
pub struct BaselineEntry {
    /// The table's physical layout when it was accepted ([`crate::report::layout_signature`]).
    pub layout: CommittedLayout,
    /// The committed columns' names in physical order, written for whoever reviews the file. A
    /// file whose names do not match the layout's live slots one for one is rejected; beyond
    /// that the gate reads only `layout`, so renaming a committed column keeps the entry.
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Vec::is_empty"))]
    pub columns: Vec<String>,
    /// The per-table byte allowance older files carry. It is read so those files keep loading,
    /// never written, and ignored by the gate; [`GateOutcome::ignored_allowances`] lists it.
    #[cfg_attr(feature = "serde", serde(rename = "bytes", default, skip_serializing))]
    pub legacy_bytes: Option<f64>,
}

/// An entry as read from disk, before its column names are held to its layout.
#[cfg(feature = "serde")]
#[derive(serde::Deserialize)]
struct EntryOnDisk {
    layout: CommittedLayout,
    #[serde(default)]
    columns: Vec<String>,
    #[serde(rename = "bytes", default)]
    legacy_bytes: Option<f64>,
}

#[cfg(feature = "serde")]
impl TryFrom<EntryOnDisk> for BaselineEntry {
    type Error = String;

    fn try_from(entry: EntryOnDisk) -> Result<Self, String> {
        let live = entry.layout.slots().iter().filter(|slot| *slot != "-").count();
        if !entry.columns.is_empty() && entry.columns.len() != live {
            return Err(format!(
                "layout `{}` commits {live} live column(s) but `columns` names {}",
                entry.layout,
                entry.columns.len()
            ));
        }
        Ok(Self {
            layout: entry.layout,
            columns: entry.columns,
            legacy_bytes: entry.legacy_bytes,
        })
    }
}

impl BaselineEntry {
    /// An entry committing `layout`, with no column names.
    pub fn new(layout: CommittedLayout) -> Self {
        Self {
            layout,
            columns: Vec::new(),
            legacy_bytes: None,
        }
    }

    /// An entry committing `table` as it is now: its whole layout and its column names.
    pub fn committing(table: &TableReport) -> Self {
        Self {
            layout: CommittedLayout::parse(&table.layout_signature).expect("rowdiet writes parseable signatures"),
            columns: table.columns.iter().map(|c| c.name.clone()).collect(),
            legacy_bytes: None,
        }
    }
}

/// A committed physical layout: one slot per attribute number, each `f{len}{align}`,
/// `v{align}` (`p` appended when proven short), or `-` for a dropped attribute. Parsed on load,
/// so an entry that is not a layout fails loudly instead of never matching.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "String", into = "String")
)]
pub struct CommittedLayout(Vec<String>);

impl CommittedLayout {
    /// Parse a comma-joined signature.
    ///
    /// # Errors
    ///
    /// When a slot is not a fixed, varlena, or dropped slot.
    pub fn parse(signature: &str) -> Result<Self, String> {
        if signature.is_empty() {
            return Ok(Self(Vec::new()));
        }
        let slots: Vec<String> = signature.split(',').map(str::to_string).collect();
        for slot in &slots {
            let align = |c: char| matches!(c, 'c' | 's' | 'i' | 'd');
            let valid = match slot.as_bytes() {
                [b'-'] => true,
                [b'f', rest @ .., last] => {
                    !rest.is_empty() && rest.iter().all(u8::is_ascii_digit) && align(char::from(*last))
                }
                [b'v', a] => align(char::from(*a)),
                [b'v', a, b'p'] => align(char::from(*a)),
                _ => false,
            };
            if !valid {
                return Err(format!("`{slot}` in layout `{signature}` is not a layout slot"));
            }
        }
        Ok(Self(slots))
    }

    /// The slots, in attribute-number order.
    pub fn slots(&self) -> &[String] {
        &self.0
    }
}

impl TryFrom<String> for CommittedLayout {
    type Error = String;

    fn try_from(signature: String) -> Result<Self, String> {
        Self::parse(&signature)
    }
}

impl From<CommittedLayout> for String {
    fn from(layout: CommittedLayout) -> Self {
        layout.0.join(",")
    }
}

impl std::fmt::Display for CommittedLayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.join(","))
    }
}

#[cfg(feature = "serde")]
fn unique_tables<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, BaselineEntry>, D::Error> {
    struct Tables;
    impl<'de> serde::de::Visitor<'de> for Tables {
        type Value = BTreeMap<String, BaselineEntry>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a map from table name to entry")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut tables = BTreeMap::new();
            while let Some((name, entry)) = map.next_entry::<String, BaselineEntry>()? {
                if tables.contains_key(&name) {
                    return Err(serde::de::Error::custom(format!("table `{name}` is listed twice")));
                }
                tables.insert(name, entry);
            }
            Ok(tables)
        }
    }
    deserializer.deserialize_map(Tables)
}

/// Per-table gate result.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize),
    serde(tag = "verdict", rename_all = "snake_case")
)]
pub enum TableVerdict {
    /// Within the applicable limit, the committed layout unchanged, the appended block
    /// dominance-optimal, or no limit in force.
    Pass,
    /// The table could not be fully modeled (an unexpanded LIKE/INHERITS/typed table): no
    /// avoidable-bytes judgment is possible, so it is neither a pass nor a violation. Non-failing
    /// on its own; `--fail-on-degraded` escalates it like any other incomplete table.
    Incomplete,
    /// No baseline entry and avoidable exceeds `fail_over`.
    NewViolation {
        /// Current avoidable bytes/row.
        avoidable: f64,
    },
    /// Columns were appended to the committed layout and some order of the appended block
    /// dominates the order written, by more than `fail_over`. Actionable while the appending
    /// migration is unapplied (reorder the block there; the prefix stays) or acceptable
    /// explicitly. The block order to write is in [`GateOutcome::blocks`].
    BlockNotDominanceOptimal {
        /// What the dominating block order saves, bytes/row.
        avoidable: f64,
        /// Columns in the appended block.
        appended: usize,
    },
    /// A committed slot changed (a type change, or a table rebuilt in another order), expiring
    /// the entry, and the table does not meet `fail_over`. Re-accept deliberately or fix the
    /// layout in the migration that changed it.
    ModifiedSinceBaseline {
        /// Current avoidable bytes/row.
        avoidable: f64,
    },
}

impl TableVerdict {
    /// True for the verdict kinds that fail the gate.
    pub fn failing(self) -> bool {
        matches!(
            self,
            Self::NewViolation { .. } | Self::BlockNotDominanceOptimal { .. } | Self::ModifiedSinceBaseline { .. }
        )
    }
}

/// The verdict kind's stable tag — the same word the serde `verdict` field carries
/// (`pass`, `new_violation`, …), so logs and JSON name verdicts identically. Payload numbers
/// are not included; renderers present those with their own wording.
impl std::fmt::Display for TableVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tag = match self {
            Self::Pass => "pass",
            Self::Incomplete => "incomplete",
            Self::NewViolation { .. } => "new_violation",
            Self::BlockNotDominanceOptimal { .. } => "block_not_dominance_optimal",
            Self::ModifiedSinceBaseline { .. } => "modified_since_baseline",
        };
        f.write_str(tag)
    }
}

/// What [`evaluate`] concluded: the overall pass/fail plus everything a renderer needs to say why.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct GateOutcome {
    /// The gate failed: some verdict is failing, or the analysis degraded under
    /// `fail_on_degraded`. The CLI's exit-1 signal.
    pub exceeded: bool,
    /// Statements the parser skipped (loudly noted, but a gate that only counts avoidable
    /// bytes would stay green over them — these counts make the degradation visible to
    /// automation, and `fail_on_degraded` turns them into a failure).
    pub skipped_statements: usize,
    /// Non-ignored tables marked incomplete (skipped or unexpanded DDL touched them).
    pub incomplete_tables: usize,
    /// Non-ignored complete tables whose dominance search ran out of budget: findings there
    /// stand, but a clean verdict says only that the searched candidates hold no dominating
    /// reorder. [`fail_on_budgeted`](Self::fail_on_budgeted) turns them into a failure.
    pub budgeted_tables: usize,
    /// The gate failed because the analysis degraded under `fail_on_degraded`.
    pub failed_on_degraded: bool,
    /// The gate failed because a dominance search was budgeted, under
    /// [`fail_on_budgeted`](Self::fail_on_budgeted).
    pub failed_on_budgeted: bool,
    /// Scanned paths that matched no SQL files at all — a typo'd migrations directory would
    /// otherwise gate green forever having analyzed nothing.
    pub empty_scans: usize,
    /// One verdict per non-ignored analyzed table.
    pub verdicts: BTreeMap<String, TableVerdict>,
    /// The appended block of every baselined table that grew since acceptance, passing or not.
    pub blocks: BTreeMap<String, BlockFinding>,
    /// Baseline entries with no matching analyzed table (renamed or dropped since acceptance).
    pub orphaned: Vec<String>,
    /// Entries whose layout changed non-append while the table now meets `fail_over` — stale,
    /// removed by the next baseline write.
    pub expired: Vec<String>,
    /// Entries that still carry a byte allowance from an older file; the gate ignores it, and
    /// the next baseline write drops it.
    pub ignored_allowances: Vec<String>,
}

impl GateOutcome {
    /// True when the analysis degraded: statements were skipped, tables are incomplete, or a
    /// scanned path matched no SQL files. This is exactly the condition `fail_on_degraded`
    /// escalates to a failure; without that flag it stays visible here while the gate stays
    /// green.
    pub fn degraded(&self) -> bool {
        self.skipped_statements > 0 || self.incomplete_tables > 0 || self.empty_scans > 0
    }

    /// Fail the gate when some table's dominance search was budgeted. Separate from
    /// `fail_on_degraded` because a budget binds on ordinary tables (42% of a 3,000-table corpus
    /// of 4 to 8 realistic columns), while a parser skip is rare and fixable.
    pub fn fail_on_budgeted(&mut self) {
        self.failed_on_budgeted = self.budgeted_tables > 0;
        self.exceeded |= self.failed_on_budgeted;
    }
}

/// Gate an analysis. An explicit `fail_over` wins over the baseline file's recorded one; with
/// neither present nothing can fail. Ignored tables are outside both gate and baseline.
/// `fail_on_degraded` additionally fails the gate when statements were skipped, tables are
/// incomplete, or a scanned path matched no SQL files — without it those are surfaced in the
/// outcome but stay green (a sqlparser user cannot always fix a parser gap; under pg-exact
/// skips should be zero, so strict is cheap).
pub fn evaluate(
    analysis: &Analysis,
    fail_over: Option<f64>,
    fail_on_degraded: bool,
    baseline: Option<&Baseline>,
) -> GateOutcome {
    let default_limit = fail_over.or_else(|| baseline.map(|b| b.fail_over));
    let mut verdicts = BTreeMap::new();
    let mut blocks = BTreeMap::new();
    let mut expired = Vec::new();
    for table in analysis.gated_tables() {
        let entry = baseline.and_then(|b| b.tables.get(&table.name));
        let judged = table_verdict(table, entry, default_limit);
        if judged.expired {
            expired.push(table.name.clone());
        }
        if let Some(block) = judged.block {
            blocks.insert(table.name.clone(), block);
        }
        verdicts.insert(table.name.clone(), judged.verdict);
    }
    let orphaned = baseline
        .map(|b| {
            b.tables
                .keys()
                .filter(|n| !verdicts.contains_key(*n))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let ignored_allowances = baseline
        .map(|b| {
            b.tables
                .iter()
                .filter(|(_, entry)| entry.legacy_bytes.is_some())
                .map(|(name, _)| name.clone())
                .collect()
        })
        .unwrap_or_default();
    let skipped_statements = analysis
        .notes
        .iter()
        .filter(|n| n.kind == crate::fold::NoteKind::SkippedStatement)
        .count();
    let incomplete_tables = analysis.gated_tables().filter(|t| t.incomplete).count();
    let budgeted_tables = analysis.gated_tables().filter(|t| t.budgeted()).count();
    let empty_scans = analysis
        .notes
        .iter()
        .filter(|n| n.kind == crate::fold::NoteKind::EmptyScan)
        .count();
    let mut outcome = GateOutcome {
        exceeded: false,
        skipped_statements,
        incomplete_tables,
        budgeted_tables,
        failed_on_degraded: false,
        failed_on_budgeted: false,
        empty_scans,
        verdicts,
        blocks,
        orphaned,
        expired,
        ignored_allowances,
    };
    outcome.failed_on_degraded = fail_on_degraded && outcome.degraded();
    outcome.exceeded = outcome.verdicts.values().any(|v| v.failing()) || outcome.failed_on_degraded;
    outcome
}

/// One table's judgment: its verdict, its appended block when it grew since acceptance, and
/// whether its entry went stale (the layout changed non-append while the table now meets the
/// limit, so the next baseline write drops it).
struct Judged {
    verdict: TableVerdict,
    block: Option<BlockFinding>,
    expired: bool,
}

fn table_verdict(table: &TableReport, entry: Option<&BaselineEntry>, default_limit: Option<f64>) -> Judged {
    let judged = |verdict| Judged {
        verdict,
        block: None,
        expired: false,
    };
    // A table nobody could fully model has no avoidable-bytes judgment to make — it is not a
    // pass (the false negative this guards against) and not a violation. `--fail-on-degraded`
    // escalates it via the incomplete-tables count, same as before.
    if table.incomplete {
        return judged(TableVerdict::Incomplete);
    }
    let avoidable = table.avoidable_bytes_per_row;
    let over_limit = |value: f64| default_limit.is_some_and(|l| value > l);
    let Some(entry) = entry else {
        return judged(if over_limit(avoidable) {
            TableVerdict::NewViolation { avoidable }
        } else {
            TableVerdict::Pass
        });
    };
    let current = CommittedLayout::parse(&table.layout_signature).expect("rowdiet writes parseable signatures");
    match relation(&entry.layout, &current) {
        // The committed layout is the whole table: nothing is free to reorder.
        SignatureRelation::Match => judged(TableVerdict::Pass),
        SignatureRelation::Grown { committed_slots } => {
            let block = block_finding(table, committed_slots);
            let verdict = match &block {
                Some(block) if over_limit(block.avoidable_bytes_per_row) => TableVerdict::BlockNotDominanceOptimal {
                    avoidable: block.avoidable_bytes_per_row,
                    appended: block.columns.len(),
                },
                Some(_) | None => TableVerdict::Pass,
            };
            Judged {
                verdict,
                block,
                expired: false,
            }
        }
        SignatureRelation::Different if over_limit(avoidable) => {
            judged(TableVerdict::ModifiedSinceBaseline { avoidable })
        }
        SignatureRelation::Different => Judged {
            verdict: TableVerdict::Pass,
            block: None,
            expired: true,
        },
    }
}

enum SignatureRelation {
    /// Every committed slot is unchanged or dropped since, and nothing was appended.
    Match,
    /// Every committed slot is unchanged or dropped since, and columns were appended in the slots
    /// past `committed_slots`.
    Grown {
        committed_slots: usize,
    },
    Different,
}

fn relation(committed: &CommittedLayout, current: &CommittedLayout) -> SignatureRelation {
    let (committed, current) = (committed.slots(), current.slots());
    let kept =
        current.len() >= committed.len() && committed.iter().zip(current).all(|(was, now)| was == now || now == "-");
    match (kept, current.len() > committed.len()) {
        (false, _) => SignatureRelation::Different,
        (true, false) => SignatureRelation::Match,
        (true, true) => SignatureRelation::Grown {
            committed_slots: committed.len(),
        },
    }
}

/// Rewrite-from-scratch baselining: one entry per analyzed, modeled, non-ignored table at its
/// current signature, so every table there counts as committed. Orphans, expired entries, and
/// old byte allowances vanish by construction.
pub fn build_from(analysis: &Analysis, fail_over: f64, version: &str) -> Baseline {
    let tables = analysis
        .gated_tables()
        .filter(|t| !t.incomplete)
        .map(|t| (t.name.clone(), BaselineEntry::committing(t)))
        .collect();
    Baseline {
        rowdiet: version.to_string(),
        fail_over,
        tables,
    }
}

/// Record the named tables' current layouts as committed: the explicit, reviewable act of
/// accepting an appended block as written, so the next migration's block is judged against the
/// new prefix. Other entries stay untouched.
///
/// # Errors
///
/// When a name (fold key or display spelling) matches no non-ignored analyzed table. Names
/// processed before the failing one have already been applied — persist the baseline only on Ok.
pub fn accept_tables(baseline: &mut Baseline, analysis: &Analysis, names: &[String]) -> Result<(), String> {
    for name in names {
        let table = analysis
            .gated_tables()
            .find(|t| t.name == *name || t.display == *name)
            .ok_or_else(|| format!("cannot accept `{name}`: no such table in the analyzed DDL (or it is ignored)"))?;
        // Entries are stored under the canonical fold key regardless of which spelling the
        // caller used to name the table — an entry keyed by display would never match a gate.
        baseline
            .tables
            .insert(table.name.clone(), BaselineEntry::committing(table));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
