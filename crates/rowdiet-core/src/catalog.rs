//! Type resolution: name → (typlen, typalign) storage class. Resolution order: types defined in
//! the analyzed migration set (exact, by replaying DDL), then user-supplied assumptions, then the
//! vendored built-in table. Unresolvable names default to (varlena, int-aligned) — CREATE TYPE's
//! documented default alignment is int4 and varlena requires at least 4 — and are flagged, never
//! guessed at `d` (which would fabricate waste).

use crate::layout::{Align, ColumnKind};
use std::collections::BTreeMap;

/// A type name as extracted from DDL: normalized lookup key, original display text, character
/// typmod (varchar(n)/char(n)) and array dimensionality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeRef {
    /// Normalized lookup key: unquoted names lowercased, qualification stripped to the last
    /// component (the catalog is unqualified).
    pub key: String,
    /// The full spelling as written, for reports.
    pub display: String,
    /// Declared character count of `varchar(n)`/`char(n)`; None when unlimited or not a
    /// character type. n ≤ 31 proves short-form storage.
    pub char_len: Option<u64>,
    /// `[]` depth as written (0 = scalar); any nonzero value resolves as one array varlena —
    /// Postgres arrays share a single representation regardless of declared dimensionality.
    pub dims: u8,
}

/// User-supplied storage assumption for a type name the DDL cannot resolve — the value side of
/// [`Config::assume`](crate::Config) and of the CLI's `--assume-type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssumedKind {
    /// Fixed-width: always `len` bytes at `align`.
    Fixed {
        /// Size in bytes; [`parse_assume_spec`] enforces 1..=32767 (pg_type.typlen's int2 range).
        len: u64,
        /// Storage alignment.
        align: Align,
    },
    /// Variable-length, counted at long-form header size.
    Varlena {
        /// Alignment of the long form.
        align: Align,
    },
}

/// A catalog answer: what a [`TypeRef`] means for layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    /// Storage class for the layout walk.
    pub kind: ColumnKind,
    /// False when the class is a guess (unresolvable name, shell type, range over an unknown
    /// subtype) — the fold notes it once and reports the column as assumed.
    pub known: bool,
    /// True only for serial pseudo-types: the column is NOT NULL without any constraint.
    pub implicit_not_null: bool,
}

#[derive(Debug, Clone, Copy)]
struct SessionEntry {
    kind: ColumnKind,
    known: bool,
}

/// The resolver for one analysis run. Lookup layers, first hit wins: session-defined types
/// (replayed `CREATE TYPE` DDL), then user assumptions, then the vendored built-in table.
#[derive(Debug)]
pub struct Catalog {
    assume: BTreeMap<String, AssumedKind>,
    session: BTreeMap<String, SessionEntry>,
}

impl Catalog {
    /// A catalog with no session-defined types yet; `assume` seeds the user layer.
    pub fn new(assume: BTreeMap<String, AssumedKind>) -> Self {
        Self {
            assume,
            session: BTreeMap::new(),
        }
    }

    /// `CREATE TYPE .. AS ENUM`: an enum value occupies four bytes on disk (typecmds.c DefineEnum).
    pub fn define_enum(&mut self, key: String) {
        self.session.insert(
            key,
            SessionEntry {
                kind: ColumnKind::Fixed {
                    len: 4,
                    align: Align::Int,
                },
                known: true,
            },
        );
    }

    /// `CREATE TYPE .. AS (..)`: composite columns are varlena, double-aligned.
    pub fn define_composite(&mut self, key: String) {
        let kind = ColumnKind::Varlena {
            align: Align::Double,
            proven_short: false,
        };
        self.session.insert(key, SessionEntry { kind, known: true });
    }

    /// `CREATE TYPE .. AS RANGE (SUBTYPE = ..)`: d iff the subtype is d-aligned, else i (typecmds.c).
    pub fn define_range(&mut self, key: String, subtype: Option<&TypeRef>) {
        let (align, known) = match subtype.map(|t| self.resolve(t)) {
            Some(sub) => (range_align(sub.kind.align()), sub.known),
            None => (Align::Int, false),
        };
        let kind = ColumnKind::Varlena {
            align,
            proven_short: false,
        };
        self.session.insert(key, SessionEntry { kind, known });
    }

    /// `CREATE DOMAIN .. AS base`: inherits the base type's storage verbatim (DefineDomain).
    pub fn define_domain(&mut self, key: String, base: &TypeRef) {
        let resolved = self.resolve(base);
        self.session.insert(
            key,
            SessionEntry {
                kind: resolved.kind,
                known: resolved.known,
            },
        );
    }

