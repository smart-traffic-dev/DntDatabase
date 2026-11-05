//! What a command does, for the guard (spec §4.4, §4.4.1): its class, targets, drops
//! and `unbounded`. The guard acts only on this, so it fails closed: a `runCommand` name
//! outside the table is `Unknown`, and an `update` / `delete` it cannot read is
//! unbounded.

use dnt_core::command::{
    CommandClass, CommandDrop, CommandDropKind, CommandSessionEffect, CommandTarget,
};
use mongodb::bson::{Bson, Document};

use super::literal::integral_value;
use super::parse::{CollMethod, DbMethod, ShellCommand, Verbosity};

/// The guard's view of one command (the fields of `ParsedCommand` it fills).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    /// The method (`find`, `runCommand`), `use` or `show`.
    pub name: String,
    pub class: CommandClass,
    pub targets: Vec<CommandTarget>,
    pub unbounded: bool,
    pub drops: Vec<CommandDrop>,
    pub session_effect: Option<CommandSessionEffect>,
}

/// The pipeline stage that makes an aggregation write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutKind {
    /// `$out`: replaces the whole destination collection.
    Out,
    /// `$merge`.
    Merge,
}

/// The first `$out` / `$merge` of `pipeline` at any depth (spec §4.4.2): the stages
/// themselves first, then the pipelines nested in `$facet`, `$lookup.pipeline` and
/// `$unionWith.pipeline`, recursively. The target is the destination; `database: None`
/// when the stage names no database, and an empty name when the destination cannot be
/// read.
pub fn has_out_or_merge(pipeline: &[Bson]) -> Option<(OutKind, CommandTarget)> {
    find_out(pipeline, 0)
}

/// Pipelines nested deeper than this are reported as writing (fail closed).
const MAX_PIPELINE_DEPTH: usize = 64;

fn find_out(pipeline: &[Bson], depth: usize) -> Option<(OutKind, CommandTarget)> {
    if depth > MAX_PIPELINE_DEPTH {
        return Some((OutKind::Out, unnamed(None)));
    }
    let stages = || {
        pipeline.iter().filter_map(|s| match s {
            Bson::Document(d) => Some(d),
            _ => None,
        })
    };
    for stage in stages() {
        if let Some(v) = stage.get("$out") {
            return Some((OutKind::Out, out_destination(v)));
        }
        if let Some(v) = stage.get("$merge") {
            return Some((OutKind::Merge, merge_destination(v)));
        }
    }
    let nested = |p: &Bson| match p {
        Bson::Array(p) => find_out(p, depth + 1),
        _ => None,
    };
    for stage in stages() {
        for (key, v) in stage {
            let found = match (key.as_str(), v) {
                ("$facet", Bson::Document(facets)) => facets.values().find_map(&nested),
                ("$lookup" | "$unionWith", Bson::Document(spec)) => {
                    spec.get("pipeline").and_then(nested)
                }
                _ => None,
            };
            if found.is_some() {
                return found;
            }
        }
    }
    None
}

fn unnamed(database: Option<String>) -> CommandTarget {
    CommandTarget {
        database,
        name: String::new(),
    }
}

fn string_of(v: Option<&Bson>) -> String {
    v.and_then(Bson::as_str).unwrap_or_default().to_string()
}

/// `{db, coll}`.
fn namespace(d: &Document) -> CommandTarget {
    CommandTarget {
        database: d.get("db").and_then(Bson::as_str).map(str::to_string),
        name: string_of(d.get("coll")),
    }
}

/// `$out: "coll"` or `$out: {db, coll}`.
fn out_destination(v: &Bson) -> CommandTarget {
    match v {
        Bson::String(c) => CommandTarget {
            database: None,
            name: c.clone(),
        },
        Bson::Document(d) => namespace(d),
        _ => unnamed(None),
    }
}

/// `$merge: "coll"`, `$merge: {into: "coll"}` or `$merge: {into: {db, coll}}`.
fn merge_destination(v: &Bson) -> CommandTarget {
    match v {
        Bson::Document(d) => match d.get("into") {
            Some(into @ Bson::String(_)) => out_destination(into),
            Some(Bson::Document(ns)) => namespace(ns),
            _ => unnamed(None),
        },
        other => out_destination(other),
    }
}

/// What a command does, before it is named.
struct Effect {
    class: CommandClass,
    targets: Vec<CommandTarget>,
    unbounded: bool,
    drops: Vec<CommandDrop>,
}

