//! pg-exact backend: the real PostgreSQL 17 grammar via libpg_query (the `pg_query` crate,
//! pganalyze's Rust binding) → the same [`DdlOp`] vocabulary as the sqlparser backend.
//! Native / wasip1 targets only (C build); everything downstream is backend-agnostic, which
//! makes this both the web-tier parser and the differential oracle for the default backend.
//!
//! Raw-parse-tree facts this mapping relies on: the scanner has already case-folded unquoted
//! identifiers; SQL-standard keyword types arrive catalog-qualified (`pg_catalog.int4`), plain
//! names (uuid, serial, timestamptz) arrive unqualified; the keyword `char` arrives as
//! `pg_catalog.bpchar` with no typmod (meaning char(1)) while a literal `bpchar` arrives
//! unqualified (meaning unlimited).

use crate::catalog::TypeRef;
use crate::extract::{DdlOp, RawColumn, RawName};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;

pub fn extract(text: &str) -> Result<Vec<DdlOp>, String> {
    let parsed = pg_query::parse(text).map_err(|e| e.to_string())?;
    let mut ops = Vec::new();
    for raw in parsed.protobuf.stmts {
        if let Some(node) = raw.stmt.and_then(|n| n.node) {
            ops.extend(map_statement(node));
        }
    }
    Ok(ops)
}

fn map_statement(node: NodeEnum) -> Vec<DdlOp> {
    match node {
        NodeEnum::CreateStmt(cs) => vec![map_create_table(&cs)],
        NodeEnum::AlterTableStmt(at) => map_alter_table(&at),
        NodeEnum::RenameStmt(rs) => map_rename(&rs),
        NodeEnum::CreateEnumStmt(ce) => vec![DdlOp::CreateEnum {
            name: name_from_nodes(&ce.type_name),
        }],
        NodeEnum::CreateRangeStmt(cr) => {
            let subtype = cr.params.iter().find_map(|n| match n.node.as_ref() {
                Some(NodeEnum::DefElem(de)) if de.defname == "subtype" => match de.arg.as_ref()?.node.as_ref() {
                    Some(NodeEnum::TypeName(tn)) => Some(map_type(tn)),
                    _ => None,
                },
                _ => None,
            });
            vec![DdlOp::CreateRange {
                name: name_from_nodes(&cr.type_name),
                subtype,
            }]
        }
        NodeEnum::CompositeTypeStmt(ct) => match &ct.typevar {
            Some(rv) => vec![DdlOp::CreateComposite {
                name: type_rangevar_name(rv),
            }],
            None => vec![DdlOp::Irrelevant],
        },
        NodeEnum::CreateDomainStmt(cd) => match &cd.type_name {
            Some(tn) => vec![DdlOp::CreateDomain {
                name: name_from_nodes(&cd.domainname),
                base: map_type(tn),
            }],
            None => vec![DdlOp::Irrelevant],
        },
        NodeEnum::DefineStmt(ds) if ds.kind() == pb::ObjectType::ObjectType => {
            vec![DdlOp::CreateBase {
                name: name_from_nodes(&ds.defnames),
            }]
        }
        NodeEnum::DropStmt(ds) => map_drop(&ds),
        NodeEnum::CreateTableAsStmt(ctas) => map_ctas(&ctas),
        _ => vec![DdlOp::Irrelevant],
    }
}

