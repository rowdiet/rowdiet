# rowdiet design notes

The decisions the code embodies and the reasoning that must survive refactors.

## Pipeline

```
split (tolerant, hand-rolled)  →  extract (sqlparser 0.62, the ONLY AST-touching module)
      →  fold (replay DDL over a schema model, version order)  →  layout (padding math)
      →  report (exact | estimate tiers)  →  CLI render (text | json | github)
```

- `split.rs` never fails: it understands PG quoting (dollar quotes with tags, E-string backslash
  escapes, doubled quotes, *nested* block comments) exactly enough to find top-level semicolons
  and 1-based line numbers. Whether a statement parses is decided per statement downstream.
- `extract.rs` is the swappable-parser boundary. If sqlparser's gap rate ever matters on a real
  corpus, `postgresql-cst-parser` (PG17-grammar-generated, ~190 KB gz wasm) slots in behind the
  same `DdlOp` output. Everything after `DdlOp` is parser-agnostic.
- Parse failure ladder: skip loudly (note) → sniff the target (`ALTER TABLE x` → mark table `x`
  incomplete; `CREATE TABLE y` → remember `y` as a ghost so later ALTERs say "its CREATE was
  skipped" instead of "unknown table").
- `DO` bodies (both backends, shared path in `lib.rs`): extract the dollar-quoted body, re-split
  it, parse each fragment from its first word-boundary CREATE/ALTER/DROP. Type-creating DDL is
  folded (idempotency-guard pattern — a column using the type implies it exists; wrong only if
  the guard would have *not* created it, in which case PG itself would have errored). Table DDL
  is never folded: conditional execution is unknowable, so it becomes a `do-block` note +
  incomplete flag. Dynamic `EXECUTE [format(]'…'` fragments get template classification: the
  literal template is parsed with placeholders substituted (`%I`/`%s` tried as identifier and as
  number — hash-partition `REMAINDER %s` needs the latter). `CREATE TABLE … PARTITION OF` a
  modeled parent is layout-inert (children inherit the parent verbatim) and earns silence — this
  covers the ubiquitous hand-rolled partition-creation loop; pg_partman's own
  `create_parent(...)` calls contain no DDL keywords and were always silent. Dynamic DDL against
  a concrete table becomes a targeted conditional note; placeholder-targeted or unparseable
  templates keep the loud summary note. DML-only DO bodies are silent — same as any other DML.
  A `rowdiet:ignore` marker inside a DO waives its scan entirely.

## The offset model (what the numbers mean)

A realization fixes what the DDL leaves open: each varlena's storage form and payload length,
and whether each nullable column holds a NULL. Varlena payload bytes are never counted toward
sizes (they are unknowable from DDL), but they determine the offset residue mod 8 that every
later column aligns against — the total payload is order-invariant while its effect on
downstream padding is order-dependent. The walk therefore carries the offset as a set of possible
residues mod 8: exact until the first varlena, the full set after any varlena (a proven-short
typmod bounds only the header form, payload byte length still varies), narrowed again by
alignment (an 8-aligned column collapses any set back to a single residue). Each pad placed over
a non-singleton set is reported as min/max/expected. Pads placed while the residue is exactly
known stay exact.

A varlena has three on-disk forms, and only one of them pads. Postgres packs any payload of
126 bytes or less into the 1-byte-header short form and stores it **unaligned**
(`heap_compute_data_size` converts, `att_align_datum` skips alignment); a toasted value is an
18-byte pointer, also unaligned; only the in-line long form (4-byte header, payloads from
127 bytes up to the TOAST threshold) aligns. pageinspect confirms all three: a
`(float8, timestamp, int4, text×5)` table with short payloads is flat at zero padding, and
4 kB `STORAGE EXTERNAL` payloads leave 18-byte unaligned pointers.

NULLs are realization variables too (issue #12). A NULL stores neither its value nor a pad
(`heap_fill_tuple` skips the attribute and only clears its bitmap bit), so a nullable column
moves later offsets by its pad and length in the rows that store it and by nothing in the rows
that do not. For a fixed column that is a NULL bit of its own. For a varlena a NULL advances 0
bytes mod 8, which is what a short value with a 7-byte payload advances. Text takes such
payloads, so its NULL adds nothing. Numeric and array payloads never do (numeric payloads are
even, so a short numeric advances an odd number of bytes; a short `float8[]` advances 5), so a
nullable numeric or array gets the NULL as a realization of its own: the engine adds residue 7
to a nullable varlena's short-form set, whatever its type narrows that set to.
`(i int4, a float8[], s int2)` shows why it matters: `(a, s, i)` dominates while `a` is NOT
NULL, and pads 2 before `i` in every row where a nullable `a` holds NULL (measured). The columns
whose NULL is a step no value takes are listed in `null_variables`, and `NOT NULL` is a
constraint that removes one.

The walk (`layout::walk`) and the `padding`, `pad_before` and `offset` fields describe rows that
store every column. Bounds come from `dominance::summary`, which walks the eight offset residues
over every realization, NULLs included, because once a NULL can skip a column the per-column
extremes stop composing (`(bool, int2 NULL, int4)` pads 1 or 3, never 0 or 4). Where the rows
without NULLs have other bounds, they are reported beside them (`without_nulls`). A pad is
deterministic in the gate's sense (`avoidable_deterministic`) only when it has one value in
every row that stores its column under every NULL pattern (`layout::certain_padding`), so a pad
behind a nullable fixed column stays deterministic only if an alignment restores it.

Expected values rest on three stated assumptions, all printed in the estimate tier's label, and
they are **display only** — nothing gates, recommends, or rewrites on them (see the decision
policy below and the impossibility section for why):

1. **Varlena pads are scored at the short-form/TOAST value of zero.** The long form
   contributes only to the pad's max. Payloads that land in the long-form band raise the real
   number toward that max.
2. **Offset residues after a varlena are taken as uniformly likely.** Real payload-width
   distributions can be skewed mod 8 (fixed-length codes; a TOAST pointer pins the residue
   entirely), which moves later fixed-column pads inside the reported min/max range.
3. **No column holds a NULL.** NULL frequencies are workload facts, so the expectation describes
   the rows that store every column.

The min/max bounds hold without any assumption: they range over every residue, storage form,
and NULL pattern, and both ends are attained (pinned by enumeration tests that simulate concrete
tuples, NULLs included, varlena NULLs among them).

Tiers:

- **Exact** — table has only fixed-width columns. Padding and footprint are byte-exact
  (fixed-width values are never toasted/compressed). Headline = MAXALIGN-rounded footprint delta;
  `avoidable == 0` whenever the reorder doesn't cross an 8-byte rung, even if raw padding drops.
  With nullable columns every row is still byte-exact, but which columns a row stores varies:
  the label reads `exact per NULL pattern`, the footprint is reported for rows that store every
  column and as a range for rows holding a NULL (with their bitmap header, `with_nulls`), and
  the decision policy below runs in row size instead of padding. A padding comparison would
  miss reorders that pad more somewhere without ever adding a row-size rung:
  `(c0 timetz NULL, c1 int2, c2 timetz NULL)` measures 56 vs 56 B with every column stored and
  48 vs 40 B with `c0` NULL, so `(c0, c2, c1)` dominates in row size although it pads 2 more in
  the first case. Searching, the absence claim, frontier bands and the no-NULL row all count row
  size there. The headline is the largest row-size saving over NULL patterns, with the saving
  every row gets as its deterministic part.
- **Estimate** — any varlena present. Headline = deterministic plus dominance-proven avoidable
  padding (the decision policy below). The min/max range is the guaranteed envelope; expected
  values are display fields.

Why no middle tier: even for "fixed prefix + varlena tail preserved" the *total* delta is not
byte-guaranteed — payload lengths shift downstream pads in both orders. The report does carry a
"deterministic" component (`OrderStats.padding`, the pads whose value the DDL fixes), and it
always appears beside the expected value and the range: in an interleaved table a fixed
column's real offset rides on the preceding varlena's actual length, so a fixed-columns-only
number presented alone would overclaim for the as-written order.

Null bitmap: present per-row when the row has a NULL, sized by table natts
(`t_hoff 24 → 32` at 9 columns, → 40 at 73). Order-invariant, so it never changes reorder
advice: both orders of one realization share the header, which is also why a row-size
difference is the padding difference plus the trailing MAXALIGN rounding. Rows that store every
column use `t_hoff = 24`, except after `DROP COLUMN`, where the bitmap is unconditionally
present in new rows (dropped attributes are stored as NULL forever), so the walk uses
`t_hoff = null_thoff(original natts)`; see Folding semantics. The exact tier reports the header
of NULL-carrying rows beside their size range (`with_nulls`), which is where the 9-column cliff
of issue #4 shows: in a nullable `cols9` a one-NULL row stays at 96 bytes.

## Why no point prior can rank varlena orders (issue #10, condensed)

A varlena's own pad depends on its storage form: the short form (payload <= 126 B inline) and
TOAST pointers store unaligned, the inline long form aligns to typalign. The form depends on
payload bytes, and payload bytes are absent from DDL. Two realizable workloads on the same DDL
then want different orders — `(int8, text, float8[])` wants text first under 200 B texts with
tiny arrays, and array first under short texts with 20-element arrays. Any deterministic
hint-free tool picks one order and is strictly suboptimal on the other workload; the missing
information is a property of the input, so no model refinement removes it.

Both point priors tried before this policy failed measurably. P(long) = 1 fabricated 6.0 B/row
on a control table that measures flat zero. P(short) = 1 scored every varlena self-pad at its
minimum, which made varlena-vs-varlena orders unrankable (0.000 vs 4.000 B/row measured on
identical columns, both reported clean) and recommended a reorder measuring 4.000 B/row worse
than the order it replaced. Expected values under any prior are therefore display conveniences,
and everything that acts — the gate, the recommendation, the `--suggest` rewrite — acts on
realization-independent facts only.

## The decision policy

Three rungs, in order:

1. **Dominance (gate + recommend).** Order A dominates order B when A's total padding (row size
   at the exact tier) is less than or equal to B's in **every** realization of storage forms,
   payload lengths, and NULLs, and strictly less in at least one. A reorder is recommended, and gates, only on dominance. Much
   resolves here: repacking the fixed block with the varlena tail preserved dominates (the
   never-negative-recovery induction), and the search finds stronger wins such as handing a
   varlena the one guaranteed-aligned slot with char-aligned columns parked behind it —
   `(text, boolean, bigint)` reordered to `(bigint, text, boolean)` pads zero in every
   realization.
2. **Minimax (tie-break, labeled).** Among dominating candidates the one with the smallest
   worst case is recommended. Findings carry the guaranteed-to-maximum saving range and never
   say "expected savings".
3. **Frontier (report only).** When no candidate dominates but one is strictly better
   somewhere (or by worst case), both orders are printed with the decision boundary: per
   storage-form band, who wins and by how much. Never an exit-code consequence. The reader
   owns the workload knowledge; the tool hands over the boundary.

Gate rule: `avoidable_deterministic` (waste in exactly-known pads the recommended reorder
removes) plus `avoidable_dominance` (further worst-case waste the dominating reorder removes).
The sum always equals the engine's proven maximum saving, so the headline can never exceed
what some realization attains; the guaranteed minimum travels beside it (`dominance_saving`).
Frontier bands never gate. "No dominating reorder exists" is printed only after an exhaustive
sweep over a payload model no wider than storage; a budgeted search, or one over a type whose
payload lengths are unverified, says "found" plus the reason, and a capped order search is
labeled on clean lines and finding lines alike.

**The dominance engine** (`dominance.rs`) computes exact bounds of `pad(A) − pad(B)` over all
realizations. Padding depends on a realization only through each varlena's (form, payload mod
8) and each nullable column's NULL, so: when both orders keep the varlenas in the same relative
sequence, a joint walk over the pair of offset residues (64 states, extremes merged per state)
is exact at any column count. A nullable fixed column the two orders place at different points
carries its NULL bit in the state from its first placement to its second (the state then holds
everything the rest of either walk depends on, so merging stays exact); each bit in flight
doubles the states, and past 12 bits the pair goes to the enumeration. Otherwise exhaustive
enumeration runs within a budget (about 2M assignments, counting up to 16 per varlena and 2 per
nullable fixed column), and past it the pair is reported as undecided
(`dominance_search: budgeted`), never a guess. Both engines walk each run of NOT NULL fixed
columns through a per-residue table, so a comparison costs a step per varlena and per nullable
column at any width. The same engines total row size (padding plus the trailing MAXALIGN
rounding) for the exact tier. Frontier bands fix each varlena's form (up to 6 long-capable
varlenas, else the frontier prints without band detail), range over every payload and NULL
pattern, and reuse the same engines; a frontier also carries the comparison in rows without
NULLs (`without_nulls`) where it differs, and names what a mixed band turns on (payload
lengths, NULLs, or both).

