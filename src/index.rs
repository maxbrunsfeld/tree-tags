use crate::language_registry::LanguageRegistry;
use ignore::{WalkBuilder, WalkState};
use rusqlite::{self, Connection, Transaction};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use tree_sitter::{Parser, Point, PropertySheet, Tree, TreePropertyCursor};

#[derive(Debug)]
pub enum Error {
    IO(io::Error),
    Ignore(ignore::Error),
    SQL(rusqlite::Error),
}

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Clone)]
pub struct Index {
    db_path: PathBuf,
    language_registry: Arc<Mutex<LanguageRegistry>>,
}

struct Definition<'a> {
    name: Option<(&'a str, Point)>,
    kind: Option<&'a str>,
    start_position: Point,
    end_position: Point,
}

struct Module<'a> {
    name: Option<&'a str>,
    definitions: Vec<Definition<'a>>,
    pending_definition_stack: Vec<Definition<'a>>,
}

struct Scope<'a> {
    kind: Option<&'a str>,
    local_refs: Vec<(&'a str, Point)>,
    local_defs: Vec<(&'a str, Point)>,
    hoisted_local_defs: HashMap<&'a str, Point>,
}

struct Walker<'a> {
    scope_stack: Vec<Scope<'a>>,
    module_stack: Vec<Module<'a>>,
    db: Transaction<'a>,
    property_matcher: TreePropertyCursor<'a>,
    source_code: &'a str,
    file_id: i64,
}

impl<'a> Walker<'a> {
    fn new(
        db: Transaction<'a>,
        file_id: i64,
        tree: &'a Tree,
        property_sheet: &'a PropertySheet,
        source_code: &'a str,
    ) -> Self {
        Self {
            db,
            source_code,
            property_matcher: tree.walk_with_properties(property_sheet),
            scope_stack: Vec::new(),
            module_stack: Vec::new(),
            file_id,
        }
    }

    fn index_tree(&mut self) -> Result<()> {
        self.push_scope(None);
        self.push_module();
        let mut visited_node = false;
        loop {
            if visited_node {
                if self.property_matcher.goto_next_sibling() {
                    self.enter_node()?;
                    visited_node = false;
                } else if self.property_matcher.goto_parent() {
                    self.leave_node()?;
                } else {
                    break;
                }
            } else if self.property_matcher.goto_first_child() {
                self.enter_node()?;
            } else {
                visited_node = true;
            }
        }
        self.pop_module()?;
        self.pop_scope()?;
        Ok(())
    }

    fn enter_node(&mut self) -> Result<()> {
        let node = self.property_matcher.node();
        let mut is_local_def = false;

        if self.has_property("local-definition") {
            is_local_def = true;
            let scope_type = self.get_property("scope-type");
            let is_hoisted = self.has_property("local-is-hoisted");
            if let Some(text) = node.utf8_text(self.source_code).ok() {
                if is_hoisted {
                    self.top_scope(scope_type)
                        .hoisted_local_defs
                        .insert(text, node.start_position());
                } else {
                    self.top_scope(scope_type)
                        .local_defs
                        .push((text, node.start_position()));
                }
            }
        }

        if self.has_property("local-reference") && !is_local_def {
            if let Some(text) = node.utf8_text(self.source_code).ok() {
                self.top_scope(self.get_property("scope-type"))
                    .local_refs
                    .push((text, node.start_position()));
            }
        }

        if self.has_property("local-scope") {
            self.push_scope(self.get_property("scope-type"));
        }

        if self.has_property("module") {
            self.push_module();
        }

        match self.get_property("module-part") {
            Some("name") => {
                if let Some(text) = node.utf8_text(self.source_code).ok() {
                    let module = self.module_stack.last_mut().unwrap();
                    module.name = Some(text);
                }
            }
            _ => {}
        }

        if self.has_property("definition") {
            let kind = self.get_property("definition-type");
            self.top_module().pending_definition_stack.push(Definition {
                name: None,
                kind,
                start_position: node.start_position(),
                end_position: node.end_position(),
            });
        }

        match self.get_property("definition-part") {
            Some("name") => {
                if let Some(text) = node.utf8_text(self.source_code).ok() {
                    self.top_definition().unwrap().name = Some((text, node.start_position()));
                }
            }
            Some("value") => {
                let kind = self.get_property("definition-type");
                if kind.is_some() {
                    self.top_definition().unwrap().kind = kind;
                }
            }
            _ => {}
        }

        if self.has_property("reference") {
            if let Some(text) = node.utf8_text(self.source_code).ok() {
                self.insert_ref(
                    text,
                    node.start_position(),
                    self.get_property("reference-type"),
                )?;
            }
        }

        Ok(())
    }