fn map_create_table(cs: &pb::CreateStmt) -> DdlOp {
    let name = match &cs.relation {
        Some(rv) => rangevar_name(rv),
        None => return DdlOp::Irrelevant,
    };
    // The grammar reuses inh_relations for the PARTITION OF parent (partbound set) as well as
    // plain INHERITS (no partbound).
    let partition_of = if cs.partbound.is_some() {
        cs.inh_relations.iter().find_map(|n| match n.node.as_ref() {
            Some(NodeEnum::RangeVar(rv)) => Some(rangevar_name(rv)),
            _ => None,
        })
    } else {
        None
    };
    let mut incomplete_columns = cs.of_typename.is_some()
        || (cs.partbound.is_none() && !cs.inh_relations.is_empty())
        || (cs.partbound.is_some() && partition_of.is_none());
    let mut columns = Vec::new();
    let mut pk_columns: Vec<String> = Vec::new();
    for elt in cs.table_elts.iter().chain(cs.constraints.iter()) {
        match elt.node.as_ref() {
            // A ColumnDef without a type is not a column: on a partition child the grammar
            // delivers `(col WITH OPTIONS ...)` option specs this way — they constrain
            // inherited columns and are layout-inert. Anywhere else a type-less def is
            // something we do not model, so the table is marked incomplete instead of
            // growing a phantom column of type "unknown".
            Some(NodeEnum::ColumnDef(cd)) if cd.type_name.is_some() => columns.push(map_column(cd)),
            Some(NodeEnum::ColumnDef(_)) => {
                if partition_of.is_none() {
                    incomplete_columns = true;
                }
            }
            Some(NodeEnum::Constraint(c)) => pk_columns.extend(pk_keys(c)),
            Some(NodeEnum::TableLikeClause(_)) => incomplete_columns = true,
            _ => {}
        }
    }
    DdlOp::CreateTable {
        name,
        columns,
        pk_columns,
        if_not_exists: cs.if_not_exists,
        is_ctas: false,
        incomplete_columns,
        temporary: cs.relation.as_ref().is_some_and(|rv| rv.relpersistence == "t"),
        partition_of,
    }
}

fn pk_keys(c: &pb::Constraint) -> Vec<String> {
    if c.contype() == pb::ConstrType::ConstrPrimary {
        c.keys.iter().filter_map(string_node).collect()
    } else {
        Vec::new()
    }
}

fn map_column(cd: &pb::ColumnDef) -> RawColumn {
    let constrained_not_null = cd.constraints.iter().any(|n| match n.node.as_ref() {
        Some(NodeEnum::Constraint(c)) => matches!(
            c.contype(),
            pb::ConstrType::ConstrNotnull | pb::ConstrType::ConstrPrimary | pb::ConstrType::ConstrIdentity
        ),
        _ => false,
    });
    let not_null = cd.is_not_null || !cd.identity.is_empty() || constrained_not_null;
    let type_ref = match &cd.type_name {
        Some(tn) => map_type(tn),
        None => TypeRef {
            key: "unknown".into(),
            display: "unknown".into(),
            char_len: None,
            dims: 0,
        },
    };
    RawColumn {
        display: cd.colname.clone(),
        key: cd.colname.clone(),
        type_ref,
        not_null,
    }
}

fn map_alter_table(at: &pb::AlterTableStmt) -> Vec<DdlOp> {
    if at.objtype() != pb::ObjectType::ObjectTable {
        return vec![DdlOp::Irrelevant];
    }
    let table = match &at.relation {
        Some(rv) => rangevar_name(rv),
        None => return vec![DdlOp::Irrelevant],
    };
    at.cmds
        .iter()
        .flat_map(|n| match n.node.as_ref() {
            Some(NodeEnum::AlterTableCmd(cmd)) => map_alter_cmd(&table, cmd),
            _ => vec![DdlOp::Irrelevant],
        })
        .collect()
}

fn map_alter_cmd(table: &RawName, cmd: &pb::AlterTableCmd) -> Vec<DdlOp> {
    match cmd.subtype() {
        pb::AlterTableType::AtAddColumn => match cmd_column_def(cmd) {
            Some(cd) => vec![DdlOp::AddColumn {
                table: table.clone(),
                column: map_column(cd),
                if_not_exists: cmd.missing_ok,
            }],
            None => vec![DdlOp::Irrelevant],
        },
        pb::AlterTableType::AtDropColumn => {
            vec![DdlOp::DropColumns {
                table: table.clone(),
                columns: vec![cmd.name.clone()],
                if_exists: cmd.missing_ok,
            }]
        }
        pb::AlterTableType::AtAlterColumnType => match cmd_column_def(cmd).and_then(|cd| cd.type_name.as_ref()) {
            Some(tn) => vec![DdlOp::SetColumnType {
                table: table.clone(),
                column: cmd.name.clone(),
                type_ref: map_type(tn),
            }],
            None => vec![DdlOp::Irrelevant],
        },
        pb::AlterTableType::AtSetNotNull => {
            vec![DdlOp::SetNotNull {
                table: table.clone(),
                column: cmd.name.clone(),
                value: true,
            }]
        }
        pb::AlterTableType::AtDropNotNull => {
            vec![DdlOp::SetNotNull {
                table: table.clone(),
                column: cmd.name.clone(),
                value: false,
            }]
        }
        // ADD PRIMARY KEY forces NOT NULL on its columns.
        pb::AlterTableType::AtAddConstraint => match cmd.def.as_ref().and_then(|n| n.node.as_ref()) {
            Some(NodeEnum::Constraint(c)) => pk_keys(c)
                .into_iter()
                .map(|column| DdlOp::SetNotNull {
                    table: table.clone(),
                    column,
                    value: true,
                })
                .collect(),
            _ => vec![DdlOp::Irrelevant],
        },
        _ => vec![DdlOp::Irrelevant],
    }
}

