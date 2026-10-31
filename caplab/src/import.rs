//! 取り込み: CREATE TABLE → かんたんスキーマ、クエリログ / 統計ビュー → クエリと頻度、外部メトリクス（CSV）。
use crate::model::*;
use anyhow::{anyhow, Result};
use serde::Serialize;
use sqlparser::ast::{ColumnOption, DataType, Statement, TableConstraint};
use sqlparser::dialect::{GenericDialect, MySqlDialect, PostgreSqlDialect};
use sqlparser::parser::Parser;
use std::collections::BTreeMap;

#[derive(Serialize, Default)]
pub struct DdlImport {
    pub tables: Vec<TableSpec>,
    pub warnings: Vec<String>,
}

fn type_bytes(t: &DataType) -> usize {
    let s = t.to_string().to_ascii_lowercase();
    let num = |s: &str| -> usize { s.split(|c: char| !c.is_ascii_digit()).filter(|x| !x.is_empty()).next().and_then(|x| x.parse().ok()).unwrap_or(0) };
    if s.starts_with("tinyint") || s.starts_with("bool") {
        1
    } else if s.starts_with("smallint") {
        2
    } else if s.starts_with("bigint") || s.starts_with("double") || s.starts_with("timestamp") || s.starts_with("datetime") || s.starts_with("decimal") || s.starts_with("numeric") {
        8
    } else if s.starts_with("int") || s.starts_with("float") || s.starts_with("real") || s.starts_with("date") || s.starts_with("serial") {
        4
    } else if s.starts_with("varchar") || s.starts_with("character varying") || s.starts_with("char") {
        let n = num(&s);
        if n == 0 { 50 } else { (n / 2).max(1) } // 平均は上限の半分と仮定
    } else if s.starts_with("json") || s.starts_with("text") || s.starts_with("blob") || s.starts_with("bytea") || s.starts_with("longtext") || s.starts_with("mediumtext") {
        500
    } else {
        16
    }
}

/// `CREATE TABLE` 群を解析して、かんたんスキーマの表に近似する。
/// 親子は外部キー（無ければ `xxx_id` という列名と表名の対応）から推定する。件数は `rows`（表名 → 件数）で上書きできる。
pub fn import_ddl(engine: EngineKind, ddl: &str, rows: &BTreeMap<String, u32>) -> Result<DdlImport> {
    let stmts = match engine {
        EngineKind::Postgres => Parser::parse_sql(&PostgreSqlDialect {}, ddl),
        EngineKind::Mysql => Parser::parse_sql(&MySqlDialect {}, ddl),
    }
    .or_else(|_| Parser::parse_sql(&GenericDialect {}, ddl))
    .map_err(|e| anyhow!("DDL を解析できません: {e}"))?;
    let mut out = DdlImport::default();
    struct T {
        name: String,
        cols: Vec<(String, usize)>,
        fk: Option<(String, String)>, // (子の列, 親の表)
    }
    let mut ts: Vec<T> = vec![];
    for s in stmts {
        let Statement::CreateTable(ct) = s else {
            out.warnings.push("CREATE TABLE 以外の文は無視しました".into());
            continue;
        };
        let name = ct.name.0.last().map(|i| i.value.to_ascii_lowercase()).unwrap_or_default();
        let mut cols = vec![];
        let mut fk = None;
        for c in &ct.columns {
            cols.push((c.name.value.to_ascii_lowercase(), type_bytes(&c.data_type)));
            for o in &c.options {
                if let ColumnOption::ForeignKey { foreign_table, .. } = &o.option {
                    fk = fk.or(Some((c.name.value.to_ascii_lowercase(), foreign_table.0.last().map(|i| i.value.to_ascii_lowercase()).unwrap_or_default())));
                }
            }
        }
        for c in &ct.constraints {
            if let TableConstraint::ForeignKey { columns, foreign_table, .. } = c {
                if let Some(col) = columns.first() {
                    fk = fk.or(Some((col.value.to_ascii_lowercase(), foreign_table.0.last().map(|i| i.value.to_ascii_lowercase()).unwrap_or_default())));
                }
            }
        }
        ts.push(T { name, cols, fk });
    }
    if ts.is_empty() {
        return Err(anyhow!("CREATE TABLE が見つかりません"));
    }
    let names: Vec<String> = ts.iter().map(|t| t.name.clone()).collect();
    for t in &ts {
        // 外部キーが無いときは、`<表名>_id` / `<表名の単数形>_id` の列から親を推定する
        let mut parent = t.fk.as_ref().map(|(_, p)| p.clone());
        let mut parent_col = t.fk.as_ref().map(|(c, _)| c.clone());
        if parent.is_none() {
            for (c, _) in &t.cols {
                if let Some(base) = c.strip_suffix("_id") {
                    if let Some(p) = names.iter().find(|n| n.as_str() == base || n.trim_end_matches('s') == base || **n == format!("{base}s")) {
                        if *p != t.name {
                            out.warnings.push(format!("'{}' の親を列名 '{c}' から '{p}' と推定しました（外部キーがありません。違えば直してください）", t.name));
                            parent = Some(p.clone());
                            parent_col = Some(c.clone());
                            break;
                        }
                    }
                }
            }
        }
        let parent = parent.filter(|p| names.contains(p));
        let skip = |c: &str| c == "id" || c == "tenant_id" || Some(c) == parent_col.as_deref() && parent.is_some();
        let kept: Vec<&(String, usize)> = t.cols.iter().filter(|(c, _)| !skip(c)).collect();
        let bytes: usize = 12 + kept.iter().map(|(_, b)| *b).sum::<usize>();
        let size = if bytes < 300 { Size::S } else if bytes < 3000 { Size::M } else { Size::L };
        let default_rows = if parent.is_some() { 5 } else { 20 };
        out.tables.push(TableSpec { name: t.name.clone(), parent, columns: kept.len().min(60) as u32, size, rows: rows.get(&t.name).copied().unwrap_or(default_rows) });
        if !t.cols.iter().any(|(c, _)| c == "tenant_id") {
            out.warnings.push(format!("'{}' に tenant_id 列がありません。生成する表には自動で tenant_id（主キーの先頭）を付けます", t.name));
        }
    }
    // 親が循環していないか / 深さ
    out.warnings.push("実際のデータは、列の型に合わせた自動生成の表（かんたんスキーマ）に置き換わります。元の DDL そのものは作りません（インデックス・制約・方言固有の型は反映されません）".into());
    Ok(out)
}