One proof needs no pair at all. Nothing costs less than zero padding, so an order that pads
zero in every realization (`dominance::summary` reports a maximum of 0) is never worse than any
other order, and it saves exactly what the other order pads, realization by realization; in row
size the same holds for an order whose rows are never larger than the same row unpadded. The
sweep and the fallback candidates apply this before any comparison, which is what keeps wide
nullable tables gated: past 12 NULL bits in flight and 2M realizations no pair comparison fits,
while the alignment-sorted order of regular types pads zero under every NULL pattern. On the
stack review's 300 random fixed-width tables of 15 to 40 columns, 80% nullable, every table is
flagged (the previous revision of this change, without the proof, flagged 154), every suggestion pads zero in
every NULL pattern, and the 24-column table measured on PostgreSQL 16 holds: the sorted order
is never larger over 400 NULL patterns and smaller in 397.

**The dominance sweep** is what makes the clean verdict a proof. `NOT NULL` fixed columns of one
padding class are pointwise interchangeable (they carry no realization variable, and their pads
depend only on (alignment, len mod 8) and the offset residue), so one representative arrangement
stands for all of them. Nullable fixed columns carry their own NULL and stay individuals, like
varlenas. Varlenas get no such collapse: a realization assigns each varlena column its
own payload, so swapping two same-class varlena columns changes padding pointwise: in
`(t1, m1, t2, m2)` the order `(m1, t2, t1, m2)` dominates while its class-sequence twin
`(m1, t1, t2, m2)` measures 4 B/row worse at t1 = 132 B. `layout::order_space` therefore
collapses `NOT NULL` fixed classes only and keeps every varlena and every nullable column an
individual, which makes it
pointwise-complete: if any reorder dominates the current order, some member attains identical
padding in every realization. The sweep tests every member against the current order (up to
5,040 sequences, 4M materialized column slots, and a comparison-work budget), pruning candidates
that fail a necessary condition for free: dominance implies <= on the worst case, the best case,
and the mean over any sub- distribution of realizations, and `dominance::summary` computes all
three over the same realization model the comparison uses. Members that keep same-class varlenas and same-class nullable fixed columns
in written order (the class sequences an earlier, collapsed sweep tested) go first, in that
sweep's order, then the rest by ascending worst case, so a trimmed sweep keeps every finding the
collapsed sweep made. A completed sweep reports `dominance_search: exhaustive` and the clean
line reads "no dominating reorder exists", a universal claim the completeness argument above
licenses; anything trimmed reports `budgeted` and says "found" instead. A trimmed or skipped
sweep still tests the search poles, the current order with its leading fixed run repacked, and
every fixed column first with the varlenas in written order; the last two keep the varlena
sequence, so the joint walk decides them exactly at any width, and a dominating pole is always
recommended, never printed as a frontier. After those it sweeps every order that keeps the
written varlena sequence and moves only fixed columns (`layout::sequence_space`, `NOT NULL`
classes collapsed, up to 40,320 orders), ranked by worst case behind the same prunes and bounded
by the same work budget. On the stack delta review's 3,000 realistic tables of 4 to 8 columns, 378
budgeted tables were clean before this sweep; 229 of them have a dominating order of that shape,
226 are now findings (the 721 budgeted findings that an independent enumeration could check all
dominate with the reported saving range), and the corpus takes 50 s, as before. `rev_g534`'s
suggestion measures 21,884 B smaller over 4,000 rows on PostgreSQL 16 and never larger on a row.
A clean budgeted table whose certain padding is above zero prints `◐`, like a capped one.
Scalar-objective poles alone were measured to miss 11-19% of dominating reorders on 4-5 column
varlena schemas, which is why the sweep exists.
Coverage, measured on 40,733 random five-column tables over an 11-type pool and 6,000 random
tables of 6 to 12 columns: the sweep completes for every table of up to six columns with at most
three varlenas and every seven-column table with at most two; at five columns, 36% of tables
with four varlenas and all with five are budgeted, and an all-`text` table is budgeted from five
columns; past seven columns of mixed fixed types the order space outgrows the 5,040 cap (49 to
97% budgeted at eight columns, 94 to 100% from nine). On the five-column corpus every budgeted table was still right against an
independent brute force (342 findings, 1,578 clean tables with no dominating order).