fn cmd_column_def(cmd: &pb::AlterTableCmd) -> Option<&pb::ColumnDef> {
    match cmd.def.as_ref()?.node.as_ref()? {
        NodeEnum::ColumnDef(cd) => Some(cd),
        _ => None,
    }
}

fn map_rename(rs: &pb::RenameStmt) -> Vec<DdlOp> {
    let table = rs.relation.as_ref().map(rangevar_name);
    match (rs.rename_type(), table) {
        (pb::ObjectType::ObjectColumn, Some(table)) => {
            vec![DdlOp::RenameColumn {
                table,
                old: rs.subname.clone(),
                new: rs.newname.clone(),
            }]
        }
        (pb::ObjectType::ObjectTable, Some(table)) => {
            let new = RawName {
                display: rs.newname.clone(),
                key: rs.newname.clone(),
            };
            vec![DdlOp::RenameTable { table, new }]
        }
        (pb::ObjectType::ObjectType | pb::ObjectType::ObjectDomain, _) => {
            let Some(name) = rs.object.as_ref().and_then(|n| dropped_name(n)) else {
                return vec![DdlOp::Irrelevant];
            };
            let new = RawName {
                display: rs.newname.clone(),
                key: rs.newname.clone(),
            };
            vec![DdlOp::RenameType { name, new }]
        }
        _ => vec![DdlOp::Irrelevant],
    }
}

fn map_drop(ds: &pb::DropStmt) -> Vec<DdlOp> {
    match ds.remove_type() {
        pb::ObjectType::ObjectTable => {
            let names = ds.objects.iter().filter_map(dropped_name).collect();
            vec![DdlOp::DropTables {
                names,
                if_exists: ds.missing_ok,
            }]
        }
        pb::ObjectType::ObjectType | pb::ObjectType::ObjectDomain => {
            let names = ds
                .objects
                .iter()
                .filter_map(|n| match n.node.as_ref() {
                    Some(NodeEnum::TypeName(tn)) => {
                        let parts: Vec<&str> = tn.names.iter().filter_map(string_node_ref).collect();
                        let last = parts.last()?;
                        Some(RawName {
                            display: parts.join("."),
                            key: (*last).to_string(),
                        })
                    }
                    _ => None,
                })
                .collect();
            vec![DdlOp::DropTypes { names }]
        }
        _ => vec![DdlOp::Irrelevant],
    }
}

fn map_ctas(ctas: &pb::CreateTableAsStmt) -> Vec<DdlOp> {
    if ctas.objtype() != pb::ObjectType::ObjectTable {
        return vec![DdlOp::Irrelevant];
    }
    match ctas.into.as_ref().and_then(|into| into.rel.as_ref()) {
        Some(rv) => vec![DdlOp::CreateTable {
            name: rangevar_name(rv),
            columns: Vec::new(),
            pk_columns: Vec::new(),
            if_not_exists: ctas.if_not_exists,
            is_ctas: true,
            incomplete_columns: false,
            temporary: rv.relpersistence == "t",
            partition_of: None,
        }],
        None => vec![DdlOp::Irrelevant],
    }
}

