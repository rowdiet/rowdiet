use super::*;

fn t(key: &str) -> TypeRef {
    TypeRef {
        key: key.into(),
        display: key.into(),
        char_len: None,
        dims: 0,
    }
}

fn tn(key: &str, n: u64) -> TypeRef {
    TypeRef {
        key: key.into(),
        display: format!("{key}({n})"),
        char_len: Some(n),
        dims: 0,
    }
}

fn arr(key: &str, dims: u8) -> TypeRef {
    TypeRef {
        key: key.into(),
        display: format!("{key}[]"),
        char_len: None,
        dims,
    }
}

fn cat() -> Catalog {
    Catalog::new(BTreeMap::new())
}

#[test]
fn verified_builtins() {
    assert_eq!(
        cat().resolve(&t("uuid")).kind,
        ColumnKind::Fixed {
            len: 16,
            align: Align::Char
        }
    );
    assert_eq!(
        cat().resolve(&t("int8")).kind,
        ColumnKind::Fixed {
            len: 8,
            align: Align::Double
        }
    );
    assert_eq!(
        cat().resolve(&t("bool")).kind,
        ColumnKind::Fixed {
            len: 1,
            align: Align::Char
        }
    );
    assert!(cat().resolve(&t("timetz")).kind.irregular());
    assert!(cat().resolve(&t("macaddr")).kind.irregular());
    assert_eq!(
        cat().resolve(&t("numeric")).kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: false,
            payload: Payload::EVEN
        }
    );
    assert!(cat().resolve(&t("inet")).known);
    assert!(!cat().resolve(&t("inet")).kind.is_fixed());
}

#[test]
fn char_and_varchar_proven_short() {
    assert_eq!(
        cat().resolve(&tn("varchar", 31)).kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: true,
            payload: Payload::UNVERIFIED
        }
    );
    assert_eq!(
        cat().resolve(&tn("varchar", 32)).kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: false,
            payload: Payload::UNVERIFIED
        }
    );
    assert_eq!(
        cat().resolve(&t("varchar")).kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: false,
            payload: Payload::ANY
        }
    );
    assert_eq!(
        cat().resolve(&tn("bpchar", 1)).kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: true,
            payload: Payload {
                compressible: false,
                ..Payload::UNVERIFIED
            }
        }
    );
}

#[test]
fn quoted_char_byte_type() {
    assert_eq!(
        cat().resolve(&t("pgchar")).kind,
        ColumnKind::Fixed {
            len: 1,
            align: Align::Char
        }
    );
}

#[test]
fn arrays_follow_element_alignment() {
    assert_eq!(
        cat().resolve(&arr("int8", 1)).kind,
        ColumnKind::Varlena {
            align: Align::Double,
            proven_short: false,
            payload: Payload::array(8)
        }
    );
    assert_eq!(
        cat().resolve(&arr("text", 1)).kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: false,
            payload: Payload::array(4)
        }
    );
    assert_eq!(
        cat().resolve(&arr("float8", 2)).kind,
        ColumnKind::Varlena {
            align: Align::Double,
            proven_short: false,
            payload: Payload::array(8)
        }
    );
    let unknown_elem = cat().resolve(&arr("nope", 1));
    assert_eq!(
        unknown_elem.kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: false,
            payload: Payload::UNVERIFIED
        }
    );
    assert!(!unknown_elem.known);
}

#[test]
fn serial_implies_not_null() {
    let r = cat().resolve(&t("bigserial"));
    assert_eq!(
        r.kind,
        ColumnKind::Fixed {
            len: 8,
            align: Align::Double
        }
    );
    assert!(r.implicit_not_null);
    assert!(cat().resolve(&t("serial")).implicit_not_null);
    assert!(!cat().resolve(&t("int4")).implicit_not_null);
}

#[test]
fn unknown_defaults_flagged() {
    let r = cat().resolve(&t("wat_type"));
    assert_eq!(
        r.kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: false,
            payload: Payload::UNVERIFIED
        }
    );
    assert!(!r.known);
}

#[test]
fn assume_specs_parse() {
    assert_eq!(
        parse_assume_spec("vector=varlena:d").unwrap(),
        ("vector".to_string(), AssumedKind::Varlena { align: Align::Double })
    );
    assert_eq!(
        parse_assume_spec("Foo=fixed:16:c").unwrap(),
        (
            "foo".to_string(),
            AssumedKind::Fixed {
                len: 16,
                align: Align::Char
            }
        )
    );
    assert!(parse_assume_spec("nope").is_err());
    assert!(parse_assume_spec("x=fixed:banana:c").is_err());
    assert!(parse_assume_spec("x=varlena:z").is_err());
}