**The search** (`layout::search`) emits three candidate poles: the fixed-first heuristic
(fixed-prefix refined), the lexicographic (deterministic, worst-case) minimum — the certainty
pole, free to hide fixed columns behind varlenas — and the (worst-case, deterministic) minimum,
the minimax pole. Both lexicographic pairs are additive per (class, residue-set state), so the
DP over class counts × the 15 cosets of Z/8 minimizes them exactly; a property test pins both
poles against brute-force permutation search with irregulars and varlenas in the pool. The DP's
memo is a dense mixed-radix vector of exactly the state-space bound (8 bytes per state, no
hashing), filled in one ascending pass with no recursion, so a class of any size terminates in
bounded stack; the whole-order search runs whenever that bound fits the state budget
(`Π(count+1) × 15 <= 2^20`, 8 MB of memo at most); there is no column-count cap, because the
budget already bounds cost and a redundant cap was measured to flip a 7 B/row finding to a
silent pass at exactly 25 columns. Past the budget the search degrades to the fixed-prefix
block search (bounded by `Π(count+1) × 8 <= 2^23` singleton states, 64 MB of memo, at any
column count), and past that to the better of the plain sort and a greedy packing that takes,
column by column, the class padding least from the current offset (it pairs `timetz` with
`int4` and `macaddr` with `int2`, which the sort keeps apart). The exact tier without nullable
columns takes the certainty pole, which is the padding minimum whenever the search completed,
and labels a capped search `budgeted`; a capped table that still pads prints `◐` with what was
searched, never `✓`. The search does not see NULLs: its poles optimize the rows that store every
column, and the decision policy verifies every pole over every NULL pattern like any other
candidate.
The scope is `complete` / `fixed_prefix` / `sort_only` in the JSON and labeled on both clean
and finding lines, because a capped search claiming nothing was avoidable is the worst defect
this tool can have.