fn map_type(tn: &pb::TypeName) -> TypeRef {
    let parts: Vec<&str> = tn.names.iter().filter_map(string_node_ref).collect();
    let qualified = parts.len() > 1;
    let last = parts.last().copied().unwrap_or("");
    let dims = tn.array_bounds.len().min(u8::MAX as usize) as u8;
    let (key, default_char_len) = match last {
        // The catalog name of the quoted one-byte `"char"` type is literally `char`; the keyword
        // `char` never arrives here (the grammar maps it to pg_catalog.bpchar).
        "char" => ("pgchar".to_string(), None),
        // Keyword-form `char` / `character` without a length means char(1); a literal unqualified
        // `bpchar` means unlimited.
        "bpchar" if qualified => ("bpchar".to_string(), Some(1)),
        other => (other.to_string(), None),
    };
    let char_len = if dims == 0 && matches!(key.as_str(), "varchar" | "bpchar") {
        first_int_typmod(&tn.typmods).or(default_char_len)
    } else {
        None
    };
    let display = render_display(&parts, &tn.typmods, dims);
    TypeRef {
        key,
        display,
        char_len,
        dims,
    }
}

fn first_int_typmod(typmods: &[pb::Node]) -> Option<u64> {
    match typmods.first()?.node.as_ref()? {
        NodeEnum::AConst(ac) => match ac.val.as_ref()? {
            pb::a_const::Val::Ival(i) if i.ival >= 0 => Some(i.ival as u64),
            _ => None,
        },
        _ => None,
    }
}

fn render_display(parts: &[&str], typmods: &[pb::Node], dims: u8) -> String {
    let base = match parts {
        ["pg_catalog", name] => (*name).to_string(),
        _ => parts.join("."),
    };
    let mods: Vec<String> = typmods
        .iter()
        .filter_map(|n| match n.node.as_ref() {
            Some(NodeEnum::AConst(ac)) => match ac.val.as_ref() {
                Some(pb::a_const::Val::Ival(i)) => Some(i.ival.to_string()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    let with_mods = if mods.is_empty() {
        base
    } else {
        format!("{}({})", base, mods.join(","))
    };
    format!("{}{}", with_mods, "[]".repeat(dims as usize))
}

/// Table identity keeps its qualification (the scanner already folded unquoted parts).
fn rangevar_name(rv: &pb::RangeVar) -> RawName {
    let (display, key) = if rv.schemaname.is_empty() {
        (rv.relname.clone(), rv.relname.clone())
    } else {
        let joined = format!("{}.{}", rv.schemaname, rv.relname);
        (joined.clone(), joined)
    };
    RawName { display, key }
}

/// Type names resolve unqualified in the catalog — last component only.
fn type_rangevar_name(rv: &pb::RangeVar) -> RawName {
    let display = if rv.schemaname.is_empty() {
        rv.relname.clone()
    } else {
        format!("{}.{}", rv.schemaname, rv.relname)
    };
    RawName {
        display,
        key: rv.relname.clone(),
    }
}

fn name_from_nodes(nodes: &[pb::Node]) -> RawName {
    let parts: Vec<&str> = nodes.iter().filter_map(string_node_ref).collect();
    let key = parts.last().copied().unwrap_or("").to_string();
    RawName {
        display: parts.join("."),
        key,
    }
}

fn dropped_name(n: &pb::Node) -> Option<RawName> {
    match n.node.as_ref()? {
        NodeEnum::List(list) => {
            let parts: Vec<&str> = list.items.iter().filter_map(string_node_ref).collect();
            if parts.is_empty() {
                return None;
            }
            let joined = parts.join(".");
            Some(RawName {
                display: joined.clone(),
                key: joined,
            })
        }
        NodeEnum::String(s) => Some(RawName {
            display: s.sval.clone(),
            key: s.sval.clone(),
        }),
        _ => None,
    }
}

fn string_node_ref(n: &pb::Node) -> Option<&str> {
    match n.node.as_ref()? {
        NodeEnum::String(s) => Some(&s.sval),
        _ => None,
    }
}

fn string_node(n: &pb::Node) -> Option<String> {
    string_node_ref(n).map(str::to_string)
}

#[cfg(test)]
mod tests;