    /// `CREATE TYPE name (INPUT = ..)` or a shell type: alignment is declared in options we do not
    /// evaluate — keep the sound default and stay flagged.
    pub fn define_shell(&mut self, key: String) {
        let kind = ColumnKind::Varlena {
            align: Align::Int,
            proven_short: false,
        };
        self.session.insert(key, SessionEntry { kind, known: false });
    }

    /// `DROP TYPE`: forgets a session definition. Names not session-defined (assumed or
    /// built-in) are unaffected.
    pub fn drop_type(&mut self, key: &str) {
        self.session.remove(key);
    }

    /// `ALTER TYPE old RENAME TO new`: moves a session-defined type; a rename of a type created
    /// outside the analyzed set is a no-op (the new name stays unresolvable, as before).
    pub fn rename_type(&mut self, old: &str, new: String) {
        if let Some(entry) = self.session.remove(old) {
            self.session.insert(new, entry);
        }
    }

    /// Resolve a reference to its storage class; total — an unresolvable name yields the flagged
    /// default (see the module doc), never an error. Arrays are varlena, d-aligned iff the
    /// element type is, and `known` tracks the element.
    pub fn resolve(&self, t: &TypeRef) -> Resolved {
        if t.dims > 0 {
            let elem = self.resolve_scalar(&t.key, None);
            let kind = ColumnKind::Varlena {
                align: range_align(elem.kind.align()),
                proven_short: false,
            };
            return Resolved {
                kind,
                known: elem.known,
                implicit_not_null: false,
            };
        }
        self.resolve_scalar(&t.key, t.char_len)
    }

    fn resolve_scalar(&self, key: &str, char_len: Option<u64>) -> Resolved {
        if let Some(entry) = self.session.get(key) {
            return Resolved {
                kind: entry.kind,
                known: entry.known,
                implicit_not_null: false,
            };
        }
        if let Some(assumed) = self.assume.get(key) {
            let kind = match assumed {
                AssumedKind::Fixed { len, align } => ColumnKind::Fixed {
                    len: *len,
                    align: *align,
                },
                AssumedKind::Varlena { align } => ColumnKind::Varlena {
                    align: *align,
                    proven_short: false,
                },
            };
            return Resolved {
                kind,
                known: true,
                implicit_not_null: false,
            };
        }
        match builtin(key, char_len) {
            Some(resolved) => resolved,
            None => Resolved {
                kind: ColumnKind::Varlena {
                    align: Align::Int,
                    proven_short: false,
                },
                known: false,
                implicit_not_null: false,
            },
        }
    }
}

/// Parse a `NAME=varlena:ALIGN` / `NAME=fixed:LEN:ALIGN` assumption spec (align c|s|i|d).
/// Shared by the CLI `--assume-type` flag and the wasm module's JSON config.
pub fn parse_assume_spec(spec: &str) -> Result<(String, AssumedKind), String> {
    let usage = || format!("assume-type `{spec}`: expected NAME=varlena:ALIGN or NAME=fixed:LEN:ALIGN");
    let (name, rest) = spec.split_once('=').ok_or_else(usage)?;
    let parts: Vec<&str> = rest.split(':').collect();
    let kind = match parts.as_slice() {
        ["varlena", align] => AssumedKind::Varlena {
            align: parse_align(align)?,
        },
        ["fixed", len, align] => {
            let len: u64 = len
                .parse()
                .map_err(|_| format!("assume-type `{spec}`: bad length `{len}`"))?;
            // pg_type.typlen is an int2; anything outside 1..=32767 is not a Postgres type and
            // would corrupt the offset walk (a huge value overflows it).
            if !(1..=32767).contains(&len) {
                return Err(format!("assume-type `{spec}`: length must be 1..=32767, got {len}"));
            }
            AssumedKind::Fixed {
                len,
                align: parse_align(align)?,
            }
        }
        _ => return Err(usage()),
    };
    Ok((name.trim().to_lowercase(), kind))
}

fn parse_align(s: &str) -> Result<Align, String> {
    match s {
        "c" | "char" => Ok(Align::Char),
        "s" | "short" => Ok(Align::Short),
        "i" | "int" => Ok(Align::Int),
        "d" | "double" => Ok(Align::Double),
        other => Err(format!("bad alignment `{other}` (use c|s|i|d)")),
    }
}