impl Effect {
    fn new(class: CommandClass, targets: Vec<CommandTarget>) -> Self {
        Self {
            class,
            targets,
            unbounded: false,
            drops: Vec::new(),
        }
    }

    fn dropping(mut self, kind: CommandDropKind, target: CommandTarget) -> Self {
        self.drops.push(CommandDrop { kind, target });
        self
    }

    /// An aggregation over `pipeline` in `db`: a write when it has `$out` / `$merge`,
    /// whose destination becomes a target (and a drop for `$out`).
    fn aggregate(mut self, pipeline: &[Bson], db: &Option<String>) -> Self {
        if let Some((kind, mut dest)) = has_out_or_merge(pipeline) {
            if dest.database.is_none() {
                dest.database = db.clone();
            }
            self.class = CommandClass::Write;
            if !self.targets.contains(&dest) {
                self.targets.push(dest.clone());
            }
            if kind == OutKind::Out {
                self = self.dropping(CommandDropKind::Object, dest);
            }
        }
        self
    }
}

/// The target of a whole database: its name, or empty for the database in effect when
/// the command runs.
fn database_target(db: &Option<String>) -> CommandTarget {
    CommandTarget {
        database: db.clone(),
        name: db.clone().unwrap_or_default(),
    }
}

/// As the server reads a flag: absent, `null`, `false` and `0` are false, anything else
/// is true.
fn truthy(v: Option<&Bson>) -> bool {
    match v {
        None | Some(Bson::Null | Bson::Undefined) => false,
        Some(Bson::Boolean(b)) => *b,
        Some(Bson::Int32(n)) => *n != 0,
        Some(Bson::Int64(n)) => *n != 0,
        Some(Bson::Double(f)) => *f != 0.0,
        Some(_) => true,
    }
}

/// A filter that matches every document: missing, `{}`, or not a document at all.
fn open_filter(q: Option<&Bson>) -> bool {
    match q {
        Some(Bson::Document(d)) => d.is_empty(),
        _ => true,
    }
}

/// An `updates` / `deletes` list with a statement that hits every document; a list
/// that cannot be read counts as one.
fn any_unbounded(list: Option<&Bson>, open: impl Fn(&Document) -> bool) -> bool {
    match list {
        Some(Bson::Array(items)) => items.iter().any(|i| match i {
            Bson::Document(d) => open(d),
            _ => true,
        }),
        _ => true,
    }
}