/// クエリの正規化: リテラルや `$1` / `?` を `:rand_any` に置き換える
pub fn normalize(sql: &str) -> String {
    let mut out = String::new();
    let b: Vec<char> = sql.trim().trim_end_matches(';').chars().collect();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == '\'' {
            i += 1;
            while i < b.len() && b[i] != '\'' {
                i += 1;
            }
            i += 1;
            out.push_str(":rand_any");
        } else if c == '$' && i + 1 < b.len() && b[i + 1].is_ascii_digit() {
            i += 1;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            out.push_str(":rand_any");
        } else if c == '?' {
            out.push_str(":rand_any");
            i += 1;
        } else if c.is_ascii_digit() && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == '_' || b[i - 1] == '.')) {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_digit() || b[j] == '.') {
                j += 1;
            }
            // LIMIT 10 のような件数はそのまま残す
            let prev: String = out.trim_end().rsplit(' ').next().unwrap_or("").to_ascii_lowercase();
            if prev == "limit" || prev == "offset" || prev == "interval" {
                out.extend(&b[i..j]);
            } else {
                out.push_str(":rand_any");
            }
            i = j;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Serialize, Clone)]
pub struct Imported {
    pub name: String,
    pub sql: String,
    pub calls: f64,
    pub write: bool,
}

/// 貼り付けたログから、(正規化した SQL, 回数) を作る。`-- calls=N` が文の前の行にあればその回数、無ければ出現回数
pub fn from_text(text: &str) -> Vec<Imported> {
    let mut counts: BTreeMap<String, f64> = BTreeMap::new();
    let mut pending: Option<f64> = None;
    let mut buf = String::new();
    let mut flush = |buf: &mut String, pending: &mut Option<f64>, counts: &mut BTreeMap<String, f64>| {
        let s = buf.trim();
        if !s.is_empty() {
            *counts.entry(normalize(s)).or_default() += pending.take().unwrap_or(1.0);
        }
        buf.clear();
    };
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("--") {
            if let Some(n) = rest.trim().strip_prefix("calls=").and_then(|x| x.trim().parse::<f64>().ok()) {
                flush(&mut buf, &mut pending, &mut counts);
                pending = Some(n);
            }
            continue;
        }
        buf.push_str(line);
        buf.push(' ');
        if t.ends_with(';') {
            flush(&mut buf, &mut pending, &mut counts);
        }
    }
    flush(&mut buf, &mut pending, &mut counts);
    let mut v: Vec<(String, f64)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    v.into_iter().enumerate().map(|(i, (sql, calls))| {
        let l = sql.trim_start().to_ascii_lowercase();
        Imported { name: format!("imp_{}", i + 1), write: l.starts_with("update") || l.starts_with("insert") || l.starts_with("delete"), sql, calls }
    }).collect()
}