    fn leave_node(&mut self) -> Result<()> {
        if self.has_property("local-scope") {
            self.pop_scope()?;
        }

        if self.has_property("definition") {
            self.pop_definition()?;
        }

        if self.has_property("module") {
            self.pop_module()?;
        }

        Ok(())
    }

    fn top_scope(&mut self, kind: Option<&'a str>) -> &mut Scope<'a> {
        self.scope_stack
            .iter_mut()
            .enumerate()
            .rev()
            .find_map(|(i, scope)| {
                if i == 0 || kind.map_or(true, |kind| Some(kind) == scope.kind) {
                    Some(scope)
                } else {
                    None
                }
            }).unwrap()
    }

    fn top_module(&mut self) -> &mut Module<'a> {
        self.module_stack.last_mut().unwrap()
    }

    fn top_definition(&mut self) -> Option<&mut Definition<'a>> {
        self.top_module().pending_definition_stack.last_mut()
    }

    fn push_scope(&mut self, kind: Option<&'a str>) {
        self.scope_stack.push(Scope {
            kind,
            local_refs: Vec::new(),
            local_defs: Vec::new(),
            hoisted_local_defs: HashMap::new(),
        });
    }

    fn pop_scope(&mut self) -> Result<()> {
        let mut scope = self.scope_stack.pop().unwrap();

        let mut local_def_ids = Vec::with_capacity(scope.local_defs.len());
        for (name, position) in scope.local_defs.iter() {
            local_def_ids.push(self.insert_local_def(name, *position)?);
        }

        let mut hoisted_local_def_ids = HashMap::new();
        for (name, position) in scope.hoisted_local_defs.iter() {
            hoisted_local_def_ids.insert(name, self.insert_local_def(name, *position)?);
        }

        let mut parent_scope = self.scope_stack.pop();
        for local_ref in scope.local_refs.drain(..) {
            let mut local_def_id = None;

            for (i, local_def) in scope.local_defs.iter().enumerate() {
                if local_def.1 > local_ref.1 {
                    break;
                }
                if local_def.0 == local_ref.0 {
                    local_def_id = Some(local_def_ids[i]);
                }
            }

            if local_def_id.is_none() {
                local_def_id = hoisted_local_def_ids.get(&local_ref.0).cloned();
            }

            if let Some(local_def_id) = local_def_id {
                self.insert_local_ref(local_def_id, local_ref.0, local_ref.1)?;
            } else if let Some(parent_scope) = parent_scope.as_mut() {
                parent_scope.local_refs.push(local_ref);
            }
        }
        parent_scope.map(|scope| self.scope_stack.push(scope));

        Ok(())
    }

    fn push_module(&mut self) {
        self.module_stack.push(Module {
            name: None,
            definitions: Vec::new(),
            pending_definition_stack: Vec::new(),
        });
    }

    fn pop_module(&mut self) -> Result<()> {
        let mod_path = self
            .module_stack
            .iter()
            .filter_map(|m| m.name)
            .collect::<Vec<_>>();
        let module = self.module_stack.pop().unwrap();
        for definition in module.definitions {
            if let Some((name, name_position)) = definition.name {
                self.insert_def(
                    name,
                    name_position,
                    definition.start_position,
                    definition.end_position,
                    definition.kind,
                    &mod_path,
                )?;
            }
        }
        Ok(())
    }

    fn pop_definition(&mut self) -> Result<()> {
        let module = self.module_stack.last_mut().unwrap();
        let definition = module.pending_definition_stack.pop().unwrap();
        module.definitions.push(definition);
        Ok(())
    }

    fn get_property(&self, prop: &'static str) -> Option<&'a str> {
        self.property_matcher
            .node_properties()
            .get(prop)
            .map(|v| v.as_str())
    }

    fn has_property(&self, prop: &'static str) -> bool {
        self.get_property(prop).is_some()
    }

    fn insert_local_ref(
        &mut self,
        local_def_id: i64,
        name: &'a str,
        position: Point,
    ) -> Result<()> {
        self.db.execute(
            "
                INSERT INTO local_refs
                (file_id, definition_id, row, column, length)
                VALUES
                (?1, ?2, ?3, ?4, ?5)
            ",
            &[
                &self.file_id,
                &local_def_id,
                &position.row,
                &position.column,
                &(name.as_bytes().len() as i64),
            ],
        )?;
        Ok(())
    }

    fn insert_local_def(&mut self, name: &'a str, position: Point) -> Result<i64> {
        self.db.execute(
            "
                INSERT INTO local_defs
                (file_id, row, column, length)
                VALUES
                (?1, ?2, ?3, ?4)
            ",
            &[
                &self.file_id,
                &position.row,
                &position.column,
                &(name.as_bytes().len() as i64),
            ],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    fn insert_ref(&mut self, name: &'a str, position: Point, kind: Option<&'a str>) -> Result<()> {
        self.db.execute(
            "
                INSERT INTO refs
                (file_id, name, row, column, kind)
                VALUES
                (?1, ?2, ?3, ?4, ?5)
            ",
            &[&self.file_id, &name, &position.row, &position.column, &kind],
        )?;
        Ok(())
    }

    fn insert_def(
        &mut self,
        name: &'a str,
        name_position: Point,
        start_position: Point,
        end_position: Point,
        kind: Option<&'a str>,
        module_path: &Vec<&'a str>,
    ) -> Result<()> {
        let mut module_path_string = String::with_capacity(
            module_path
                .iter()
                .map(|entry| entry.as_bytes().len() + 1)
                .sum(),
        );
        for entry in module_path {
            module_path_string += entry;
            module_path_string += "\t";
        }
        self.db.execute(
            "
                INSERT INTO defs
                (
                    file_id,
                    start_row, start_column,
                    end_row, end_column,
                    name, name_start_row, name_start_column,
                    kind,
                    module_path
                )
                VALUES
                (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ",
            &[
                &self.file_id,
                &start_position.row,
                &start_position.column,
                &end_position.row,
                &end_position.column,
                &name,
                &name_position.row,
                &name_position.column,
                &kind,
                &module_path_string,
            ],
        )?;
        Ok(())
    }
}

impl Index {
    pub fn new(config_dir: PathBuf) -> Self {
        Index {
            db_path: config_dir.join("db.sqlite"),
            language_registry: Arc::new(Mutex::new(LanguageRegistry::new(
                config_dir,
                vec!["/Users/max/github".into()],
            ))),
        }
    }

    pub fn index_path(&mut self, path: PathBuf) -> Result<()> {
        self.language_registry.lock().unwrap().load_parsers()?;
        let last_error = Arc::new(Mutex::new(Ok(())));
        let db = Connection::open(&self.db_path)?;
        db.execute_batch(include_str!("./schema.sql"))
            .expect("Failed to ensure schema is set up");

        WalkBuilder::new(path).threads(1).build_parallel().run(|| {
            let worker = self.clone();
            let last_error = last_error.clone();
            let mut parser = Parser::new();
            match Connection::open(&self.db_path) {
                Ok(mut db) => Box::new({
                    move |entry| {
                        match entry {
                            Ok(entry) => {
                                if let Some(t) = entry.file_type() {
                                    if t.is_file() {
                                        if let Err(e) =
                                            worker.index_file(&mut db, &mut parser, entry.path())
                                        {
                                            *last_error.lock().unwrap() = Err(e);
                                            return WalkState::Quit;
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                *last_error.lock().unwrap() = Err(e.into());
                            }
                        }
                        WalkState::Continue
                    }
                }),
                Err(error) => {
                    *last_error.lock().unwrap() = Err(error.into());
                    Box::new(|_| WalkState::Quit)
                }
            }
        });

        Arc::try_unwrap(last_error).unwrap().into_inner().unwrap()
    }

    pub fn find_definition(
        &mut self,
        path: PathBuf,
        position: Point,
    ) -> Result<Vec<(PathBuf, Point, usize)>> {
        let db = Connection::open(&self.db_path)?;
        let file_id: i64 = db.query_row(
            "SELECT id FROM files WHERE path = ?1",
            &[&path.as_os_str().as_bytes()],
            |row| row.get(0),
        )?;

        let local_result = db.query_row(
            "
                SELECT
                    local_defs.row,
                    local_defs.column,
                    local_defs.length
                FROM
                    local_refs,
                    local_defs
                WHERE
                    local_refs.definition_id = local_defs.id AND
                    local_refs.file_id = ?1 AND
                    local_refs.row = ?2 AND
                    local_refs.column <= ?3 AND
                    local_refs.column + local_refs.length > ?3
            ",
            &[&file_id, &(position.row as i64), &(position.column as i64)],
            |row| {
                (
                    Point {
                        row: row.get(0),
                        column: row.get(1),
                    },
                    row.get::<usize, i64>(2),
                )
            },
        );

        match local_result {
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Ok((position, length)) => return Ok(vec![(path, position, length as usize)]),
            Err(e) => return Err(e.into()),
        }

        let mut statement = db.prepare(
            "
                SELECT
                    files.path,
                    defs.name_start_row,
                    defs.name_start_column,
                    length(defs.name)
                FROM
                    files,
                    defs,
                    refs
                WHERE
                    files.id == defs.file_id AND
                    defs.name = refs.name AND
                    refs.file_id = ?1 AND
                    refs.row = ?2 AND
                    refs.column <= ?3 AND
                    refs.column + length(refs.name) > ?3
                LIMIT
                    50
            ",
        )?;

        let rows = statement.query_map(
            &[&file_id, &(position.row as i64), &(position.column as i64)],
            |row| {
                (
                    OsString::from_vec(row.get::<usize, Vec<u8>>(0)).into(),
                    Point::new(row.get(1), row.get(2)),
                    row.get::<usize, i64>(3) as usize,
                )
            },
        )?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }

        Ok(result)
    }

    fn index_file(&self, db: &mut Connection, parser: &mut Parser, path: &Path) -> Result<()> {
        let mut file = File::open(path)?;
        if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
            if let Some((language, property_sheet)) = self
                .language_registry
                .lock()
                .unwrap()
                .language_for_file_extension(extension)?
            {
                parser
                    .set_language(language)
                    .expect("Incompatible language version");
                let mut source_code = String::new();
                file.read_to_string(&mut source_code)?;
                let tree = parser
                    .parse_str(&source_code, None)
                    .expect("Parsing failed");
                let tx = db.transaction()?;
                tx.execute(
                    "DELETE FROM files WHERE path = ?1",
                    &[&path.as_os_str().as_bytes()],
                )?;
                tx.execute(
                    "INSERT INTO files (path) VALUES (?1)",
                    &[&path.as_os_str().as_bytes()],
                )?;
                let file_id = tx.last_insert_rowid();
                let mut walker = Walker::new(tx, file_id, &tree, &property_sheet, &source_code);
                walker.index_tree()?;
                walker.db.commit()?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::IO(e) => e.fmt(f),
            Error::SQL(e) => e.fmt(f),
            Error::Ignore(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::IO(e)
    }
}

impl From<ignore::Error> for Error {
    fn from(e: ignore::Error) -> Error {
        Error::Ignore(e)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Error {
        Error::SQL(e)
    }
}