#[test]
fn curated_extension_types_are_verified() {
    for key in [
        "citext",
        "hstore",
        "vector",
        "halfvec",
        "sparsevec",
        "ltree",
        "lquery",
        "ltxtquery",
    ] {
        let r = cat().resolve(&t(key));
        let payload = if key == "citext" {
            Payload::ANY
        } else {
            Payload::UNVERIFIED
        };
        assert_eq!(
            r.kind,
            ColumnKind::Varlena {
                align: Align::Int,
                proven_short: false,
                payload
            },
            "{key}"
        );
        assert!(r.known, "{key}");
    }
    for key in ["geometry", "geography", "cube"] {
        let r = cat().resolve(&t(key));
        assert_eq!(
            r.kind,
            ColumnKind::Varlena {
                align: Align::Double,
                proven_short: false,
                payload: Payload::UNVERIFIED
            },
            "{key}"
        );
        assert!(r.known, "{key}");
    }
    let box3d = cat().resolve(&t("box3d"));
    assert_eq!(
        box3d.kind,
        ColumnKind::Fixed {
            len: 52,
            align: Align::Double
        }
    );
    assert!(box3d.kind.irregular());
}

/// Every builtin entry, asserted against pg_type.dat facts stated independently of the source
/// table. The catalog is vendored data: a typo in one arm silently mis-models every layout
/// using that type, and no other test necessarily touches the entry (mutation testing found
/// exactly that — deleted arms for money/date/interval/… survived the suite).
#[test]
fn builtin_table_is_pinned_entry_by_entry() {
    let fixed = |len, align| ColumnKind::Fixed { len, align };
    let varlena = |align, payload| ColumnKind::Varlena {
        align,
        proven_short: false,
        payload,
    };
    let expectations: &[(&str, ColumnKind)] = &[
        ("bool", fixed(1, Align::Char)),
        ("pgchar", fixed(1, Align::Char)),
        ("int2", fixed(2, Align::Short)),
        ("int4", fixed(4, Align::Int)),
        ("int8", fixed(8, Align::Double)),
        ("float4", fixed(4, Align::Int)),
        ("float8", fixed(8, Align::Double)),
        ("money", fixed(8, Align::Double)),
        ("oid", fixed(4, Align::Int)),
        ("regclass", fixed(4, Align::Int)),
        ("date", fixed(4, Align::Int)),
        ("time", fixed(8, Align::Double)),
        ("timetz", fixed(12, Align::Double)),
        ("timestamp", fixed(8, Align::Double)),
        ("timestamptz", fixed(8, Align::Double)),
        ("interval", fixed(16, Align::Double)),
        ("uuid", fixed(16, Align::Char)),
        ("macaddr", fixed(6, Align::Int)),
        ("macaddr8", fixed(8, Align::Int)),
        ("name", fixed(64, Align::Char)),
        ("point", fixed(16, Align::Double)),
        ("lseg", fixed(32, Align::Double)),
        ("box", fixed(32, Align::Double)),
        ("line", fixed(24, Align::Double)),
        ("circle", fixed(24, Align::Double)),
        ("pg_lsn", fixed(8, Align::Double)),
        ("serial", fixed(4, Align::Int)),
        ("serial4", fixed(4, Align::Int)),
        ("bigserial", fixed(8, Align::Double)),
        ("serial8", fixed(8, Align::Double)),
        ("smallserial", fixed(2, Align::Short)),
        ("serial2", fixed(2, Align::Short)),
        ("box3d", fixed(52, Align::Double)),
        ("numeric", varlena(Align::Int, Payload::EVEN)),
        ("text", varlena(Align::Int, Payload::ANY)),
        ("bytea", varlena(Align::Int, Payload::ANY)),
        ("json", varlena(Align::Int, Payload::ANY)),
        ("jsonb", varlena(Align::Int, Payload::ANY)),
        ("xml", varlena(Align::Int, Payload::ANY)),
        ("inet", varlena(Align::Int, Payload::UNVERIFIED)),
        ("cidr", varlena(Align::Int, Payload::UNVERIFIED)),
        ("bit", varlena(Align::Int, Payload::UNVERIFIED)),
        ("varbit", varlena(Align::Int, Payload::ANY)),
        ("varchar", varlena(Align::Int, Payload::ANY)),
        ("bpchar", varlena(Align::Int, Payload::UNVERIFIED)),
        ("tsvector", varlena(Align::Int, Payload::UNVERIFIED)),
        ("tsquery", varlena(Align::Int, Payload::UNVERIFIED)),
        ("int4range", varlena(Align::Int, Payload::UNVERIFIED)),
        ("numrange", varlena(Align::Int, Payload::UNVERIFIED)),
        ("daterange", varlena(Align::Int, Payload::UNVERIFIED)),
        ("int4multirange", varlena(Align::Int, Payload::UNVERIFIED)),
        ("nummultirange", varlena(Align::Int, Payload::UNVERIFIED)),
        ("datemultirange", varlena(Align::Int, Payload::UNVERIFIED)),
        ("int8range", varlena(Align::Double, Payload::UNVERIFIED)),
        ("tsrange", varlena(Align::Double, Payload::UNVERIFIED)),
        ("tstzrange", varlena(Align::Double, Payload::UNVERIFIED)),
        ("int8multirange", varlena(Align::Double, Payload::UNVERIFIED)),
        ("tsmultirange", varlena(Align::Double, Payload::UNVERIFIED)),
        ("tstzmultirange", varlena(Align::Double, Payload::UNVERIFIED)),
        ("path", varlena(Align::Double, Payload::UNVERIFIED)),
        ("polygon", varlena(Align::Double, Payload::UNVERIFIED)),
        ("citext", varlena(Align::Int, Payload::ANY)),
        ("hstore", varlena(Align::Int, Payload::UNVERIFIED)),
        ("vector", varlena(Align::Int, Payload::UNVERIFIED)),
        ("halfvec", varlena(Align::Int, Payload::UNVERIFIED)),
        ("sparsevec", varlena(Align::Int, Payload::UNVERIFIED)),
        ("ltree", varlena(Align::Int, Payload::UNVERIFIED)),
        ("lquery", varlena(Align::Int, Payload::UNVERIFIED)),
        ("ltxtquery", varlena(Align::Int, Payload::UNVERIFIED)),
        ("geometry", varlena(Align::Double, Payload::UNVERIFIED)),
        ("geography", varlena(Align::Double, Payload::UNVERIFIED)),
        ("cube", varlena(Align::Double, Payload::UNVERIFIED)),
    ];
    for (key, expected) in expectations {
        let resolved = cat().resolve(&t(key));
        assert_eq!(resolved.kind, *expected, "{key}");
        assert!(resolved.known, "{key}");
        let serial_family = key.contains("serial");
        assert_eq!(resolved.implicit_not_null, serial_family, "{key}");
    }
}