The certainty pole exists for the frontier: `(timetz, timetz, text)` pays a certain 4 B/row,
and interposing the text trades that for a data-dependent 0..=7 — measured 3 B/row worse on
4-byte texts, 3 B/row better on 2-byte texts. The policy never recommends that trade; it prints
it with both worst cases and the bands.

Two scope notes on the frontier itself. First (deviation five): a pole earns a frontier line
only when it also improves a realization-free summary (worst case, or deterministic padding); an
incomparable pole that is worse on both summaries is suppressed as noise, so one spelling of an
incomparable pair can render a frontier while the reverse spelling renders none. Second, payload
residues. A short value is stored uncompressed, and some encodings pin its length mod 8: an
array's data area starts MAXALIGNed and every element is padded to the element alignment
(`array.h`, `construct_md_array`), so a `float8[]` payload is always 4 mod 8 and an `int4[]` or
`text[]` payload 0 or 4 mod 8; numeric stores a 2- or 4-byte header plus 2-byte digits, so its
payloads are even (`numeric.c`). The model narrows the short form to those residues
(`layout::Payload`); the long form keeps every residue, because a compressed inline value takes
any length (measured on PostgreSQL 16.15: uncompressed `float8[]` payloads only ever 4 mod 8,
`int4[]` and `text[]` 0 or 4, `int2[]` and `numeric` even, while compressed inline `float8[]`
and `numeric` values reached all 8 residues). A model wider than storage is safe for a win,
since dominance over more realizations implies dominance over fewer, but not for an absence
claim: modeled with every residue, `(smallint, float8[], float8[], macaddr)` printed "no
dominating reorder exists" while `(macaddr, smallint, a2, a1)` measures never worse on 5,996
rows paired across short, long and mixed loads, and 8.444 to 1.730 B/row on the long-heavy one.
Types whose stored lengths depend on the database encoding or were not checked here (`inet`,
`bit`, `char(n)`, `varchar(n)` below 134 characters, whose single-byte spelling can neither
reach every short residue nor an uncompressed long value, `tsvector`, ranges, composites,
extension types other than `citext`, unknown types) keep every residue; their findings hold, and
a completed sweep over them reports `dominance_search: superset` and says "found", naming the
types.

Third, the toaster decides a value's form per tuple: it compresses or moves out the largest
attribute until the tuple fits, the tuple size includes the order's own padding, and size ties
go to the lower attribute number. A row near the 2 kB threshold, or with two equally large
values, can therefore realize differently in two orders of the same table (measured: 4 of 6,000
`pin` rows, an array moved out of line in one order and kept inline in the other). Every claim
here is per realization, and the measurement harness compares only rows both orders stored the
same way and reports the rest.