/// 取り込んだクエリを、プロジェクトのクエリと actor（1 クエリ 1 actor、頻度は回数 ÷ 期間 ÷ テナント数）にする
pub fn apply_imported(p: &mut Project, items: &[Imported], window_secs: f64) {
    p.actors.retain(|a| !a.name.starts_with("imp-"));
    p.queries.retain(|k, _| !k.starts_with("imp_"));
    for it in items {
        p.queries.insert(it.name.clone(), QueryDef { sql: it.sql.clone(), write: it.write });
        p.actors.push(Actor {
            name: format!("imp-{}", &it.name[4..]),
            kind: Kind::Web,
            every_ms: None,
            instances: 1,
            qps_per_tenant: Some(it.calls / window_secs.max(1.0) / p.tenants.max(1) as f64),
            stagger: false,
            concurrency: None,
            tenant_limit: None,
            pool_max_open: 5,
            target: Target::Writer,
            tenant_skew: None,
            shape: None,
            hot_rows: None,
            connections_per_tenant: None,
            singleflight: false,
            tx: false,
            steps: vec![Step { id: None, query: it.name.clone(), foreach: None, limit: None, repeat: None, cache_hit: None }],
        });
    }
}

/// CSV（`時刻,値`）から (秒, 値) を読む。時刻は、実行開始からの秒 または UNIX 秒
pub fn parse_csv(csv: &str, run_start_unix: f64) -> Result<Vec<(f64, f64)>> {
    let mut out = vec![];
    for (i, l) in csv.lines().enumerate() {
        let l = l.trim();
        if l.is_empty() {
            continue;
        }
        let mut it = l.split(',');
        let (a, b) = (it.next().unwrap_or("").trim(), it.next().unwrap_or("").trim());
        let (Ok(t), Ok(v)) = (a.parse::<f64>(), b.parse::<f64>()) else {
            if i == 0 { continue } else { return Err(anyhow!("{} 行目を読めません: '{l}'（`時刻,値` の形式）", i + 1)) }
        };
        out.push(((if t > 1e9 { t - run_start_unix } else { t }), v));
    }
    if out.is_empty() {
        return Err(anyhow!("データがありません"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_literals_and_placeholders() {
        assert_eq!(normalize("SELECT id FROM t WHERE a = 5 AND b = 'x' AND c = $1 AND d = ? LIMIT 10;"), "SELECT id FROM t WHERE a = :rand_any AND b = :rand_any AND c = :rand_any AND d = :rand_any LIMIT 10");
        // 識別子の中の数字は触らない
        assert_eq!(normalize("select c1 from t2 where id = 7"), "select c1 from t2 where id = :rand_any");
    }

    #[test]
    fn text_import_counts_and_writes() {
        let v = from_text("-- calls=100\nSELECT 1 FROM a WHERE id = 1;\nUPDATE a SET x = 1 WHERE id = 2;\nUPDATE a SET x = 9 WHERE id = 3;");
        assert_eq!(v[0].calls, 100.0);
        let upd = v.iter().find(|x| x.write).unwrap();
        assert_eq!(upd.calls, 2.0, "同じ形の文は正規化して 1 つにまとまる");
    }

    #[test]
    fn ddl_foreign_key_and_inference() {
        let r = import_ddl(EngineKind::Postgres, "CREATE TABLE a (id int primary key, n text); CREATE TABLE b (id int primary key, a_id int references a(id), v int); CREATE TABLE c (id int primary key, b_id int, w int);", &Default::default()).unwrap();
        let p = |n: &str| r.tables.iter().find(|t| t.name == n).unwrap().parent.clone();
        assert_eq!(p("a"), None);
        assert_eq!(p("b"), Some("a".into()), "外部キーから");
        assert_eq!(p("c"), Some("b".into()), "列名 b_id から推定");
        assert!(r.warnings.iter().any(|w| w.contains("推定")));
    }

    #[test]
    fn csv_relative_and_epoch() {
        let v = parse_csv("t,v\n0,1\n1700000010,2", 1_700_000_000.0).unwrap();
        assert_eq!(v, vec![(0.0, 1.0), (10.0, 2.0)]);
        assert!(parse_csv("1,2\nfoo", 0.0).is_err());
    }

    #[test]
    fn rewrite_dsn_both_engines() {
        let a: std::net::SocketAddr = "127.0.0.1:9999".parse().unwrap();
        assert_eq!(crate::net::rewrite_dsn(EngineKind::Mysql, "mysql://u:p@db.example:3306/caplab_x?a=b", a), "mysql://u:p@127.0.0.1:9999/caplab_x?a=b");
        assert!(crate::net::rewrite_dsn(EngineKind::Postgres, "host=db port=5432 user=x dbname=y", a).contains("port=9999"));
        assert_eq!(crate::net::host_port(EngineKind::Postgres, "host=db port=6000 user=x"), ("db".into(), 6000));
    }
}