fn only_query_planner(method: &str) -> String {
    format!(r#"Only "queryPlanner" explain is allowed for {method}()"#)
}

/// `runCommand` / `adminCommand` in `db` (spec §4.4.1): by the first key.
fn run_command(cmd: &Document, db: &Option<String>) -> Result<Effect, String> {
    use CommandClass::*;
    let Some((key, value)) = cmd.iter().next() else {
        return Ok(Effect::new(Unknown, Vec::new()));
    };
    let target = value.as_str().map(|name| CommandTarget {
        database: db.clone(),
        name: name.to_string(),
    });
    let targets: Vec<CommandTarget> = target.iter().cloned().collect();
    Ok(match key.as_str() {
        "ping" | "hello" | "isMaster" | "buildInfo" | "serverStatus" | "dbStats" | "collStats"
        | "listDatabases" | "listCollections" | "listIndexes" | "find" | "count" | "distinct"
        | "getMore" | "connectionStatus" | "hostInfo" | "getParameter" | "currentOp"
        | "dataSize" => Effect::new(Read, targets),
        "aggregate" => match cmd.get("pipeline") {
            Some(Bson::Array(p)) => Effect::new(Read, targets).aggregate(p, db),
            _ => Effect::new(Unknown, targets),
        },
        "explain" => match value {
            Bson::Document(inner) => {
                let e = run_command(inner, db)?;
                let planner = cmd.get("verbosity").and_then(Bson::as_str) == Some("queryPlanner");
                if e.class != Read && !planner {
                    let name = inner.keys().next().map_or("", String::as_str);
                    return Err(only_query_planner(name));
                }
                e
            }
            _ => Effect::new(Unknown, targets),
        },
        "insert" | "findAndModify" => Effect::new(Write, targets),
        "update" => Effect {
            unbounded: any_unbounded(cmd.get("updates"), |u| {
                open_filter(u.get("q")) && truthy(u.get("multi"))
            }),
            ..Effect::new(Write, targets)
        },
        "delete" => Effect {
            // `limit` is 0 (all) or 1; anything but 1 counts as all.
            unbounded: any_unbounded(cmd.get("deletes"), |d| {
                open_filter(d.get("q")) && d.get("limit").and_then(integral_value) != Some(1)
            }),
            ..Effect::new(Write, targets)
        },
        "create" | "createIndexes" | "dropIndexes" | "collMod" => Effect::new(Ddl, targets),
        "drop" => {
            let dropped = target.unwrap_or_else(|| unnamed(db.clone()));
            Effect::new(Ddl, targets).dropping(CommandDropKind::Object, dropped)
        }
        "dropDatabase" => {
            let t = database_target(db);
            Effect::new(Ddl, vec![t.clone()]).dropping(CommandDropKind::Database, t)
        }
        "renameCollection" => {
            let e = Effect::new(Ddl, targets);
            if truthy(cmd.get("dropTarget")) {
                let to = CommandTarget {
                    database: db.clone(),
                    name: string_of(cmd.get("to")),
                };
                e.dropping(CommandDropKind::Object, to)
            } else {
                e
            }
        }
        "validate"
        | "killOp"
        | "killCursors"
        | "setParameter"
        | "createUser"
        | "updateUser"
        | "dropUser"
        | "grantRolesToUser"
        | "revokeRolesFromUser"
        | "createRole"
        | "dropRole"
        | "shutdown"
        | "fsync"
        | "compact"
        | "replSetStepDown"
        | "replSetReconfig" => Effect::new(Admin, targets),
        _ => Effect::new(Unknown, targets),
    })
}

/// A collection method on `coll` in `db` (spec §4.4).
fn coll_effect(db: &Option<String>, coll: &str, method: CollMethod, args: &[Bson]) -> Effect {
    use CollMethod::*;
    use CommandClass::*;
    let target = CommandTarget {
        database: db.clone(),
        name: coll.to_string(),
    };
    let e = Effect::new(Read, vec![target.clone()]);
    match method {
        Find
        | FindOne
        | CountDocuments
        | EstimatedDocumentCount
        | Distinct
        | GetIndexes
        | Stats => e,
        Aggregate => match args.first() {
            Some(Bson::Array(p)) => e.aggregate(p, db),
            _ => e,
        },
        InsertOne | InsertMany | UpdateOne | ReplaceOne | DeleteOne | FindOneAndUpdate
        | FindOneAndReplace | FindOneAndDelete => Effect { class: Write, ..e },
        UpdateMany | DeleteMany => Effect {
            class: Write,
            unbounded: open_filter(args.first()),
            ..e
        },
        CreateIndex | CreateIndexes | DropIndex | DropIndexes => Effect { class: Ddl, ..e },
        RenameCollection => {
            let e = Effect { class: Ddl, ..e };
            if truthy(args.get(1)) {
                let to = CommandTarget {
                    database: db.clone(),
                    name: string_of(args.first()),
                };
                e.dropping(CommandDropKind::Object, to)
            } else {
                e
            }
        }
        Drop => Effect { class: Ddl, ..e }.dropping(CommandDropKind::Object, target),
    }
}

/// The classification of `cmd`. `Err`: an explain verbosity other than `queryPlanner`
/// for a command that is not Read (§4.2).
pub fn classify(cmd: &ShellCommand) -> Result<Classification, String> {
    let named = |name: &str, e: Effect| Classification {
        name: name.to_string(),
        class: e.class,
        targets: e.targets,
        unbounded: e.unbounded,
        drops: e.drops,
        session_effect: None,
    };
    Ok(match cmd {
        ShellCommand::Use(db) => Classification {
            session_effect: Some(CommandSessionEffect::UseDatabase(db.clone())),
            ..named("use", Effect::new(CommandClass::SessionChange, Vec::new()))
        },
        ShellCommand::Show(_) => named("show", Effect::new(CommandClass::Read, Vec::new())),
        ShellCommand::Db {
            db, method, args, ..
        } => {
            let db = match method {
                DbMethod::AdminCommand => Some("admin".to_string()),
                _ => db.clone(),
            };
            let e = match method {
                DbMethod::GetCollectionNames
                | DbMethod::Stats
                | DbMethod::ServerStatus
                | DbMethod::Version => Effect::new(CommandClass::Read, Vec::new()),
                DbMethod::CreateCollection => {
                    let t = CommandTarget {
                        database: db.clone(),
                        name: string_of(args.first()),
                    };
                    Effect::new(CommandClass::Ddl, vec![t])
                }
                DbMethod::DropDatabase => {
                    let t = database_target(&db);
                    Effect::new(CommandClass::Ddl, vec![t.clone()])
                        .dropping(CommandDropKind::Database, t)
                }
                DbMethod::RunCommand | DbMethod::AdminCommand => match args.first() {
                    Some(Bson::Document(d)) => run_command(d, &db)?,
                    _ => Effect::new(CommandClass::Unknown, Vec::new()),
                },
            };
            named(method.name(), e)
        }
        ShellCommand::Coll {
            db,
            coll,
            method,
            args,
            explain,
            ..
        } => {
            let e = coll_effect(db, coll, *method, args);
            if explain.is_some_and(|v| v != Verbosity::QueryPlanner)
                && e.class != CommandClass::Read
            {
                return Err(only_query_planner(method.name()));
            }
            named(method.name(), e)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::{parse_document, MongoLanguage};
    use dnt_core::command::{CommandLanguage, ParsedCommand};
    use CommandClass::*;

    fn parsed(text: &str) -> ParsedCommand {
        MongoLanguage
            .parse(text)
            .unwrap_or_else(|e| panic!("{text}: {e:?}"))
    }

    fn err(text: &str) -> (String, Option<usize>) {
        match MongoLanguage.parse(text) {
            Ok(c) => panic!("{text}: parsed as {c:?}"),
            Err(e) => (e.message, e.offset),
        }
    }

    fn t(db: Option<&str>, name: &str) -> CommandTarget {
        CommandTarget {
            database: db.map(str::to_string),
            name: name.into(),
        }
    }

    fn c(name: &str) -> CommandTarget {
        t(None, name)
    }

    fn obj(db: Option<&str>, name: &str) -> CommandDrop {
        CommandDrop {
            kind: CommandDropKind::Object,
            target: t(db, name),
        }
    }

    fn database(db: Option<&str>) -> CommandDrop {
        CommandDrop {
            kind: CommandDropKind::Database,
            target: t(db, db.unwrap_or_default()),
        }
    }

    type Facts = (CommandClass, Vec<CommandTarget>, Vec<CommandDrop>, bool);

    fn facts(text: &str) -> Facts {
        let p = parsed(text);
        (p.class, p.targets, p.drops, p.unbounded)
    }

    fn check(rows: Vec<(String, Facts)>) {
        for (text, want) in rows {
            assert_eq!(facts(&text), want, "{text}");
        }
    }

    fn pipeline(text: &str) -> Vec<Bson> {
        match parse_document(&format!("{{p: {text}}}"))
            .unwrap()
            .remove("p")
        {
            Some(Bson::Array(p)) => p,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn classification_table() {
        let none = || (vec![], vec![], false);
        let mut rows: Vec<(String, Facts)> = Vec::new();
        let mut row =
            |text: &str, class: CommandClass, f: (Vec<CommandTarget>, Vec<CommandDrop>, bool)| {
                rows.push((text.to_string(), (class, f.0, f.1, f.2)));
            };
        for text in [
            "db.c.find({})",
            "db.c.findOne()",
            "db.c.countDocuments({})",
            "db.c.estimatedDocumentCount()",
            "db.c.distinct('a')",
            "db.c.getIndexes()",
            "db.c.stats()",
            "db.c.find().sort({a: 1}).limit(1)",
            "db.c.find().count()",
            "db.c.aggregate([{$match: {a: 1}}, {$group: {_id: '$a'}}])",
        ] {
            row(text, Read, (vec![c("c")], vec![], false));
        }
        for text in [
            "show dbs",
            "show collections",
            "db.getCollectionNames()",
            "db.stats()",
            "db.serverStatus()",
            "db.version()",
        ] {
            row(text, Read, none());
        }
        row(
            "db.c.aggregate([{$match: {}}, {$out: 'dest'}])",
            Write,
            (vec![c("c"), c("dest")], vec![obj(None, "dest")], false),
        );
        row(
            "db.c.aggregate([{$out: {db: 'other', coll: 'dest'}}])",
            Write,
            (
                vec![c("c"), t(Some("other"), "dest")],
                vec![obj(Some("other"), "dest")],
                false,
            ),
        );
        for text in [
            "db.c.aggregate([{$merge: {into: 'dest'}}])",
            "db.c.aggregate([{$merge: 'dest'}])",
        ] {
            row(text, Write, (vec![c("c"), c("dest")], vec![], false));
        }
        row(
            "db.c.aggregate([{$merge: {into: {db: 'o', coll: 'd'}, on: '_id'}}])",
            Write,
            (vec![c("c"), t(Some("o"), "d")], vec![], false),
        );
        for text in [
            "db.c.insertOne({a: 1})",
            "db.c.insertMany([{a: 1}])",
            "db.c.updateOne({}, {$set: {a: 1}})",
            "db.c.updateMany({a: 1}, {$set: {b: 1}})",
            "db.c.replaceOne({}, {a: 1})",
            "db.c.deleteOne({})",
            "db.c.deleteMany({a: 1})",
            "db.c.findOneAndUpdate({}, {$set: {a: 1}})",
            "db.c.findOneAndReplace({}, {a: 1})",
            "db.c.findOneAndDelete({})",
        ] {
            row(text, Write, (vec![c("c")], vec![], false));
        }
        row(
            "db.c.updateMany({}, {$set: {a: 1}})",
            Write,
            (vec![c("c")], vec![], true),
        );
        row("db.c.deleteMany({})", Write, (vec![c("c")], vec![], true));
        row(
            "db.createCollection('n')",
            Ddl,
            (vec![c("n")], vec![], false),
        );
        for text in [
            "db.c.createIndex({a: 1})",
            "db.c.createIndexes([{a: 1}])",
            "db.c.dropIndex('a_1')",
            "db.c.dropIndexes()",
            "db.c.renameCollection('d')",
            "db.c.renameCollection('d', false)",
        ] {
            row(text, Ddl, (vec![c("c")], vec![], false));
        }
        row(
            "db.c.renameCollection('d', true)",
            Ddl,
            (vec![c("c")], vec![obj(None, "d")], false),
        );
        row(
            "db.c.drop()",
            Ddl,
            (vec![c("c")], vec![obj(None, "c")], false),
        );
        row(
            "db.dropDatabase()",
            Ddl,
            (vec![t(None, "")], vec![database(None)], false),
        );
        row("use other", SessionChange, none());
        // Explain: the class, targets, drops and unbounded of the explained command.
        row(
            "db.c.explain().deleteMany({})",
            Write,
            (vec![c("c")], vec![], true),
        );
        row(
            "db.c.find({}).explain('executionStats')",
            Read,
            (vec![c("c")], vec![], false),
        );
        row(
            "db.c.explain().aggregate([{$out: 'dest'}])",
            Write,
            (vec![c("c"), c("dest")], vec![obj(None, "dest")], false),
        );
        row("db.runCommand({ping: 1})", Read, none());
        check(rows);

        assert_eq!(
            parsed("use other").session_effect,
            Some(CommandSessionEffect::UseDatabase("other".into()))
        );
        assert_eq!(parsed("db.c.find()").session_effect, None);
        for (text, name) in [
            ("use x", "use"),
            ("show dbs", "show"),
            ("db.c.updateMany({}, {})", "updateMany"),
            ("db.runCommand({ping: 1})", "runCommand"),
            ("db.c.explain().find()", "find"),
        ] {
            assert_eq!(parsed(text).name, name, "{text}");
        }
    }

    #[test]
    fn run_command_table() {
        let mut rows: Vec<(String, Facts)> = Vec::new();
        let run = |body: &str| format!("db.runCommand({{{body}}})");
        for k in [
            "ping",
            "hello",
            "isMaster",
            "buildInfo",
            "serverStatus",
            "dbStats",
            "collStats",
            "listDatabases",
            "listCollections",
            "listIndexes",
            "find",
            "aggregate",
            "count",
            "distinct",
            "getMore",
            "connectionStatus",
            "hostInfo",
            "getParameter",
            "currentOp",
            "dataSize",
        ] {
            rows.push((
                run(&format!("{k}: 'c', pipeline: []")),
                (Read, vec![c("c")], vec![], false),
            ));
        }
        // The target is the string value of the first key, if it has one.
        rows.push((run("ping: 1"), (Read, vec![], vec![], false)));
        for body in [
            "insert: 'c', documents: [{a: 1}]",
            "update: 'c', updates: [{q: {a: 1}, u: {$set: {b: 1}}, multi: true}]",
            "update: 'c', updates: [{q: {}, u: {$set: {b: 1}}}]",
            "delete: 'c', deletes: [{q: {a: 1}, limit: 0}]",
            "delete: 'c', deletes: [{q: {}, limit: 1}]",
            "findAndModify: 'c', query: {}, remove: true",
        ] {
            rows.push((run(body), (Write, vec![c("c")], vec![], false)));
        }
        rows.push((
            run("aggregate: 'c', pipeline: [{$out: 'd'}], cursor: {}"),
            (Write, vec![c("c"), c("d")], vec![obj(None, "d")], false),
        ));
        rows.push((
            run("aggregate: 'c', pipeline: [{$merge: {into: 'd'}}], cursor: {}"),
            (Write, vec![c("c"), c("d")], vec![], false),
        ));
        for k in ["create", "createIndexes", "dropIndexes", "collMod"] {
            rows.push((
                run(&format!("{k}: 'c'")),
                (Ddl, vec![c("c")], vec![], false),
            ));
        }
        rows.push((
            run("drop: 'c'"),
            (Ddl, vec![c("c")], vec![obj(None, "c")], false),
        ));
        rows.push((
            run("dropDatabase: 1"),
            (Ddl, vec![t(None, "")], vec![database(None)], false),
        ));
        rows.push((
            run("renameCollection: 'app.a', to: 'app.b'"),
            (Ddl, vec![c("app.a")], vec![], false),
        ));
        rows.push((
            run("renameCollection: 'app.a', to: 'app.b', dropTarget: true"),
            (Ddl, vec![c("app.a")], vec![obj(None, "app.b")], false),
        ));
        for k in [
            "validate",
            "killOp",
            "killCursors",
            "setParameter",
            "createUser",
            "updateUser",
            "dropUser",
            "grantRolesToUser",
            "revokeRolesFromUser",
            "createRole",
            "dropRole",
            "shutdown",
            "fsync",
            "compact",
            "replSetStepDown",
            "replSetReconfig",
        ] {
            rows.push((
                run(&format!("{k}: 'c'")),
                (Admin, vec![c("c")], vec![], false),
            ));
        }
        // Explain: the explained command's class, targets, drops and unbounded.
        rows.push((
            run("explain: {find: 'c', filter: {}}, verbosity: 'executionStats'"),
            (Read, vec![c("c")], vec![], false),
        ));
        rows.push((
            run("explain: {delete: 'c', deletes: [{q: {}, limit: 0}]}, verbosity: 'queryPlanner'"),
            (Write, vec![c("c")], vec![], true),
        ));
        rows.push((
            run("explain: {drop: 'c'}, verbosity: 'queryPlanner'"),
            (Ddl, vec![c("c")], vec![obj(None, "c")], false),
        ));
        // Every other key, an empty document, another case, a pipeline that cannot be read.
        for body in [
            "foo: 'c'",
            "eval: 'c'",
            "PING: 'c'",
            "aggregate: 'c', pipeline: {}",
            "explain: 'c'",
        ] {
            rows.push((run(body), (Unknown, vec![c("c")], vec![], false)));
        }
        rows.push((run(""), (Unknown, vec![], vec![], false)));
        check(rows);

        // The db of the command names the targets.
        assert_eq!(
            facts("db.getSiblingDB('x').runCommand({find: 'c'})").1,
            [t(Some("x"), "c")]
        );
        assert_eq!(
            facts("db.adminCommand({collStats: 'c'})").1,
            [t(Some("admin"), "c")]
        );
        assert_eq!(
            facts("db.adminCommand({listDatabases: 1})"),
            (Read, vec![], vec![], false)
        );
    }

    #[test]
    fn explain_verbosity_refused_for_writes() {
        let only = |m: &str| format!(r#"Only "queryPlanner" explain is allowed for {m}()"#);
        let cases = [
            ("db.c.explain('executionStats').deleteMany({})", only("deleteMany"), 13),
            (
                "db.c.aggregate([{$out: 'd'}]).explain('allPlansExecution')",
                only("aggregate"),
                38,
            ),
            (
                "db.c.explain('allPlansExecution').updateOne({a: 1}, {$set: {b: 1}})",
                only("updateOne"),
                13,
            ),
            ("db.c.explain('executionStats').drop()", only("drop"), 13),
            (
                "db.runCommand({explain: {delete: 'c', deletes: [{q: {a: 1}, limit: 1}]}, verbosity: 'executionStats'})",
                only("delete"),
                14,
            ),
            // The server's default verbosity is not queryPlanner.
            (
                "db.runCommand({explain: {update: 'c', updates: []}})",
                only("update"),
                14,
            ),
            (
                "db.runCommand({explain: {foo: 'c'}, verbosity: 'allPlansExecution'})",
                only("foo"),
                14,
            ),
        ];
        for (text, message, offset) in cases {
            assert_eq!(err(text), (message, Some(offset)), "{text}");
        }
        for text in [
            "db.c.explain().deleteMany({})",
            "db.c.explain('queryPlanner').updateMany({}, {$set: {a: 1}})",
            "db.c.find().explain('executionStats')",
            "db.c.aggregate([]).explain('allPlansExecution')",
            "db.runCommand({explain: {find: 'c'}})",
            "db.runCommand({explain: {delete: 'c', deletes: []}, verbosity: 'queryPlanner'})",
        ] {
            parsed(text);
        }
    }

    #[test]
    fn out_in_nested_pipeline_is_write() {
        for (p, kind) in [
            (
                "[{$facet: {a: [{$match: {}}], b: [{$out: 'd'}]}}]",
                OutKind::Out,
            ),
            (
                "[{$lookup: {from: 'x', as: 'y', pipeline: [{$merge: {into: 'd'}}]}}]",
                OutKind::Merge,
            ),
            (
                "[{$unionWith: {coll: 'x', pipeline: [{$out: 'd'}]}}]",
                OutKind::Out,
            ),
            (
                "[{$match: {}}, {$facet: {a: [{$lookup: {from: 'x', as: 'y', pipeline: \
                 [{$unionWith: {coll: 'z', pipeline: [{$merge: 'd'}]}}]}}]}}]",
                OutKind::Merge,
            ),
        ] {
            assert_eq!(has_out_or_merge(&pipeline(p)), Some((kind, c("d"))), "{p}");
            let text = format!("db.c.aggregate({p})");
            assert_eq!(parsed(&text).class, Write, "{text}");
            let text = format!("db.runCommand({{aggregate: 'c', pipeline: {p}, cursor: {{}}}})");
            assert_eq!(parsed(&text).class, Write, "{text}");
            assert!(parsed(&format!("db.c.aggregate({p}).explain()")).class == Write);
        }
        for p in [
            "[]",
            "[{$facet: {a: [{$match: {}}]}}]",
            "[{$lookup: {from: 'x', localField: 'a', foreignField: 'b', as: 'y'}}]",
            "[{$unionWith: 'x'}]",
            "[{$lookup: {from: 'x', as: 'y', pipeline: [{$match: {}}]}}]",
        ] {
            assert_eq!(has_out_or_merge(&pipeline(p)), None, "{p}");
            assert_eq!(parsed(&format!("db.c.aggregate({p})")).class, Read, "{p}");
        }
        // A stage of the pipeline itself wins over a nested one.
        assert_eq!(
            has_out_or_merge(&pipeline("[{$facet: {a: [{$out: 'n'}]}}, {$out: 'top'}]")),
            Some((OutKind::Out, c("top")))
        );
        // A destination that cannot be read is still a write.
        assert_eq!(
            has_out_or_merge(&pipeline("[{$merge: {on: '_id'}}]")),
            Some((OutKind::Merge, c("")))
        );
    }

    #[test]
    fn out_drops_destination() {
        assert_eq!(
            parsed("db.c.aggregate([{$match: {}}, {$out: 'archive'}])").drops,
            [obj(None, "archive")]
        );
        assert!(parsed("db.c.aggregate([{$merge: 'archive'}])")
            .drops
            .is_empty());
        let p = parsed("db.getSiblingDB('x').c.aggregate([{$out: 'archive'}])");
        assert_eq!(p.targets, [t(Some("x"), "c"), t(Some("x"), "archive")]);
        assert_eq!(p.drops, [obj(Some("x"), "archive")]);
        assert_eq!(
            parsed("db.c.aggregate([{$out: {db: 'y', coll: 'z'}}])").drops,
            [obj(Some("y"), "z")]
        );
        let p = parsed("db.c.aggregate([{$out: 5}])");
        assert_eq!((p.class, p.drops), (Write, vec![obj(None, "")]));
        // $out to the collection itself: one target.
        assert_eq!(parsed("db.c.aggregate([{$out: 'c'}])").targets, [c("c")]);
    }

    #[test]
    fn rename_drop_target_drops() {
        assert_eq!(
            parsed("db.c.renameCollection('d', true)").drops,
            [obj(None, "d")]
        );
        assert!(parsed("db.c.renameCollection('d')").drops.is_empty());
        let p =
            parsed("db.adminCommand({renameCollection: 'app.c', to: 'app.d', dropTarget: true})");
        assert_eq!(
            (p.class, p.targets, p.drops),
            (
                Ddl,
                vec![t(Some("admin"), "app.c")],
                vec![obj(Some("admin"), "app.d")]
            )
        );
        assert!(
            parsed("db.adminCommand({renameCollection: 'a.c', to: 'a.d', dropTarget: false})")
                .drops
                .is_empty()
        );
        // Any true-ish value drops, as the server reads it.
        assert_eq!(
            parsed("db.adminCommand({renameCollection: 'a.c', to: 'a.d', dropTarget: 1})")
                .drops
                .len(),
            1
        );
    }

    #[test]
    fn admin_command_drop_names_admin() {
        for text in [
            "db.adminCommand({dropDatabase: 1})",
            "db.getSiblingDB('x').adminCommand({dropDatabase: 1})",
        ] {
            assert_eq!(
                facts(text),
                (
                    Ddl,
                    vec![t(Some("admin"), "admin")],
                    vec![database(Some("admin"))],
                    false
                ),
                "{text}"
            );
        }
    }

    #[test]
    fn sibling_db_drop_names_it() {
        for text in [
            "db.getSiblingDB('x').dropDatabase()",
            "db.getSiblingDB('x').runCommand({dropDatabase: 1})",
        ] {
            assert_eq!(parsed(text).drops, [database(Some("x"))], "{text}");
        }
        let p = parsed("db.getSiblingDB('x').c.drop()");
        assert_eq!(
            (p.targets, p.drops),
            (vec![t(Some("x"), "c")], vec![obj(Some("x"), "c")])
        );
        // Without getSiblingDB: the database in effect when it runs (an empty name).
        assert_eq!(parsed("db.dropDatabase()").drops, [database(None)]);
    }

    #[test]
    fn unbounded_update_many_and_run_command_delete() {
        let unbounded = |text: &str| parsed(text).unbounded;
        for text in [
            "db.c.updateMany({}, {$set: {a: 1}})",
            "db.c.updateMany({}, [{$set: {a: 1}}])",
            "db.c.deleteMany({})",
            "db.runCommand({delete: 'c', deletes: [{q: {}, limit: 0}]})",
            "db.runCommand({delete: 'c', deletes: [{q: {a: 1}, limit: 0}, {limit: 0}]})",
            "db.runCommand({update: 'c', updates: [{q: {}, u: {$set: {a: 1}}, multi: true}]})",
            "db.runCommand({update: 'c', updates: [{u: {$set: {a: 1}}, multi: true}]})",
            // Fail closed: what cannot be read counts as unbounded.
            "db.runCommand({delete: 'c'})",
            "db.runCommand({delete: 'c', deletes: [{q: {}}]})",
            "db.runCommand({delete: 'c', deletes: [1]})",
            "db.runCommand({update: 'c', updates: {}})",
            "db.runCommand({update: 'c', updates: [{q: 'x', u: {}, multi: 1}]})",
        ] {
            assert!(unbounded(text), "{text}");
        }
        for text in [
            "db.c.updateMany({a: 1}, {$set: {a: 1}})",
            "db.c.updateOne({}, {$set: {a: 1}})",
            "db.c.deleteMany({a: 1})",
            "db.c.deleteOne({})",
            "db.runCommand({delete: 'c', deletes: [{q: {a: 1}, limit: 0}]})",
            "db.runCommand({delete: 'c', deletes: [{q: {}, limit: 1}]})",
            "db.runCommand({update: 'c', updates: [{q: {a: 1}, u: {}, multi: true}]})",
            "db.runCommand({update: 'c', updates: [{q: {}, u: {}, multi: false}]})",
            "db.runCommand({update: 'c', updates: [{q: {}, u: {}}]})",
        ] {
            assert!(!unbounded(text), "{text}");
        }
    }

    #[test]
    fn validate_is_admin() {
        for text in [
            "db.runCommand({validate: 'c'})",
            "db.runCommand({validate: 'c', full: true})",
        ] {
            assert_eq!(facts(text), (Admin, vec![c("c")], vec![], false), "{text}");
        }
    }
}
