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
    pub key: String,
    pub display: String,
    pub char_len: Option<u64>,
    pub dims: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssumedKind {
    Fixed { len: u64, align: Align },
    Varlena { align: Align },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    pub kind: ColumnKind,
    pub known: bool,
    pub implicit_not_null: bool,
}

#[derive(Debug, Clone, Copy)]
struct SessionEntry {
    kind: ColumnKind,
    known: bool,
}

#[derive(Debug)]
pub struct Catalog {
    assume: BTreeMap<String, AssumedKind>,
    session: BTreeMap<String, SessionEntry>,
}

impl Catalog {
    pub fn new(assume: BTreeMap<String, AssumedKind>) -> Self {
        Catalog {
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

    pub fn drop_type(&mut self, key: &str) {
        self.session.remove(key);
    }

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
        // Verified against pg_type.dat in the spike research (64-bit, MAXALIGN 8).
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
        // Standard pg_type.dat values, not re-verified in the spike research.
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
        _ => None,
    }
}

#[cfg(test)]
mod tests;