Fourth, storage strategies. `heap_fill_tuple` makes the 1-byte header only for an attribute
whose strategy is not PLAIN (`VARLENA_ATT_IS_PACKABLE`), so under PLAIN a value that arrives with
the 4-byte header keeps it, aligned, at any length: a `COPY` or an `UPDATE` stores it so, while a
plain `INSERT` of a literal arrives short and stays short (measured on PostgreSQL 16.15, a PLAIN
`varchar(5)`: `(c, s)` pads 0.500 B/row, `(s, c)` 2.000, on rows loaded by `COPY` and by
`UPDATE`). A PLAIN column also never moves a value out of line. The fold records each column's
strategy and whether any row since the last rewrite was written under PLAIN (`plain_rows`) or
under a strategy that toasts (`toasted_rows`), since `SET STORAGE` changes only the rows written
afterwards; `ALTER COLUMN ... TYPE` resets the strategy to the type's own, as PostgreSQL does, and
keeps the history, since a binary-coercible change leaves the rows in place. A column with PLAIN
rows is modeled long-capable whatever its typmod proves, and without the TOAST pointer when every
row was written under PLAIN. pg-exact reads `STORAGE` on column definitions and in
`SET STORAGE`; sqlparser does not parse the clause, so the statement is skipped loudly and the
table makes no claim. The layout signature keeps the declared kinds, so a strategy change
expires no baseline entry.

**The settling queries.** A frontier's winner turns on payload lengths, storage forms and
NULLs the DDL does not carry. Every frontier carries two read-only `SELECT`s (`query.reader` and
`query.pageinspect` in the JSON; the text and the step summary print the reader query, or the
pageinspect one under `--settle-exact`). Both lay every row out in both orders the way
`heap_fill_tuple` places values (a 1-byte header or a TOAST pointer unaligned, a 4-byte header
and every fixed-width value aligned, a NULL nowhere) and return the rows each order stores
smaller and the bytes the switch saves, in padding at the estimate tier and in row size at the
exact tier. Each walk is one nested expression per order over per-row values computed once
(`LATERAL ... OFFSET 0`), so both queries are linear in rows and write no temp files.

The reader query runs as any role that can read the table on PostgreSQL 14 or later, managed
PostgreSQL included; on an older server it answers "cannot settle" and names the version. It
reads each value's stored form exactly. For the text and bytea families, `pg_column_size` and
`octet_length` tell a 1-byte header, a 4-byte header and an uncompressed TOAST pointer apart.
For every other varlena, and for a compressed text, it compares `pg_column_size` of the value
with that of a one-column row built from it: the row adds 24 bytes to a value stored in line,
21 to a 4-byte header a 1-byte one could replace, and more to an out-of-line value, which
PostgreSQL fetches from TOAST to build the row. That fetch is the only TOAST data the query
reads. A sum of payload sizes cannot make this call: for an out-of-line value `pg_column_size`
reports the external payload, so a row just past the threshold looks like one that fits. Both
orders are then counted as a rewrite would store the row: a 4-byte header shortened where the
column allows it, and an out-of-line value back in line when the row, with the value fetched,
fits under the TOAST threshold (2,032 bytes at 8 kB pages) in both orders. A row that fits in
one order and not the other would be toasted in one order only; it is counted in
`approximate_rows` and left out of the other counts. The per-row SQL is built by `format` over
the column names the catalog confirmed and run through `query_to_xml`, whose answer is read
with a regular expression rather than `xpath`, so a missing table or column answers "cannot
settle" with the reason instead of an error. The pageinspect query reads every row version off
the page (`heap_page_item_attrs`, superuser), replays its stored bytes exactly, and checks the
written-order replay against the tuple's own length.

Neither answers silently wrong. Both look the table up by the schema the DDL names (or the
search path when it names none) and the columns by name, so an attached partition or a `LIKE`
copy with its own attribute numbers reads right. A table or a column the database does not have
yet (a block whose migration has not run) gives a "cannot settle" row with the reason and no
counts. A partitioned parent reads every partition (the pageinspect query every leaf); an
inheritance parent reads its own rows and the note names the children left out. A column with a
fast default is counted as a rewrite stores it by the reader query and noted; the pageinspect
query skips the row versions written before it (`t_infomask2` holds fewer attributes) and counts
them in `skipped_rows`. Rows that still hold a dropped column's value, and partitions in another
column order, are left out of the replay check only. The pageinspect query counts dead row
versions until pruning or VACUUM removes them. Names reach either query only as string literals
(`U&'...'` escapes for a backslash, a line break, `##[`, and the characters markdown or HTML
would read). The harness runs both printed queries on every frontier workload and holds them to
the paired measurement (the same rows, winners and bytes saved, nothing approximate, mismatched
or skipped), and runs both on a table outside the search path, hostile names, an absent table
and column, a fast default before and after a rewrite, dropped-column rows, dead versions, a
partitioned parent with an attached partition in its own order, and an inheritance parent. On
jsonb in every storage form (short, 4-byte, compressed in line, out of line just past the
threshold, values at the 127-byte short limit), on rows a `PLAIN` era left with 4-byte headers,
on out-of-line values an `UPDATE` left in rows that now fit, and on rows the frontier order
pushes over the threshold, the reader query must match copies of the table rebuilt in each order
and rewritten by `VACUUM FULL`: the same counts over the rows both copies store alike, and
`approximate_rows` equal to the rows the toaster stored differently.

Measured on PostgreSQL 16, one row in ten NULL, seconds including the client. Text table:
`(m macaddr, t text NOT NULL, s smallint NOT NULL)`, texts of 0 to 200 B. Jsonb table: the
same with `j jsonb`, one row in twenty holding an incompressible 3.9 kB value stored out of
line.