/// Arrays and ranges alike: d iff the element/subtype is d-aligned, else i.
fn range_align(elem: Align) -> Align {
    if elem == Align::Double {
        Align::Double
    } else {
        Align::Int
    }
}

fn builtin(key: &str, char_len: Option<u64>) -> Option<Resolved> {
    let fixed = |len, align| {
        Some(Resolved {
            kind: ColumnKind::Fixed { len, align },
            known: true,
            implicit_not_null: false,
        })
    };
    let varlena = |align| {
        Some(Resolved {
            kind: ColumnKind::Varlena {
                align,
                proven_short: false,
            },
            known: true,
            implicit_not_null: false,
        })
    };
    let serial = |len, align| {
        Some(Resolved {
            kind: ColumnKind::Fixed { len, align },
            known: true,
            implicit_not_null: true,
        })
    };
    match key {
        // Verified against pg_type.dat (64-bit, MAXALIGN 8).
        "bool" => fixed(1, Align::Char),
        // The quoted one-byte "char" type; extract maps `"char"` to this key.
        "pgchar" => fixed(1, Align::Char),
        "int2" => fixed(2, Align::Short),
        "int4" => fixed(4, Align::Int),
        "int8" => fixed(8, Align::Double),
        "float4" => fixed(4, Align::Int),
        "float8" => fixed(8, Align::Double),
        "money" => fixed(8, Align::Double),
        "oid" | "regclass" => fixed(4, Align::Int),
        "date" => fixed(4, Align::Int),
        "time" => fixed(8, Align::Double),
        "timetz" => fixed(12, Align::Double),
        "timestamp" | "timestamptz" => fixed(8, Align::Double),
        "interval" => fixed(16, Align::Double),
        "uuid" => fixed(16, Align::Char),
        "macaddr" => fixed(6, Align::Int),
        "macaddr8" => fixed(8, Align::Int),
        "name" => fixed(64, Align::Char),
        "point" => fixed(16, Align::Double),
        "numeric" | "text" | "bytea" | "json" | "jsonb" | "xml" | "inet" | "cidr" | "bit" | "varbit" => {
            varlena(Align::Int)
        }
        // typmod can PROVE short form: n <= 31 chars is at most 4*31+1 = 125 bytes even in
        // worst-case UTF-8, under the 127-byte short-varlena limit — stored unaligned.
        "varchar" | "bpchar" => Some(Resolved {
            kind: ColumnKind::Varlena {
                align: Align::Int,
                proven_short: char_len.is_some_and(|n| n <= 31),
            },
            known: true,
            implicit_not_null: false,
        }),
        // Standard pg_type.dat values, not independently re-verified.
        "lseg" | "box" => fixed(32, Align::Double),
        "line" | "circle" => fixed(24, Align::Double),
        "pg_lsn" => fixed(8, Align::Double),
        "path" | "polygon" => varlena(Align::Double),
        "tsvector" | "tsquery" => varlena(Align::Int),
        "int4range" | "numrange" | "daterange" | "int4multirange" | "nummultirange" | "datemultirange" => {
            varlena(Align::Int)
        }
        "int8range" | "tsrange" | "tstzrange" | "int8multirange" | "tsmultirange" | "tstzmultirange" => {
            varlena(Align::Double)
        }
        // Serial pseudo-types resolve to their int type and imply NOT NULL.
        "serial" | "serial4" => serial(4, Align::Int),
        "bigserial" | "serial8" => serial(8, Align::Double),
        "smallserial" | "serial2" => serial(2, Align::Short),
        // Curated extension types, verified against their CREATE TYPE definitions (pgvector
        // sql/vector.sql; contrib citext/hstore/ltree; contrib cube; PostGIS postgis.sql.in and
        // geography.sql.in). The int-aligned group declares no ALIGNMENT and takes CREATE TYPE's
        // int4 default — same storage class as the unknown-type assumption, verified rather than
        // assumed. The double-aligned group declares `alignment = double`, which changes where
        // these cluster among varlenas — the entries that alter numbers, not just confidence.
        "citext" | "hstore" | "vector" | "halfvec" | "sparsevec" | "ltree" | "lquery" | "ltxtquery" => {
            varlena(Align::Int)
        }
        "geometry" | "geography" | "cube" => varlena(Align::Double),
        // PostGIS box3d: fixed 52 bytes, double-aligned — irregular (52 % 8 != 0), so the
        // exact-search places it at the end of its alignment group.
        "box3d" => fixed(52, Align::Double),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