#[test]
fn assume_map_overrides_builtin() {
    let mut assume = BTreeMap::new();
    assume.insert("citext".to_string(), AssumedKind::Varlena { align: Align::Int });
    assume.insert(
        "vector".to_string(),
        AssumedKind::Fixed {
            len: 16,
            align: Align::Double,
        },
    );
    let c = Catalog::new(assume);
    assert!(c.resolve(&t("citext")).known);
    assert_eq!(
        c.resolve(&t("vector")).kind,
        ColumnKind::Fixed {
            len: 16,
            align: Align::Double
        }
    );
}

#[test]
fn session_types() {
    let mut c = cat();
    c.define_enum("status".into());
    assert_eq!(
        c.resolve(&t("status")).kind,
        ColumnKind::Fixed {
            len: 4,
            align: Align::Int
        }
    );
    assert!(c.resolve(&t("status")).known);
    c.define_composite("pair".into());
    assert_eq!(
        c.resolve(&t("pair")).kind,
        ColumnKind::Varlena {
            align: Align::Double,
            proven_short: false,
            payload: Payload::UNVERIFIED
        }
    );
    c.define_range("bigrange".into(), Some(&t("int8")));
    assert_eq!(
        c.resolve(&t("bigrange")).kind,
        ColumnKind::Varlena {
            align: Align::Double,
            proven_short: false,
            payload: Payload::UNVERIFIED
        }
    );
    c.define_range("textrange".into(), Some(&t("text")));
    assert_eq!(
        c.resolve(&t("textrange")).kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: false,
            payload: Payload::UNVERIFIED
        }
    );
    c.define_domain("code".into(), &tn("varchar", 20));
    assert_eq!(
        c.resolve(&t("code")).kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: true,
            payload: Payload::UNVERIFIED
        }
    );
    c.drop_type("status");
    assert!(!c.resolve(&t("status")).known);
}

#[test]
fn enum_array_is_int_aligned_varlena() {
    let mut c = cat();
    c.define_enum("status".into());
    assert_eq!(
        c.resolve(&arr("status", 1)).kind,
        ColumnKind::Varlena {
            align: Align::Int,
            proven_short: false,
            payload: Payload::array(4)
        }
    );
    assert!(c.resolve(&arr("status", 1)).known);
}