| rows | text table | reader | pageinspect, JIT on | pageinspect, JIT off | jsonb table | reader | pageinspect, JIT off | temp files |
|---|---|---|---|---|---|---|---|---|
| 10,000 | 1.4 MB | 0.08 s | 0.54 s | 0.09 s | 1.5 MB + 2.1 MB TOAST | 0.08 s | 0.09 s | 0 |
| 100,000 | 14 MB | 0.23 s | 0.73 s | 0.28 s | 14 MB + 20 MB TOAST | 0.25 s | 0.28 s | 0 |
| 1,000,000 | 136 MB | 1.7 s | 2.5 s | 2.2 s | 144 MB + 198 MB TOAST | 1.8 s | 2.2 s | 0 |

The pageinspect query's fixed 0.4 s under JIT is compilation; both queries agree on every
count at every size.

## Suggested order

Sort key of the heuristic pole: `(fixed=0 | varlena=1 | always-short=2, alignment desc,
irregular-last, original index)`. Irregulars are fixed types whose size isn't a multiple of
their own alignment — exactly `timetz (12,d)` and `macaddr (6,i)` among built-ins (also
`tid (6,s)`); putting them last in their group keeps every following smaller-alignment column
aligned. For all-regular schemas the heuristic pads zero under any NULL mask (a subsequence of
a desc-aligned regular sequence is still one, and `dominance::summary` proves it per table), and
a heuristic order achieving
zero deterministic and zero worst-case padding is the proven global minimum of both
lexicographic objectives, so the search completes without running. `refine_fixed_block` repairs
irregular blocks exactly (`timetz, int4, timetz` is zero where the sort pads 4) with the
varlena tail left in place, which keeps the repair dominance-safe. The report layer then
compares the poles against the current order by dominance and either recommends, prints a
frontier, or says what it searched (see the decision policy above).

## Type catalog provenance

`catalog.rs` marks the provenance of its built-in entries in two blocks:

1. **Verified against `pg_type.dat`** — the ~30 core entries (including the lint-loud
   surprises: `uuid (16,c)`, `timetz (12,d)` irregular, `macaddr (6,i)` irregular, `inet`/`cidr`
   varlena, `numeric` varlena, `char(1)` → varlena bpchar).
2. **Standard `pg_type.dat` values, not independently re-verified** — geometric types, `pg_lsn`,
   `tsvector`/`tsquery`, multiranges, `tid`-family omissions. If any is ever found wrong, fix the
   table, not the walk.

Derived rules (all from PostgreSQL source): enum → `(4,i)`; domain → base verbatim;
array/range/multirange → varlena, `d` iff element/subtype is `d`, else `i`; composite → `(-1,d)`;
serial family → int type + implicit NOT NULL. `varchar(n≤31)`/`char(n≤31)` keep the `p` in the
layout signature (≤ 4·31+1 = 125 B worst-case UTF-8, under the 127 B short-varlena cap, so an
uncompressed value stores unaligned), but only n ≤ 5 is *always short*: the toaster considers an
attribute for compression once it passes 24 bytes with its header (`toast_helper.c`), lz4 has
no minimum input, and a compressed value keeps the aligned 4-byte header (`VARATT_CAN_MAKE_SHORT`
needs an uncompressed value). From n = 6 a value can pass 20 payload bytes in 4-byte UTF-8, so
the model gives it the aligned form and leaves its payload unverified for absence claims.
Bare `char` is `char(1)`; bare `varchar` is unlimited. Quoted `"char"` is the 1-byte type
(catalog key `pgchar`).

Unknown types: (varlena, `i`), flagged, teachable — per `CREATE TYPE`'s documented defaults
(alignment defaults int4; varlena alignment must be ≥ 4). Never default to `d`: that fabricates
waste. No unverified extension entries are hardcoded; the curated extension entries that do
exist (pgvector, citext, hstore) come from their published `CREATE TYPE` definitions.

## Folding semantics

- **Tables keep their qualification as identity**: the fold key is the full dotted name with
  each part folded by its own quoting (`A.Things` → `a.things`, `a."Things"` → `a.Things`), so
  same-named tables in different schemas are distinct relations. Mixed qualified/unqualified
  references to one relation are not resolved (search_path is out of scope) — qualify
  consistently, as migrations should anyway. **Types** are keyed by their last
  name component (`pg_catalog.int4` resolves as `int4`; the type catalog is unqualified).
- `ADD COLUMN` appends — physically true in Postgres. `SET DATA TYPE` edits in place (a type
  change rewrites the table but keeps attnum order). `DROP COLUMN` removes the column from the
  walk but keeps a dropped-slot count: Postgres retains dropped attributes (attisdropped) and
  stores a NULL for each in every subsequent row, so the exact-tier footprint uses
  `t_hoff = null_thoff(original natts)` once anything was dropped (pageinspect-verified:
  10 int4 columns minus one = 72 B/row, 107 rows/page, not 64/120). Partition children inherit
  the parent's dropped slots. Renames tracked (renaming onto an existing table is noted, never
  silent); `DROP TABLE` removes; redefinition replaces with a note; `IF NOT EXISTS` duplicates
  are silent no-ops.
- `ADD PRIMARY KEY` (table-level or ALTER) forces NOT NULL on its columns; identity columns are
  implicitly NOT NULL; serial types likewise.
- CTAS (`CREATE TABLE … AS SELECT`) and `LIKE` clauses cannot be resolved statically → note +
  ghost/incomplete.
- `PARTITION OF parent`: the child inherits the parent's modeled columns (children cannot add
  columns), including the parent's incompleteness; an out-of-set parent leaves the child not
  modeled. A child, like a plain `(LIKE source)` copy of a known table, gets the live columns
  only, numbered from 1: PostgreSQL copies no dropped attribute into a new table, so a copy of a
  table with a dropped column has no dropped slot and no bitmap it would force (measured: 64 B
  rows for the copy where the source stores 72 B). Plain `INHERITS` stays incomplete (inherited-plus-own semantics
  are not modeled).

## Baseline gate (brownfield adoption)

Real schemas arrive with debt, and applied tables are exactly the ones a linter cannot ask
anyone to rewrite. The baseline file records which layouts are committed so the gate can stay
strict about everything still free to change. Design points, in the order they were decided:

- **Core owns the gate.** The cross-implementation parity of table reports (native CLI, both
  parser backends, the wasm module and everything built on it) is the project's strongest
  correctness property; if wrappers implemented their own gate arithmetic, parity would stop
  covering the pass/fail decision itself. `baseline::evaluate` is the single implementation;
  wrappers only read and write the file.
- **An entry is a committed layout, with no number in it (issue #13).** For an applied table
  the physical order is fixed and reordering needs a rewrite, so a per-table byte allowance
  only ever recorded debt and needed a ratchet to stay honest. The one order a reviewer can
  still change is the block a migration appends with `ADD COLUMN` (issue #3), and the dominance
  engine answers the question that matters there: given the committed columns and the offset
  residues they can end at, is the appended block as written dominance-optimal among orders of
  the block? The engine starts both block orders from those residues (the prefix pads the same
  in both, so it adds nothing to the difference) and searches block orders the same way it
  searches tables, the prefix never reordered. A block's order space is small even when the
  table is wide, so the block sweep is exhaustive exactly where a whole-table sweep is
  budgeted. A dominating block order fails the gate as `block_not_dominance_optimal` when its
  saving exceeds the fail-over, with the block order to write printed; a block nothing
  dominates passes, and a committed layout with nothing appended always passes. New tables
  get the full search against the fail-over as before. There is no ratchet: a dominance-optimal
  block has nothing to tighten.
- **Entries are pinned to the physical slots, not just a name.** Entries record `layout`, one
  slot per attribute number (`f{len}{align}` per fixed column, `v{align}` + `p` for proven-short
  varlena, `-` for a dropped attribute, comma-joined: `f8d,f4i,-,vi,vip`), parsed on load so a
  malformed entry fails loudly, and `columns`, the committed column names, written so a
  reviewer can see which columns an entry commits. The gate reads only the slots: renames and
  `SET/DROP NOT NULL` move no stored byte, so they keep the entry (nullability still feeds the
  block judgment, which runs with the current constraints). A file that lists one table twice
  is rejected instead of read last-wins. The signature is stored as that readable string rather
  than a hash: a baseline diff then *shows* what changed, and there is no hash-stability
  liability across releases.
- **Slots define the block (the prefix rule).** `ADD COLUMN` appends a slot. `DROP COLUMN`
  rewrites nothing: PostgreSQL keeps the attribute, marked dropped, at its number, stores NULL
  for it in every later row, and leaves every other column where it was. So a committed slot
  that is now `-` still belongs to the committed prefix, and the block is every column in a slot
  past the committed ones, whatever was dropped before it. Comparing kind strings instead would
  read drop-then-add as an append: committed `(id bigint, flag boolean, n smallint)`, then `n`
  dropped and `(x smallint, y int, z boolean)` added, starts with the committed string, and the
  gate judged only `(y, z)` while `(z, x, y)` saves 8 B/row (measured 48 vs 40 B page spacing).
  Any other change to a committed slot (a type change, or a table rebuilt in another order)
  expires the entry (`modified since baseline`, judged as a new table) and forces a deliberate
  re-accept. The finding names every migration that appended to the block, since a block can
  span more than one.
- **Accepting is explicit.** `--update-baseline` commits every analyzed table's current layout;
  `--accept <table>` commits one, which is how a block written as it is gets accepted: the next
  migration's block is then judged against the new prefix. After upgrading from a version that
  stored byte allowances, the first `--update-baseline` therefore writes an entry for every
  table, not only for those over the fail-over, which is a large diff to review once.
- **Old files keep loading.** Entries written before block gating carry a `bytes` allowance.
  It is read, ignored, listed in the gate outcome (`ignored_allowances`) and in the text, and
  dropped by the next write. Their layouts lack dropped slots, so an old entry for a table with
  a dropped column no longer matches and is judged as modified until re-accepted. `--fail-over`
  and a loaded file's `fail_over` reject nan and non-finite numbers: a templating bug that
  resolves an empty CI variable to `nan` would otherwise turn every `avoidable > limit`
  comparison false and silently disable the gate.

The gate also carries degradation: counts of skipped statements and incomplete tables ride in
the outcome (a bytes-only gate would stay green over an unparseable migration set), and
`fail_on_degraded` turns them into a failure — off by default so sqlparser users are not
punished for known parser gaps, cheap to enable under pg-exact where skips should be zero.
The degradation set is deliberately narrow: a skipped statement (a table that should be gated
went unparsed), an incomplete table (columns only partly known), or an empty scan (a path
matched no SQL). Notes for constructs rowdiet structurally does not gate stay loud but are not
degradation — a `CREATE TABLE AS SELECT` (columns come from the query, so the table is a ghost,
never gated), a temp table (no persistent storage), a dropped column (bitmap cost is modeled),
an assumed type (a separate honesty axis), or dynamic DDL in a `DO` body. Nothing that should be
gated passes green silently; a consumer wanting any of those to fail asserts on the specific note
kind, and `Analysis::degraded()` exposes the same set to library adopters. A budgeted dominance
search is counted beside it (`budgeted_tables`) but not in it: findings on such a table stand,
and a clean verdict there says "found", not "exists". The budget binds on ordinary tables (42%
of a 3,000-table corpus of 4 to 8 realistic columns), so folding it into `fail_on_degraded`
would fail most projects that use that flag; `--fail-on-budgeted` (`GateOutcome::fail_on_budgeted`)
is the separate opt-in for a gate that wants every clean verdict proven. The membership lives
in one wildcard-free `NoteKind::is_degradation` match, so adding a note kind is a compile error
until it is classified — the catch-all cannot silently miss a future kind.
Reports and baselines key on the fold key (lowercased unless quoted), which is identical across
parser backends; the as-written spelling is carried separately as `display`. Verdicts per
non-ignored table: `pass`, `new_violation` (no entry, over fail-over),
`block_not_dominance_optimal`, `modified_since_baseline`; every grown table's block finding
rides in `blocks`, passing or not; entries with no matching table are listed as `orphaned`,
expired-but-passing ones as `expired`. Ignored tables stay outside both gate and baseline.

## Version ordering

`version.rs`: optional `V/U/B` prefix, digit segments separated by `_` or `.` (sub-versions like
`V1718984460_1__` sort between their base and the next version), unversioned names (Flyway
`R__` repeatables) after all versioned, lexicographic tiebreak. Directories are walked
recursively; ordering applies per directory. That per-directory scope is a deliberate
divergence from Flyway, which orders by version *globally* across its locations: on a tree
whose version numbers interleave across sibling subdirectories, rowdiet folds in a different
order than Flyway applies. Timestamp-versioned and year-partitioned layouts are monotonic per
directory, so they are unaffected.

## Discovery policy

The walk (shared verbatim by the CLI and the JVM adapters — the only policy both can honor
identically, since Gradle's input fingerprinting cannot follow links) skips dot-prefixed
entries and does not enter symlinked directories, so a link cycle cannot collect a file once
per traversal depth; a symlinked `.sql` file is still collected and read through its link. An
explicitly passed root is exempt from the dot filter. A scanned path that matches no SQL files
produces an `empty_scan` note and counts as degradation under `fail_on_degraded` — a typo'd
migrations path must not gate green forever having analyzed nothing.

## Parser decision

sqlparser-rs 0.62 is the default parser: typed AST, Apache governance,
wasm32-unknown-unknown-clean (~400 KB gzipped, measured). Its known statement-level gaps are
contained by the splitter. `libpg_query` (the real PG parser) was tested empirically for wasm
shipping: it builds and runs end-to-end on `wasm32-unknown-emscripten` and on `wasm32-wasip1` +
wasi-sdk 33 (~390 KB gzipped, runs under wasmtime and Node's WASI), with PG's sigsetjmp error
handling working on both — but it is structurally blocked from `wasm32-unknown-unknown` (no
libc, no setjmp/longjmp runtime), and wasm-bindgen supports no other wasm target.

The adopted design: the web tier ships a single Rust-linked `wasm32-wasip1` module with
`libpg_query` behind the off-by-default `pg-exact` feature (WASI over emscripten for
toolchain-ownership reasons: frozen ABI, pin-a-tarball toolchain, no rustc↔linker version
pairing). sqlparser-rs remains the default parser and the native-primary path, and the
differential oracle lives in the `pg-exact` backend's test suite. Build recipe + stub headers:
`wasm/`.

## Measured verification (xtask measure)

`cargo run -p xtask -- measure` regression-tests the model against real tuples: it builds the
release binary, applies the fixture DDL to a disposable Dockerized PostgreSQL with pageinspect,
inserts short-heavy and long-heavy workloads (long-heavy payloads live in the 127 B..TOAST
band that falsified both point priors), and asserts the three spec properties: measured
padding inside every reported [min, max]; every recommended reorder measuring no worse than
the current order on both workloads; every declared frontier boundary flipping the measured
winner. Fixtures with nullable columns add workloads that NULL each nullable column by its own
bit of the row counter (every NULL pattern over up to ten columns occurs) and are held to the
bounds over every NULL pattern, the rows without NULLs to the bounds in rows that store every
column; exact-tier rows are checked for their header (24 without a NULL, the bitmap header with
one) and their MAXALIGN-rounded size against `footprint` and `with_nulls`, and recommendations
and bands there are checked in row size; band sweeps add NULL patterns on top of the residue
sweep; and a witness order (one the NULL-blind model would have recommended) must measure worse
than the written order on some row of its witness workload. Skipped loudly when no container is
reachable (`ROWDIET_MEASURE_CONTAINER`, default `condescending_tu`; tables carry the
`ROWDIET_MEASURE_PREFIX` prefix, default `synb_`), so plain CI stays database-free. Fixtures include the schemas that
falsified earlier revisions: the issue #1 pair, the issue #10 band pair, `(float8[], int4)`
both ways, `(timetz, timetz, text)`, the TOAST pointer case, and the 25-column and many-class
cap tables.

## Platform assumption

64-bit Postgres: `d` alignment = 8, `MAXALIGN` = 8. The Postgres docs hedge that `d` means
8 bytes "on many machines, but by no means all"; a 32-bit knob is deliberately out of scope for
v1 and would be a `layout.rs` parameter, not a redesign.
